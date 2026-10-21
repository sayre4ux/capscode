//! The per-path record of what the host and a capsule last agreed on.
//!
//! File sync compares each capsule file against a base. Until a path is written to the host,
//! or found identical on both sides, its base is the ingress commit. From then on its base is
//! the content both sides held at that moment. Without this record, every file one sync
//! round wrote would read as a host edit in the next round.
//!
//! The record is an append-only journal beside `receipt.json`: a header line naming the
//! ingress commit, then one JSON line per event, the last event for a path winning.
//!
//! Losing an event is not harmless. If the record forgot that a sync wrote a newer version of
//! a file, a capsule that changed the file back would match the older base and its change
//! would be dropped. So a write is journaled as an intent, flushed, before the host file is
//! replaced, then confirmed, or cancelled if the write failed. An intent left open by a crash
//! counts as landed only if the host holds exactly the intended file; anything else makes the
//! path disputed, which reports a conflict until the host and the capsule match again. A
//! journal that cannot be read is an error, not an empty record.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use git2::Oid;
use serde::{Deserialize, Serialize};

use crate::{path_from_git_bytes, shadow_egress_error, validate_relative_path, GitError};

pub(crate) const AGREED_FILE: &str = "agreed.jsonl";
const VERSION: u32 = 1;
/// Rewrite the journal once superseded lines outnumber live ones by this margin.
const COMPACT_SLACK: usize = 64;

/// What the record knows about one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agreed {
    /// Both sides held a regular file with this content. The two executable bits can differ:
    /// a sync keeps an existing host file's mode, so the host side records the bit actually
    /// written and the capsule side the bit the capsule had.
    File {
        blob: Oid,
        host_executable: bool,
        capsule_executable: bool,
    },
    /// Neither side had a file here.
    Absent,
    /// A write was interrupted and the host holds neither the old nor the new file, or holds
    /// the old one and may have been changed back to it. Nothing is known to be agreed, so
    /// every difference at this path is a conflict.
    Disputed,
}

/// A host write that was journaled before it happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Intent {
    /// What the path agrees on once the write lands. Always a file.
    pub(crate) agreed: Agreed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    agreed: u32,
    ingress: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLine {
    blob: String,
    host_x: bool,
    capsule_x: bool,
}

/// File sync refuses paths that are not valid UTF-8 before they reach the journal.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Line {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<FileLine>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    absent: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    disputed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    writing: Option<FileLine>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cancelled: bool,
}

enum Event {
    Agreed(Agreed),
    Writing(Intent),
    Cancelled,
}

fn file_agreement(line: &FileLine) -> Option<Agreed> {
    Some(Agreed::File {
        blob: line.blob.parse().ok()?,
        host_executable: line.host_x,
        capsule_executable: line.capsule_x,
    })
}

fn file_line(agreed: Agreed) -> Option<FileLine> {
    match agreed {
        Agreed::File {
            blob,
            host_executable,
            capsule_executable,
        } => Some(FileLine {
            blob: blob.to_string(),
            host_x: host_executable,
            capsule_x: capsule_executable,
        }),
        Agreed::Absent | Agreed::Disputed => None,
    }
}

fn utf8_path(path: &Path) -> Result<&str, GitError> {
    path.to_str().ok_or_else(|| {
        shadow_egress_error(format!(
            "{} is not valid UTF-8, so the file sync record cannot name it",
            crate::escape_shadow_path(path)
        ))
    })
}

impl Line {
    fn for_path(path: &str) -> Self {
        Line {
            path: path.to_owned(),
            ..Line::default()
        }
    }

    fn path(&self) -> Option<Vec<u8>> {
        validate_relative_path(Path::new(&self.path)).ok()?;
        Some(self.path.clone().into_bytes())
    }

    fn event(&self) -> Option<Event> {
        let kinds = [
            self.file.is_some(),
            self.absent,
            self.disputed,
            self.writing.is_some(),
            self.cancelled,
        ];
        if kinds.iter().filter(|kind| **kind).count() != 1 {
            return None;
        }
        if let Some(file) = &self.file {
            return file_agreement(file).map(Event::Agreed);
        }
        if let Some(writing) = &self.writing {
            return Some(Event::Writing(Intent {
                agreed: file_agreement(writing)?,
            }));
        }
        Some(if self.absent {
            Event::Agreed(Agreed::Absent)
        } else if self.disputed {
            Event::Agreed(Agreed::Disputed)
        } else {
            Event::Cancelled
        })
    }
}

