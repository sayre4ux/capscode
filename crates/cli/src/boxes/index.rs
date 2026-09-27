//! File-only box name cache used by help and completion. Runtime labels remain
//! the source of truth and refresh this file when `ls` is run.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use capscode_core::Config;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Entry {
    pub name: String,
    pub id: String,
    pub session: String,
    pub project: PathBuf,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub mode: String,
    pub providers: Vec<String>,
    pub created: String,
    pub last_entered: String,
    pub last_synced: String,
    /// Toolchain last used by this box. Empty for a pre-toolchain box until its
    /// stopped capsule image tag is read as the fallback source of truth.
    pub toolchain: String,
    pub toolchain_pinned: bool,
    /// The box was created without a persistent cache; explicit resume --cache clears this.
    pub no_cache: bool,
}

pub(crate) fn path() -> PathBuf {
    Config::config_home().join("boxes.json")
}

pub(crate) fn read() -> Result<Vec<Entry>> {
    read_from(&path())
}

pub(crate) fn read_from(path: &std::path::Path) -> Result<Vec<Entry>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut entries: Vec<Entry> =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let mut names = std::collections::HashSet::new();
    let mut sessions = std::collections::HashSet::new();
    entries
        .retain(|entry| names.insert(entry.name.clone()) && sessions.insert(entry.session.clone()));
    Ok(entries)
}

pub(crate) fn write(entries: &[Entry]) -> Result<()> {
    write_to(&path(), entries)
}

pub(crate) fn write_to(path: &Path, entries: &[Entry]) -> Result<()> {
    let parent = path.parent().context("box index has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary index in {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut temporary, entries).context("serializing box index")?;
    temporary.write_all(b"\n").context("writing box index")?;
    temporary
        .as_file()
        .sync_all()
        .context("syncing box index")?;
    temporary
        .persist(path)
        .with_context(|| format!("atomically replacing {}", path.display()))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing {}", parent.display()))?;
    Ok(())
}

/// Box names from the index file; never contacts the container runtime.
pub fn names() -> Vec<String> {
    read()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

pub(crate) fn upsert(entry: Entry) -> Result<()> {
    upsert_at(&path(), entry)
}

pub(crate) fn upsert_at(path: &Path, mut entry: Entry) -> Result<()> {
    let mut entries = read_from(path)?;
    entries.retain(|old| old.session != entry.session && old.name != entry.name);
    if entry.created.is_empty() {
        entry.created = now();
    }
    entries.push(entry);
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    write_to(path, &entries)
}

pub(crate) fn update_last_entered(session: &str) -> Result<()> {
    let mut entries = read()?;
    if let Some(entry) = entries.iter_mut().find(|entry| entry.session == session) {
        entry.last_entered = now();
        write(&entries)?;
    }
    Ok(())
}

pub(crate) fn entry_for_session(session: &str) -> Result<Option<Entry>> {
    entry_for_session_from(&path(), session)
}

pub(crate) fn entry_for_session_from(path: &Path, session: &str) -> Result<Option<Entry>> {
    Ok(read_from(path)?
        .into_iter()
        .find(|entry| entry.session == session))
}

pub(crate) fn now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_seconds / 3600,
        (day_seconds / 60) % 60,
        day_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_boxes_json_without_toolchain_fields_still_loads() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("boxes.json");
        fs::write(
            &path,
            r#"[{"name":"alpha-123abc","id":"123abc","session":"s1","project":"/tmp/alpha","mode":"copy","providers":[],"created":"","last_entered":"","last_synced":""}]"#,
        )
        .unwrap();
        let entries = read_from(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].toolchain, "");
        assert!(!entries[0].toolchain_pinned);
    }

    #[test]
    fn no_cache_defaults_for_existing_boxes_and_round_trips_when_enabled() {
        let old_record: Entry = serde_json::from_str(r#"{"name":"alpha-123abc"}"#).unwrap();
        assert!(!old_record.no_cache);

        let no_cache: Entry =
            serde_json::from_str(r#"{"name":"alpha-123abc","no_cache":true}"#).unwrap();
        let encoded = serde_json::to_value(no_cache).unwrap();
        assert_eq!(encoded["no_cache"], true);
    }
}