/// The journal as one round sees it, every intent settled.
#[derive(Debug, Default)]
pub(crate) struct Journal {
    pub(crate) entries: BTreeMap<Vec<u8>, Agreed>,
}

#[derive(Default)]
struct Parsed {
    entries: BTreeMap<Vec<u8>, Agreed>,
    pending: BTreeMap<Vec<u8>, Intent>,
    lines: usize,
    needs_rewrite: bool,
}

fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join(AGREED_FILE)
}

fn header_line(ingress: Oid) -> String {
    serde_json::to_string(&Header {
        agreed: VERSION,
        ingress: ingress.to_string(),
    })
    .expect("the journal header serializes")
}

fn parse(bytes: &[u8], ingress: Oid) -> Result<Parsed, String> {
    if !bytes.contains(&b'\n') {
        // The first append was cut short before its header ended, so nothing it carried was
        // ever flushed and no host write followed it. Anything else without a newline is
        // damage, not a cut-short header.
        if !header_line(ingress).as_bytes().starts_with(bytes) {
            return Err("it has no complete line".into());
        }
        return Ok(Parsed {
            needs_rewrite: true,
            ..Parsed::default()
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "it is not valid UTF-8".to_string())?;
    let mut parsed = Parsed {
        needs_rewrite: !text.ends_with('\n'),
        ..Parsed::default()
    };
    let mut pieces = text.split('\n');
    let header = pieces
        .next()
        .and_then(|line| serde_json::from_str::<Header>(line).ok())
        .ok_or_else(|| "its header is unreadable".to_string())?;
    if header.agreed != VERSION {
        return Err(format!(
            "it has format version {}, and this CapsCode reads version {VERSION}",
            header.agreed
        ));
    }
    if header.ingress != ingress.to_string() {
        return Err("it belongs to a different ingress baseline".into());
    }
    let pieces: Vec<&str> = pieces.collect();
    for (index, piece) in pieces.iter().enumerate() {
        let last = index + 1 == pieces.len();
        if piece.is_empty() && last {
            continue;
        }
        let event = serde_json::from_str::<Line>(piece)
            .ok()
            .and_then(|line| Some((line.path()?, line.event()?)));
        match event {
            Some((path, Event::Agreed(agreed))) => {
                parsed.pending.remove(&path);
                parsed.entries.insert(path, agreed);
            }
            Some((path, Event::Writing(intent))) => {
                parsed.pending.insert(path, intent);
            }
            Some((path, Event::Cancelled)) => {
                parsed.pending.remove(&path);
            }
            // A final line with no newline is an append a crash cut short. Its flush never
            // finished, so a write it announced never started.
            None if last => {}
            None => return Err(format!("line {} is unreadable", index + 2)),
        }
        parsed.lines += 1;
    }
    Ok(parsed)
}

/// Read the journal for a round. Each write left open by a crash is settled with `landed`,
/// which says whether the host now holds exactly the intended file: if so the write counts,
/// and otherwise the path is disputed. Call this only while holding the box's workspace lock,
/// before any record call in the same round.
pub(crate) fn open_for_round(
    state_dir: &Path,
    ingress: Oid,
    mut landed: impl FnMut(&Path, &Intent) -> Result<bool, GitError>,
) -> Result<Journal, GitError> {
    let path = journal_path(state_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Journal::default()),
        Err(error) => {
            return Err(shadow_egress_error(format!(
                "reading the file sync record {}: {error}",
                path.display()
            )))
        }
    };
    let mut parsed = parse(&bytes, ingress).map_err(|reason| {
        shadow_egress_error(format!(
            "the file sync record {} cannot be used because {reason}. Move it aside to sync again; files earlier syncs wrote will then be compared with the session's starting snapshot, and a file the box changed back to that snapshot will not be offered",
            path.display()
        ))
    })?;
    let pending = std::mem::take(&mut parsed.pending);
    for (relative, intent) in &pending {
        let settled = if landed(&path_from_git_bytes(relative.clone()), intent)? {
            intent.agreed
        } else {
            Agreed::Disputed
        };
        parsed.entries.insert(relative.clone(), settled);
    }
    if parsed.needs_rewrite
        || !pending.is_empty()
        || parsed.lines > parsed.entries.len() * 2 + COMPACT_SLACK
    {
        rewrite(state_dir, ingress, &parsed.entries)?;
    }
    Ok(Journal {
        entries: parsed.entries,
    })
}

fn agreement_line(path: &str, agreed: Agreed) -> String {
    let line = Line {
        absent: agreed == Agreed::Absent,
        disputed: agreed == Agreed::Disputed,
        file: file_line(agreed),
        ..Line::for_path(path)
    };
    serde_json::to_string(&line).expect("a journal line serializes")
}

fn rewrite(
    state_dir: &Path,
    ingress: Oid,
    entries: &BTreeMap<Vec<u8>, Agreed>,
) -> Result<(), GitError> {
    let mut text = header_line(ingress);
    text.push('\n');
    for (path, agreed) in entries {
        let path = std::str::from_utf8(path).expect("journal paths are parsed from a str");
        text.push_str(&agreement_line(path, *agreed));
        text.push('\n');
    }
    let mut temporary = tempfile::NamedTempFile::new_in(state_dir)
        .map_err(|error| shadow_egress_error(format!("creating the file sync record: {error}")))?;
    temporary
        .write_all(text.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| shadow_egress_error(format!("writing the file sync record: {error}")))?;
    temporary
        .persist(journal_path(state_dir))
        .map_err(|error| {
            shadow_egress_error(format!("replacing the file sync record: {}", error.error))
        })?;
    sync_directory(state_dir)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), GitError> {
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| shadow_egress_error(format!("syncing the file sync record: {error}")))
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), GitError> {
    Ok(())
}

fn append(state_dir: &Path, ingress: Oid, lines: &str, durable: bool) -> Result<(), GitError> {
    let path = journal_path(state_dir);
    let journal_error = |action: &str, error: std::io::Error| {
        shadow_egress_error(format!(
            "{action} the file sync record {}: {error}",
            path.display()
        ))
    };
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .map_err(|error| journal_error("opening", error))?;
    let mut length = file
        .metadata()
        .map_err(|error| journal_error("inspecting", error))?
        .len();
    if length > 0 {
        // An earlier append in this round may have stopped partway. Its fragment was never
        // a whole line, so a reader would drop it; drop it here instead of extending it.
        let mut last = [0u8];
        file.seek(SeekFrom::End(-1))
            .and_then(|_| file.read_exact(&mut last))
            .map_err(|error| journal_error("reading", error))?;
        if last[0] != b'\n' {
            let mut content = Vec::new();
            file.seek(SeekFrom::Start(0))
                .and_then(|_| file.read_to_end(&mut content))
                .map_err(|error| journal_error("reading", error))?;
            length = content
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index as u64 + 1);
            file.set_len(length)
                .map_err(|error| journal_error("trimming", error))?;
        }
    }
    let mut text = String::new();
    if length == 0 {
        text.push_str(&header_line(ingress));
        text.push('\n');
    }
    text.push_str(lines);
    file.write_all(text.as_bytes())
        .map_err(|error| journal_error("writing", error))?;
    if durable {
        file.sync_all()
            .map_err(|error| journal_error("syncing", error))?;
        if length == 0 {
            sync_directory(state_dir)?;
        }
    }
    Ok(())
}

/// Record agreements observed in a round and flush them.
pub(crate) fn record(
    state_dir: &Path,
    ingress: Oid,
    agreements: &[(PathBuf, Agreed)],
) -> Result<(), GitError> {
    let mut lines = String::new();
    for (path, agreed) in agreements {
        lines.push_str(&agreement_line(utf8_path(path)?, *agreed));
        lines.push('\n');
    }
    if lines.is_empty() {
        return Ok(());
    }
    append(state_dir, ingress, &lines, true)
}

/// Journal a host write before it happens, flushed. The caller must not write the host file
/// unless this succeeds.
pub(crate) fn record_intent(
    state_dir: &Path,
    ingress: Oid,
    path: &Path,
    intent: Intent,
) -> Result<(), GitError> {
    let line = Line {
        writing: file_line(intent.agreed),
        ..Line::for_path(utf8_path(path)?)
    };
    let mut text = serde_json::to_string(&line).expect("a journal line serializes");
    text.push('\n');
    append(state_dir, ingress, &text, true)
}

/// Confirm a journaled write. Not flushed: if this line is lost, the next round finds the
/// intended file on the host and settles the intent the same way.
pub(crate) fn record_landed(
    state_dir: &Path,
    ingress: Oid,
    path: &Path,
    agreed: Agreed,
) -> Result<(), GitError> {
    let mut text = agreement_line(utf8_path(path)?, agreed);
    text.push('\n');
    append(state_dir, ingress, &text, false)
}

/// Withdraw a journaled write that failed, so the path keeps its earlier base. If this line
/// is lost, the next round finds the host without the intended file and disputes the path.
pub(crate) fn record_cancelled(
    state_dir: &Path,
    ingress: Oid,
    path: &Path,
) -> Result<(), GitError> {
    let line = Line {
        cancelled: true,
        ..Line::for_path(utf8_path(path)?)
    };
    let mut text = serde_json::to_string(&line).expect("a journal line serializes");
    text.push('\n');
    append(state_dir, ingress, &text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(text: &str) -> Oid {
        Oid::hash_object(git2::ObjectType::Blob, text.as_bytes()).unwrap()
    }

    fn file(text: &str) -> Agreed {
        Agreed::File {
            blob: oid(text),
            host_executable: false,
            capsule_executable: false,
        }
    }

    fn intent(text: &str) -> Intent {
        Intent { agreed: file(text) }
    }

    fn never_called(_: &Path, _: &Intent) -> Result<bool, GitError> {
        panic!("no intent should be pending")
    }

    fn append_raw(dir: &Path, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(journal_path(dir))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn the_last_line_for_a_path_wins_and_a_torn_tail_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let ingress = oid("ingress");
        record(
            dir.path(),
            ingress,
            &[
                (PathBuf::from("a.txt"), file("one")),
                (PathBuf::from("gone.txt"), Agreed::Absent),
            ],
        )
        .unwrap();
        record(
            dir.path(),
            ingress,
            &[(PathBuf::from("a.txt"), file("two"))],
        )
        .unwrap();
        append_raw(dir.path(), br#"{"path":"b.txt","fi"#);

        let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
        assert_eq!(journal.entries.get(b"a.txt".as_slice()), Some(&file("two")));
        assert_eq!(
            journal.entries.get(b"gone.txt".as_slice()),
            Some(&Agreed::Absent)
        );
        assert_eq!(journal.entries.len(), 2);

        // The repair means the next append starts on a line of its own.
        record(
            dir.path(),
            ingress,
            &[(PathBuf::from("c.txt"), file("three"))],
        )
        .unwrap();
        let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
        assert_eq!(
            journal.entries.get(b"c.txt".as_slice()),
            Some(&file("three"))
        );
    }

    #[test]
    fn an_append_never_extends_a_line_cut_short_in_the_same_round() {
        let dir = tempfile::tempdir().unwrap();
        let ingress = oid("ingress");
        record(
            dir.path(),
            ingress,
            &[(PathBuf::from("a.txt"), file("one"))],
        )
        .unwrap();
        append_raw(dir.path(), br#"{"path":"a.txt","fi"#);
        record(
            dir.path(),
            ingress,
            &[(PathBuf::from("b.txt"), file("two"))],
        )
        .unwrap();

        let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
        assert_eq!(journal.entries.get(b"a.txt".as_slice()), Some(&file("one")));
        assert_eq!(journal.entries.get(b"b.txt".as_slice()), Some(&file("two")));
    }

    #[test]
    fn an_empty_or_headless_journal_is_no_journal() {
        let dir = tempfile::tempdir().unwrap();
        let ingress = oid("ingress");
        std::fs::write(journal_path(dir.path()), "garbage-without-newline").unwrap();
        assert!(open_for_round(dir.path(), ingress, never_called)
            .unwrap_err()
            .to_string()
            .contains("no complete line"));
        for text in ["", "{\"agreed\":1,\"ingr"] {
            std::fs::write(journal_path(dir.path()), text).unwrap();
            let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
            assert!(journal.entries.is_empty());
            record(
                dir.path(),
                ingress,
                &[(PathBuf::from("a.txt"), file("one"))],
            )
            .unwrap();
            assert_eq!(
                open_for_round(dir.path(), ingress, never_called)
                    .unwrap()
                    .entries
                    .len(),
                1
            );
        }
    }

    #[test]
    fn an_open_write_counts_only_when_the_host_holds_the_intended_file() {
        let dir = tempfile::tempdir().unwrap();
        let ingress = oid("ingress");
        for path in ["landed.txt", "lost.txt"] {
            record_intent(dir.path(), ingress, Path::new(path), intent("new")).unwrap();
        }
        let mut asked = Vec::new();
        let journal = open_for_round(dir.path(), ingress, |path, _| {
            asked.push(path.to_path_buf());
            Ok(path == Path::new("landed.txt"))
        })
        .unwrap();
        assert_eq!(asked.len(), 2);
        assert_eq!(
            journal.entries.get(b"landed.txt".as_slice()),
            Some(&file("new"))
        );
        assert_eq!(
            journal.entries.get(b"lost.txt".as_slice()),
            Some(&Agreed::Disputed)
        );
        // Settled intents are written back, so they are not asked about again.
        let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
        assert_eq!(
            journal.entries.get(b"lost.txt".as_slice()),
            Some(&Agreed::Disputed)
        );

        // A confirmation or a cancellation closes the intent without asking the host.
        record(
            dir.path(),
            ingress,
            &[(PathBuf::from("kept.txt"), file("old"))],
        )
        .unwrap();
        record_intent(dir.path(), ingress, Path::new("kept.txt"), intent("x")).unwrap();
        record_cancelled(dir.path(), ingress, Path::new("kept.txt")).unwrap();
        record_intent(dir.path(), ingress, Path::new("confirmed.txt"), intent("y")).unwrap();
        record_landed(dir.path(), ingress, Path::new("confirmed.txt"), file("y")).unwrap();
        let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
        assert_eq!(
            journal.entries.get(b"kept.txt".as_slice()),
            Some(&file("old"))
        );
        assert_eq!(
            journal.entries.get(b"confirmed.txt".as_slice()),
            Some(&file("y"))
        );
    }

    #[test]
    fn a_journal_it_cannot_trust_stops_the_round() {
        let dir = tempfile::tempdir().unwrap();
        let ingress = oid("ingress");
        let entry = [(PathBuf::from("a.txt"), file("one"))];
        let error = |text: &str| {
            std::fs::write(journal_path(dir.path()), text).unwrap();
            open_for_round(dir.path(), ingress, never_called)
                .unwrap_err()
                .to_string()
        };

        record(dir.path(), oid("another ingress"), &entry).unwrap();
        let message = open_for_round(dir.path(), ingress, never_called)
            .unwrap_err()
            .to_string();
        assert!(message.contains("different ingress baseline"), "{message}");
        assert!(message.contains("Move it aside"), "{message}");

        assert!(error("not json\n").contains("header is unreadable"));
        let newer = format!("{{\"agreed\":2,\"ingress\":\"{ingress}\"}}\n");
        assert!(error(&newer).contains("format version 2"));
        let escape = format!(
            "{}\n{{\"path\":\"../escape\",\"absent\":true}}\n{{\"path\":\"ok.txt\",\"absent\":true}}\n",
            header_line(ingress)
        );
        assert!(error(&escape).contains("line 2 is unreadable"));
        let mixed = format!(
            "{}\n{{\"path\":\"a\",\"absent\":true,\"disputed\":true}}\n\n",
            header_line(ingress)
        );
        assert!(error(&mixed).contains("unreadable"));
    }

    #[test]
    fn a_journal_with_many_superseded_lines_is_compacted() {
        let dir = tempfile::tempdir().unwrap();
        let ingress = oid("ingress");
        for round in 0..(COMPACT_SLACK + 10) {
            record(
                dir.path(),
                ingress,
                &[
                    (PathBuf::from("a.txt"), file(&round.to_string())),
                    (PathBuf::from("b.txt"), file("steady")),
                ],
            )
            .unwrap();
        }
        let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
        assert_eq!(journal.entries.len(), 2);
        let text = std::fs::read_to_string(journal_path(dir.path())).unwrap();
        assert_eq!(text.lines().count(), 3, "header plus one line per path");
        let journal = open_for_round(dir.path(), ingress, never_called).unwrap();
        assert_eq!(
            journal.entries.get(b"a.txt".as_slice()),
            Some(&file(&(COMPACT_SLACK + 9).to_string())),
            "the compacted journal keeps the last line for each path"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_path_that_is_not_utf8_cannot_be_journaled() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let path = Path::new(std::ffi::OsStr::from_bytes(b"bad\xff.txt"));
        assert!(record_intent(dir.path(), oid("ingress"), path, intent("x")).is_err());
    }
}
