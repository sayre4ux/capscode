//! Update-channel tests. No test touches the network or the real `~/.capscode`:
//! fetches go through fake transports and every path is a temporary directory.

use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

use super::channel::{self, ChannelName, MAX_LOCK_BYTES, MAX_MANIFEST_BYTES};
use super::fetch::{self, Fetch, Fetcher, RawResponse, Transport};
use super::passive::{self, PassiveInputs, INTERVAL_SECS};
use super::state::{self, CheckState, UpdateState};
use super::*;
use crate::toolchain::{self, ResolvedLock};

const BUILTIN: &str = include_str!("../../../../images/toolchain.lock.toml");
const CHANNEL: &str = "sayre4ux/capscode";
const NOW: u64 = 1_800_000_000;

fn channel_name() -> ChannelName {
    ChannelName::parse(CHANNEL).unwrap()
}

/// A valid channel lock: the built-in lock at `sequence`, with optional edits.
fn lock_text(sequence: u64, edit: impl Fn(String) -> String) -> String {
    let text = BUILTIN.replacen("sequence = 1\n", &format!("sequence = {sequence}\n"), 1);
    edit(text)
}

fn digest_of(text: &str) -> String {
    toolchain::digest(&toolchain::parse_validated(text).unwrap())
}

/// The digest of the shipped lock, as the channel would publish it.
fn builtin_digest() -> String {
    toolchain::digest(&toolchain::builtin_lock().unwrap())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn manifest_value(sequence: u64, lock: &str, digest: &str) -> serde_json::Value {
    let tag = format!("toolchain-{sequence}");
    serde_json::json!({
        "schema": 1,
        "generated_at": "2026-10-20T06:12:00Z",
        "cli": {
            "version": "0.1.0",
            "tag": "v0.1.0",
            "commit": "749dcba0c0e2",
            "min_toolchain_sequence": 1,
            "notes_url": "https://github.com/sayre4ux/capscode/releases/tag/v0.1.0",
            "yanked_versions": []
        },
        "toolchain": {
            "sequence": sequence,
            "tag": tag,
            "digest": digest,
            "min_cli_version": "0.1.0",
            "lock_url": format!("https://github.com/sayre4ux/capscode/releases/download/{tag}/toolchain.lock.toml"),
            "lock_sha256": sha256_hex(lock.as_bytes()),
            "attestation_url": format!("https://github.com/sayre4ux/capscode/releases/download/{tag}/toolchain.lock.toml.sigstore"),
            "yanked": []
        }
    })
}

fn manifest_for(sequence: u64, lock: &str) -> serde_json::Value {
    manifest_value(sequence, lock, &digest_of(lock))
}

/// Serves fixed bodies by URL and records every URL requested.
#[derive(Default)]
struct FakeFetcher {
    bodies: HashMap<String, Result<Vec<u8>, String>>,
    log: Mutex<Vec<String>>,
}

impl FakeFetcher {
    fn serving(manifest: &serde_json::Value, lock: &str) -> Self {
        let mut fetcher = Self::default();
        fetcher.bodies.insert(
            channel_name().manifest_url(),
            Ok(serde_json::to_vec(manifest).unwrap()),
        );
        let lock_url = manifest["toolchain"]["lock_url"]
            .as_str()
            .unwrap()
            .to_owned();
        fetcher
            .bodies
            .insert(lock_url, Ok(lock.as_bytes().to_vec()));
        fetcher
    }

    fn requested(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Fetch for FakeFetcher {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        self.log.lock().unwrap().push(url.to_owned());
        match self.bodies.get(url) {
            Some(Ok(body)) if body.len() <= limit => Ok(body.clone()),
            Some(Ok(_)) => Err(anyhow!("over limit")),
            Some(Err(error)) => Err(anyhow!(error.clone())),
            None => Err(anyhow!("network unreachable")),
        }
    }
}

struct Answer {
    value: bool,
    asked: usize,
    prompt: String,
}

impl Confirmer for Answer {
    fn confirm(&mut self, prompt: &str) -> Result<bool> {
        self.asked += 1;
        self.prompt = prompt.to_owned();
        Ok(self.value)
    }
}

fn no_prompt() -> Answer {
    Answer {
        value: false,
        asked: 0,
        prompt: String::new(),
    }
}

fn builtin_resolved() -> ResolvedLock {
    ResolvedLock {
        lock: toolchain::builtin_lock().unwrap(),
        source: "built-in".to_owned(),
        path: None,
    }
}

fn context(home: &Path) -> UpdateContext {
    UpdateContext {
        channel: channel_name(),
        verify_with_gh: false,
        gh_path: None,
        own_version: "0.1.0".to_owned(),
        active: builtin_resolved(),
        config_override: false,
        paths: UpdatePaths::in_home(home),
        interactive: false,
        now_unix: NOW,
        cli_update_command: "capscode update --cli".to_owned(),
    }
}

fn apply() -> UpdateOptions {
    UpdateOptions {
        check_only: false,
        yes: false,
    }
}

fn run(context: &UpdateContext, fetcher: &FakeFetcher, options: UpdateOptions) -> Result<Report> {
    run_update(context, fetcher, options, &mut no_prompt())
}

fn error_text(result: Result<Report>) -> String {
    format!(
        "{:#}",
        result.expect_err("expected the update to be refused")
    )
}

fn assert_nothing_written(home: &Path) {
    assert!(
        !home.join("toolchain.lock.toml").exists(),
        "runtime lock was written"
    );
    assert!(!home.join("toolchains").exists(), "a snapshot was written");
    assert!(
        !home.join("update-state.json").exists(),
        "the sequence was recorded"
    );
}

fn bump_codex(text: String, version: &str) -> String {
    text.replace("version = \"0.159.3\"", &format!("version = \"{version}\""))
}

// ---- happy path -------------------------------------------------------------

#[test]
fn a_valid_newer_toolchain_is_applied_and_its_sequence_recorded() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let report = run(&context(home.path()), &fetcher, apply()).unwrap();
    let digest = digest_of(&lock);
    assert_eq!(
        report.outcome,
        Outcome::Applied {
            digest: digest.clone()
        }
    );
    let written = toolchain::read_validated(&home.path().join("toolchain.lock.toml")).unwrap();
    assert_eq!(written.tools["codex"].version, "0.160.0");
    assert_eq!(written.sequence, Some(2));
    assert!(home
        .path()
        .join(format!("toolchains/{digest}.lock.toml"))
        .is_file());
    let recorded = state::read_update_state(&home.path().join("update-state.json")).unwrap();
    assert_eq!(recorded.highest_accepted_sequence, 2);
    assert!(report
        .lines
        .iter()
        .any(|line| line.contains("codex 0.159.3 -> 0.160.0")));
    let shown = provenance_lines(home.path(), &digest).join("\n");
    assert!(shown.contains("toolchain.lock.toml.sigstore"), "{shown}");
    assert!(shown.contains("gh attestation verify"), "{shown}");
}

#[test]
fn check_only_reports_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let report = run(
        &context(home.path()),
        &fetcher,
        UpdateOptions {
            check_only: true,
            yes: false,
        },
    )
    .unwrap();
    assert!(matches!(report.outcome, Outcome::Available { .. }));
    assert_nothing_written(home.path());
}

// ---- checks on the offered lock ------------------------------------------------

#[test]
fn a_lock_whose_sha256_differs_from_the_manifest_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let mut manifest = manifest_for(2, &lock);
    manifest["toolchain"]["lock_sha256"] = serde_json::json!("0".repeat(64));
    let fetcher = FakeFetcher::serving(&manifest, &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert!(error.contains("SHA-256"), "{error}");
    assert_nothing_written(home.path());
}

#[test]
fn manifest_schema_two_is_refused_with_update_guidance() {
    let lock = lock_text(2, |text| text);
    let mut manifest = manifest_for(2, &lock);
    manifest["schema"] = serde_json::json!(2);
    let error = channel::parse(&serde_json::to_vec(&manifest).unwrap(), &channel_name())
        .unwrap_err()
        .to_string();
    assert!(error.contains("schema 2"), "{error}");
    assert!(error.contains("update CapsCode"), "{error}");
}

#[test]
fn a_lock_with_schema_two_is_refused() {
    let lock = lock_text(2, |text| text.replacen("schema = 1\n", "schema = 2\n", 1));
    let manifest: channel::Channel =
        serde_json::from_value(manifest_value(2, &lock, &builtin_digest())).unwrap();
    let error = format!(
        "{:#}",
        verify_lock(lock.as_bytes(), &manifest, "0.1.0").unwrap_err()
    );
    assert!(error.contains("schema 2"), "{error}");
}

#[test]
fn a_toolchain_needing_a_newer_cli_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| {
        text.replace("min_cli_version = \"0.1.0\"", "min_cli_version = \"9.0.0\"")
    });
    let mut manifest = manifest_for(2, &lock);
    manifest["toolchain"]["min_cli_version"] = serde_json::json!("9.0.0");
    let fetcher = FakeFetcher::serving(&manifest, &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert!(error.contains("9.0.0 or newer"), "{error}");
    assert_eq!(
        fetcher.requested(),
        vec![channel_name().manifest_url()],
        "the lock was fetched"
    );
    assert_nothing_written(home.path());

    // The lock's own min_cli_version is enforced even if the manifest understates it.
    let manifest: channel::Channel = serde_json::from_value(manifest_for(2, &lock)).unwrap();
    let mut manifest = manifest;
    manifest.toolchain.min_cli_version = "9.0.0".to_owned();
    let error = format!(
        "{:#}",
        verify_lock(lock.as_bytes(), &manifest, "0.1.0").unwrap_err()
    );
    assert!(error.contains("9.0.0 or newer"), "{error}");
}

#[test]
fn a_lock_min_cli_version_that_differs_from_the_manifest_is_refused() {
    let lock = lock_text(2, |text| text);
    let mut manifest: channel::Channel = serde_json::from_value(manifest_for(2, &lock)).unwrap();
    manifest.toolchain.min_cli_version = "0.0.9".to_owned();
    let error = format!(
        "{:#}",
        verify_lock(lock.as_bytes(), &manifest, "0.1.0").unwrap_err()
    );
    assert!(error.contains("min_cli_version does not match"), "{error}");
}

#[test]
fn an_equal_sequence_is_not_applied() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(1, |text| bump_codex(text, "0.160.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(1, &lock), &lock);
    let report = run(&context(home.path()), &fetcher, apply()).unwrap();
    assert_eq!(report.outcome, Outcome::UpToDate);
    assert_eq!(
        fetcher.requested(),
        vec![channel_name().manifest_url()],
        "the lock was fetched"
    );
    assert_nothing_written(home.path());
}

#[test]
fn an_up_to_date_check_still_reports_a_withdrawn_active_toolchain() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(1, |text| text);
    let mut manifest = manifest_for(1, &lock);
    manifest["toolchain"]["yanked"] = serde_json::json!(["codex@0.159.3"]);
    let fetcher = FakeFetcher::serving(&manifest, &lock);
    let report = run(&context(home.path()), &fetcher, apply()).unwrap();
    assert_eq!(report.outcome, Outcome::UpToDate);
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.contains("was withdrawn (codex@0.159.3)")),
        "{:?}",
        report.lines
    );
}

#[test]
fn a_replayed_older_manifest_is_refused_even_after_toolchain_use_built_in() {
    let home = tempfile::tempdir().unwrap();
    // This machine accepted sequence 5 earlier, then went back to the built-in lock (sequence 1).
    let accepted = UpdateState {
        highest_accepted_sequence: 5,
        ..UpdateState::default()
    };
    state::write_json(&home.path().join("update-state.json"), &accepted).unwrap();
    let lock = lock_text(3, |text| bump_codex(text, "0.150.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(3, &lock), &lock);
    let report = run(&context(home.path()), &fetcher, apply()).unwrap();
    assert_eq!(report.outcome, Outcome::UpToDate);
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.contains("older than sequence 5")),
        "{:?}",
        report.lines
    );
    assert!(!home.path().join("toolchain.lock.toml").exists());
}

#[test]
fn a_damaged_sequence_record_fails_closed() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("update-state.json"), "{not json").unwrap();
    let lock = lock_text(2, |text| text);
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert!(error.contains("damaged"), "{error}");
}

#[test]
fn a_lock_sequence_or_digest_that_differs_from_the_manifest_is_refused() {
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let mut manifest: channel::Channel = serde_json::from_value(manifest_for(2, &lock)).unwrap();
    manifest.toolchain.digest = builtin_digest();
    let error = format!(
        "{:#}",
        verify_lock(lock.as_bytes(), &manifest, "0.1.0").unwrap_err()
    );
    assert!(error.contains("do not match channel digest"), "{error}");

    let other = lock_text(3, |text| bump_codex(text, "0.160.0"));
    let mut manifest: channel::Channel = serde_json::from_value(manifest_for(2, &other)).unwrap();
    manifest.toolchain.sequence = 2;
    let error = format!(
        "{:#}",
        verify_lock(other.as_bytes(), &manifest, "0.1.0").unwrap_err()
    );
    assert!(error.contains("sequence does not match"), "{error}");
}

#[test]
fn an_untested_channel_lock_is_refused() {
    let lock = lock_text(2, |text| {
        text.replacen("schema = 1\n", "schema = 1\nuntested = true\n", 1)
    });
    let manifest: channel::Channel = serde_json::from_value(manifest_for(2, &lock)).unwrap();
    let error = format!(
        "{:#}",
        verify_lock(lock.as_bytes(), &manifest, "0.1.0").unwrap_err()
    );
    assert!(error.contains("untested"), "{error}");
}

fn refused_lock(edit: impl Fn(String) -> String) -> String {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, edit);
    // The manifest digest is the built-in one so the manifest itself still validates
    // even when the edited lock cannot be parsed.
    let manifest = manifest_value(2, &lock, &builtin_digest());
    let fetcher = FakeFetcher::serving(&manifest, &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert_nothing_written(home.path());
    error
}

#[test]
fn a_package_outside_the_compiled_in_allowlist_is_refused() {
    let error = refused_lock(|text| text.replace("\"@openai/codex\"", "\"@attacker/codex\""));
    assert!(error.contains("must be '@openai/codex'"), "{error}");
    let error = refused_lock(|text| {
        text.replace("[tools]\n", "[tools]\nevil = { package = \"evil\", version = \"1.0.0\", integrity = \"sha512-frr0DLmVHSDNjw+hC6ZmXMVQ8yH4nNmVcI4lVFzWt0bdPNZP7clOKsuLcqSdwQabIpe6A3NEc3TJfiBVD8B2Bg==\", resolved_before = \"2026-10-01T04:51:24Z\" }\n")
    });
    assert!(error.contains("unknown tool 'evil'"), "{error}");
}

#[test]
fn an_image_repository_outside_the_allowlist_is_refused() {
    let error = refused_lock(|text| {
        text.replace("image  = \"alpine:3\"", "image  = \"attacker/alpine:3\"")
    });
    assert!(error.contains("'alpine' image repository"), "{error}");
    let error = refused_lock(|text| {
        text.replace(
            "image  = \"debian:bookworm-slim\"",
            "image  = \"docker.io/attacker/debian:bookworm-slim\"",
        )
    });
    assert!(error.contains("'debian' image repository"), "{error}");
    let error = refused_lock(|text| {
        text.replace(
            "bun_image   = \"oven/bun:1-debian\"",
            "bun_image   = \"node:22\"",
        )
    });
    assert!(error.contains("'oven/bun' image repository"), "{error}");
    let error =
        refused_lock(|text| text.replace("[base]\n", "[base]\nextra_image = \"attacker/x:1\"\n"));
    assert!(error.contains("unknown key 'extra_image'"), "{error}");
}

#[test]
fn shell_metacharacters_and_newlines_in_lock_values_are_refused() {
    let cases: [(&str, String, &str); 7] = [
        (
            "version",
            "1.0.0; rm -rf /".to_owned(),
            "version for 'codex'",
        ),
        (
            "version",
            "1.0.0\\nRUN curl evil".to_owned(),
            "version for 'codex'",
        ),
        (
            "version",
            "$(touch /tmp/pwned)".to_owned(),
            "version for 'codex'",
        ),
        (
            "resolved_before",
            "2026-10-01T04:51:25Z; rm -rf /".to_owned(),
            "resolved_before for 'codex'",
        ),
        (
            "resolved_before",
            "$(id)".to_owned(),
            "resolved_before for 'codex'",
        ),
        (
            "integrity",
            "sha512-$(id)".to_owned(),
            "integrity for 'codex'",
        ),
        ("channel", "latest`id`".to_owned(), "channel for 'codex'"),
    ];
    for (field, value, expected) in cases {
        let error = refused_lock(|text| {
            let line_start = text.find("codex    = {").unwrap();
            let line_end = line_start + text[line_start..].find('\n').unwrap();
            let line = &text[line_start..line_end];
            let key = format!("{field} = \"");
            let value_start = line.find(&key).unwrap() + key.len();
            let value_end = value_start + line[value_start..].find('"').unwrap();
            let replaced = format!("{}{}{}", &line[..value_start], value, &line[value_end..]);
            text.replace(line, &replaced)
        });
        assert!(error.contains(expected), "{field}={value}: {error}");
    }
    let error = refused_lock(|text| {
        text.replace(
            "digest = \"sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b\"",
            "digest = \"sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b; id\"",
        )
    });
    assert!(error.contains("[seeder].digest"), "{error}");
    let error = refused_lock(|text| {
        // Anchor on the codex-only field so recorded smoke values cannot make this a no-op.
        let replaced = text.replace(
            "daemon_seed_layout = 1 }",
            "daemon_seed_layout = 1, downgrade_reason = \"ok\\u001b[2J\" }",
        );
        assert_ne!(replaced, text, "the codex lock line was not found");
        replaced
    });
    assert!(error.contains("downgrade_reason for 'codex'"), "{error}");
}

#[test]
fn a_toolchain_containing_a_yanked_tool_version_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let mut manifest = manifest_for(2, &lock);
    manifest["toolchain"]["yanked"] = serde_json::json!(["codex@0.160.0"]);
    let fetcher = FakeFetcher::serving(&manifest, &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert!(error.contains("withdrawn codex@0.160.0"), "{error}");
    assert_nothing_written(home.path());
}

#[test]
fn a_toolchain_the_channel_itself_lists_as_withdrawn_is_not_downloaded() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let mut manifest = manifest_for(2, &lock);
    manifest["toolchain"]["yanked"] = serde_json::json!([digest_of(&lock)]);
    let fetcher = FakeFetcher::serving(&manifest, &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert!(error.contains("withdrawn"), "{error}");
    assert_eq!(
        fetcher.requested(),
        vec![channel_name().manifest_url()],
        "the lock was fetched"
    );
    assert_nothing_written(home.path());
}

#[test]
fn toolchain_use_refuses_a_yanked_digest_without_force() {
    let home = tempfile::tempdir().unwrap();
    let paths = UpdatePaths::in_home(home.path());
    let lock =
        toolchain::parse_validated(&lock_text(2, |text| bump_codex(text, "0.160.0"))).unwrap();
    let digest = toolchain::digest(&lock);
    toolchain::write_snapshot_in(&paths.snapshots, &lock, &digest).unwrap();

    for yanks in [vec![digest.clone()], vec!["codex@0.160.0".to_owned()]] {
        let error = crate::commands::toolchain_use_in(&paths, &digest, false, &yanks)
            .unwrap_err()
            .to_string();
        assert!(error.contains("withdrawn"), "{error}");
        assert!(error.contains("--force"), "{error}");
        assert!(!paths.runtime_lock.exists());
    }
    let warnings =
        crate::commands::toolchain_use_in(&paths, &digest, true, std::slice::from_ref(&digest))
            .unwrap();
    assert!(warnings[0].contains("withdrawn"));
    assert!(paths.runtime_lock.is_file());
}

#[test]
fn known_yanks_combine_the_stored_manifest_and_the_active_lock() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| text);
    let mut manifest: channel::Channel = serde_json::from_value(manifest_for(2, &lock)).unwrap();
    manifest.toolchain.yanked = vec!["codex@0.159.3".to_owned()];
    let check = CheckState {
        last_attempt_unix: NOW,
        channel: CHANNEL.to_owned(),
        manifest: Some(manifest),
    };
    state::write_json(&state::check_state_path(home.path()), &check).unwrap();
    let mut active = toolchain::builtin_lock().unwrap();
    active.yanked = vec!["6d07f8901234".to_owned()];
    let yanks = known_yanks_in(home.path(), Some(&channel_name()), &active);
    assert_eq!(
        yanks,
        vec!["6d07f8901234".to_owned(), "codex@0.159.3".to_owned()]
    );
    // A manifest stored for another channel is ignored.
    let other = ChannelName::parse("fork/capscode").unwrap();
    assert_eq!(
        known_yanks_in(home.path(), Some(&other), &active),
        vec!["6d07f8901234".to_owned()]
    );
}

#[test]
fn a_downgrade_without_a_recorded_reason_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.150.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let error = error_text(run(
        &context(home.path()),
        &fetcher,
        UpdateOptions {
            check_only: false,
            yes: true,
        },
    ));
    assert!(
        error.contains("without a recorded downgrade_reason"),
        "{error}"
    );
    assert_nothing_written(home.path());
}

fn downgrade_with_reason() -> String {
    lock_text(2, |text| {
        bump_codex(text, "0.150.0").replace(
            "home_exec_paths = [], daemon_seed_layout = 1 }",
            "home_exec_paths = [], daemon_seed_layout = 1, downgrade_reason = \"0.159.3 breaks sandbox exec\" }",
        )
    })
}

#[test]
fn a_downgrade_with_a_reason_needs_yes_when_not_interactive() {
    let home = tempfile::tempdir().unwrap();
    let lock = downgrade_with_reason();
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert!(error.contains("--yes"), "{error}");
    assert_nothing_written(home.path());

    let report = run(
        &context(home.path()),
        &fetcher,
        UpdateOptions {
            check_only: false,
            yes: true,
        },
    )
    .unwrap();
    assert!(matches!(report.outcome, Outcome::Applied { .. }));
    assert!(report
        .lines
        .iter()
        .any(|line| line.contains("downgrade: 0.159.3 breaks sandbox exec")));
}

#[test]
fn an_interactive_downgrade_asks_and_defaults_to_refusing() {
    let home = tempfile::tempdir().unwrap();
    let lock = downgrade_with_reason();
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let mut ctx = context(home.path());
    ctx.interactive = true;
    let mut declined = no_prompt();
    let error = format!(
        "{:#}",
        run_update(&ctx, &fetcher, apply(), &mut declined).unwrap_err()
    );
    assert!(error.contains("cancelled"), "{error}");
    assert_eq!(declined.asked, 1);
    assert!(
        declined
            .prompt
            .contains("codex 0.159.3 -> 0.150.0 (downgrade: 0.159.3 breaks sandbox exec)"),
        "{}",
        declined.prompt
    );
    assert_nothing_written(home.path());
    let mut accepted = Answer {
        value: true,
        asked: 0,
        prompt: String::new(),
    };
    let report = run_update(&ctx, &fetcher, apply(), &mut accepted).unwrap();
    assert!(matches!(report.outcome, Outcome::Applied { .. }));
}

// ---- gh attestation bridge -------------------------------------------------

#[cfg(unix)]
fn fake_gh(dir: &Path, exit_code: i32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("gh-args.txt");
    let script = dir.join("gh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" > '{}'\nexit {exit_code}\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

#[cfg(unix)]
#[test]
fn verify_with_gh_fails_closed_when_gh_is_missing_or_rejects() {
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);

    let home = tempfile::tempdir().unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let mut ctx = context(home.path());
    ctx.verify_with_gh = true;
    ctx.gh_path = Some(empty_path.path().as_os_str().to_owned());
    let error = error_text(run(&ctx, &fetcher, apply()));
    assert!(error.contains("`gh` is not on PATH"), "{error}");
    ctx.gh_path = None;
    let error = error_text(run(&ctx, &fetcher, apply()));
    assert!(error.contains("`gh` is not on PATH"), "{error}");
    assert_nothing_written(home.path());

    let rejecting = tempfile::tempdir().unwrap();
    let log = fake_gh(rejecting.path(), 1);
    ctx.gh_path = Some(rejecting.path().as_os_str().to_owned());
    let error = error_text(run(&ctx, &fetcher, apply()));
    assert!(error.contains("rejected the downloaded lock"), "{error}");
    assert_nothing_written(home.path());
    let args = std::fs::read_to_string(log).unwrap();
    assert!(args.starts_with("attestation verify "), "{args}");
    assert!(args.contains("--repo sayre4ux/capscode"), "{args}");
    assert!(
        args.contains(
            "--signer-workflow sayre4ux/capscode/.github/workflows/release-toolchain.yml"
        ),
        "{args}"
    );

    let accepting = tempfile::tempdir().unwrap();
    fake_gh(accepting.path(), 0);
    ctx.gh_path = Some(accepting.path().as_os_str().to_owned());
    let report = run(&ctx, &fetcher, apply()).unwrap();
    assert!(matches!(report.outcome, Outcome::Applied { .. }));
}

// ---- manifest validation ---------------------------------------------------

type ManifestEdit = Box<dyn Fn(&mut serde_json::Value)>;

fn manifest_error(edit: impl Fn(&mut serde_json::Value)) -> String {
    let lock = lock_text(2, |text| text);
    let mut manifest = manifest_for(2, &lock);
    edit(&mut manifest);
    format!(
        "{:#}",
        channel::parse(&serde_json::to_vec(&manifest).unwrap(), &channel_name()).unwrap_err()
    )
}

#[test]
fn manifest_fields_are_shape_checked_and_length_capped() {
    let long_url = format!("https://github.com/sayre4ux/capscode/{}", "a".repeat(2100));
    let cases: Vec<(ManifestEdit, &str)> = vec![
        (
            Box::new(|m| {
                m["toolchain"]["lock_url"] = "https://github.com/attacker/capscode/releases/download/toolchain-2/toolchain.lock.toml".into()
            }),
            "lock_url must be",
        ),
        (
            Box::new(|m| {
                m["toolchain"]["lock_url"] = "http://github.com/sayre4ux/capscode/releases/download/toolchain-2/toolchain.lock.toml".into()
            }),
            "lock_url must be",
        ),
        (
            Box::new(move |m| m["cli"]["notes_url"] = long_url.clone().into()),
            "notes_url",
        ),
        (
            Box::new(|m| {
                m["cli"]["notes_url"] = "https://github.com/sayre4ux/capscode/\u{1b}[2J".into()
            }),
            "notes_url",
        ),
        (
            Box::new(|m| m["toolchain"]["digest"] = "457332B75990".into()),
            "digest",
        ),
        (
            Box::new(|m| m["toolchain"]["tag"] = "toolchain-3".into()),
            "toolchain-<sequence>",
        ),
        (
            Box::new(|m| m["toolchain"]["sequence"] = 0.into()),
            "toolchain.sequence",
        ),
        (
            Box::new(|m| m["toolchain"]["sequence"] = 9_007_199_254_740_992_u64.into()),
            "toolchain.sequence",
        ),
        (
            Box::new(|m| m["toolchain"]["yanked"] = serde_json::json!(["codex@1.0.0; rm -rf /"])),
            "yanked",
        ),
        (
            Box::new(|m| m["toolchain"]["yanked"] = serde_json::json!(["evil@1.0.0"])),
            "yanked",
        ),
        (
            Box::new(|m| m["cli"]["yanked_versions"] = serde_json::json!(["0.1.0\n"])),
            "yanked_versions",
        ),
        (
            Box::new(|m| m["cli"]["version"] = "0.1.0$(id)".into()),
            "CLI version",
        ),
        (
            Box::new(|m| m["toolchain"]["attestation_sha256"] = "xyz".into()),
            "attestation_sha256",
        ),
        (
            Box::new(|m| {
                m["toolchain"]
                    .as_object_mut()
                    .unwrap()
                    .remove("lock_sha256")
                    .map(|_| ())
                    .unwrap()
            }),
            "missing or mistyped",
        ),
        (
            Box::new(|m| m["toolchain"]["sequence"] = "2".into()),
            "missing or mistyped",
        ),
    ];
    for (edit, expected) in cases {
        let error = manifest_error(edit);
        assert!(
            error.contains(expected),
            "expected '{expected}' in: {error}"
        );
    }
    // Unknown fields are tolerated for forward compatibility.
    let lock = lock_text(2, |text| text);
    let mut manifest = manifest_for(2, &lock);
    manifest["future"] = serde_json::json!({"anything": true});
    channel::parse(&serde_json::to_vec(&manifest).unwrap(), &channel_name()).unwrap();
}

#[test]
fn an_oversized_manifest_is_refused_by_the_parser() {
    let bytes = vec![b' '; MAX_MANIFEST_BYTES + 1];
    let error = channel::parse(&bytes, &channel_name())
        .unwrap_err()
        .to_string();
    assert!(error.contains("larger than"), "{error}");
}

#[test]
fn update_channel_must_be_a_github_owner_and_repo() {
    for good in ["sayre4ux/capscode", "my-org/capscode.fork", "a/b_c-d"] {
        ChannelName::parse(good).unwrap();
    }
    for bad in [
        "",
        "capscode",
        "owner/",
        "/repo",
        "owner/repo/extra",
        "-owner/repo",
        "owner-/repo",
        "ow--ner/repo",
        "owner/..",
        "owner/re po",
        "owner/repo?x=1",
        "evil.com/repo",
        "owner/repo#",
        "owner\n/repo",
    ] {
        assert!(ChannelName::parse(bad).is_err(), "accepted {bad:?}");
    }
    assert!(crate::config::parse_update_channel("attacker.example/x").is_err());
    let fork = ChannelName::parse("fork-owner/capscode").unwrap();
    assert_eq!(
        fork.manifest_url(),
        "https://raw.githubusercontent.com/fork-owner/capscode/refs/heads/channel/channel.json"
    );
}

#[test]
fn version_comparison_follows_semver_precedence() {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let cmp = |left: &str, right: &str| compare_versions(left, right).unwrap();
    assert_eq!(cmp("0.160.0", "0.159.3"), Greater);
    assert_eq!(cmp("0.159.3", "0.160.0"), Less);
    assert_eq!(cmp("1.0.0", "1.0.0-rc.1"), Greater);
    assert_eq!(cmp("1.0.0-rc.2", "1.0.0-rc.10"), Less);
    assert_eq!(cmp("1.0.0+build", "1.0.0"), Equal);
    assert_eq!(cmp("10.0.0", "9.0.0"), Greater);
    assert_eq!(cmp("1.0.0-alpha", "1.0.0-alpha.1"), Less);
    assert_eq!(cmp("1.0.0-alpha.1", "1.0.0-alpha.beta"), Less);
    assert_eq!(cmp("1.0.0-beta.11", "1.0.0-rc.1"), Less);
    assert_eq!(cmp("1.0.0-rc-1", "1.0.0-rc-1+build.2"), Equal);
}

const HUGE: &str = "99999999999999999999999.0.0";

#[test]
fn versions_that_do_not_fit_cannot_be_compared() {
    for (left, right) in [
        (HUGE, "0.1.0"),
        ("0.1.0", HUGE),
        (HUGE, HUGE),
        ("1.0.0-rc.99999999999999999999999", "1.0.0-rc.1"),
        ("not a version", "0.1.0"),
        ("1.2", "1.2.0"),
    ] {
        assert!(
            compare_versions(left, right).is_err(),
            "{left} vs {right} compared"
        );
    }
}

#[test]
fn a_manifest_with_an_incomparable_version_is_refused() {
    let lock = lock_text(2, |text| text);
    for (section, field, value) in [
        ("toolchain", "min_cli_version", serde_json::json!(HUGE)),
        ("cli", "version", serde_json::json!(HUGE)),
        ("cli", "yanked_versions", serde_json::json!([HUGE])),
    ] {
        let mut manifest = manifest_for(2, &lock);
        manifest[section][field] = value;
        if field == "version" {
            manifest["cli"]["tag"] = serde_json::json!(format!("v{HUGE}"));
        }
        let error = channel::parse(&serde_json::to_vec(&manifest).unwrap(), &channel_name())
            .expect_err("accepted a version that does not fit");
        assert!(format!("{error:#}").contains("SemVer"), "{error:#}");
    }
}

#[test]
fn a_lock_whose_minimum_cli_version_cannot_be_compared_is_refused() {
    let lock = lock_text(2, |text| {
        text.replacen(
            "min_cli_version = \"0.1.0\"",
            &format!("min_cli_version = \"{HUGE}\""),
            1,
        )
    });
    // Same tools as the built-in lock, so the same digest.
    let digest = digest_of(&lock_text(2, |text| text));
    let mut manifest: channel::Channel =
        serde_json::from_value(manifest_value(2, &lock, &digest)).unwrap();
    // Built directly, as if the manifest's own check had let the value through.
    manifest.toolchain.min_cli_version = HUGE.to_owned();
    let error = verify_lock(lock.as_bytes(), &manifest, "0.1.0")
        .expect_err("a minimum CLI version that cannot be compared passed");
    let shown = format!("{error:#}");
    assert!(
        shown.contains("minimum CapsCode version") || shown.contains("failed validation"),
        "{shown}"
    );
}

#[test]
fn a_tool_version_that_cannot_be_compared_is_never_treated_as_unchanged() {
    let offered = toolchain::builtin_lock().unwrap();
    let mut active = offered.clone();
    active
        .tools
        .get_mut("codex")
        .unwrap()
        .version
        .clone_from(&HUGE.to_owned());
    // Comparing 0.159.3 against a version that cannot be ordered must not pass as
    // "no downgrade".
    let error = tool_changes(&active, &offered).expect_err("an unorderable version passed");
    assert!(format!("{error:#}").contains("codex"), "{error:#}");
    let mut offered_huge = offered.clone();
    offered_huge.tools.get_mut("codex").unwrap().version = HUGE.to_owned();
    assert!(tool_changes(&offered, &offered_huge).is_err());
    let changes = tool_changes(&offered, &offered).unwrap();
    assert!(changes.iter().all(|change| !change.downgrade));
}

// ---- HTTPS fetch policy ------------------------------------------------------

/// Status, Location, Content-Length, body.
type Scripted<L> = (u16, Option<L>, Option<u64>, Vec<u8>);

struct ScriptedTransport {
    responses: Mutex<VecDeque<Scripted<String>>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl ScriptedTransport {
    fn new(responses: Vec<Scripted<&str>>) -> (Self, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(status, location, length, body)| {
                            (status, location.map(str::to_owned), length, body)
                        })
                        .collect(),
                ),
                log: Arc::clone(&log),
            },
            log,
        )
    }
}

impl Transport for ScriptedTransport {
    fn get(&self, url: &str) -> Result<RawResponse> {
        self.log.lock().unwrap().push(url.to_owned());
        let (status, location, content_length, body) =
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted response"))?;
        Ok(RawResponse {
            status,
            location,
            content_length,
            body: Box::new(Cursor::new(body)),
        })
    }
}

const ASSET: &str =
    "https://github.com/sayre4ux/capscode/releases/download/toolchain-2/toolchain.lock.toml";

#[test]
fn a_redirect_to_a_host_outside_the_allowlist_is_refused() {
    for location in [
        "https://evil.example/toolchain.lock.toml",
        "http://release-assets.githubusercontent.com/x",
        "https://user:pass@release-assets.githubusercontent.com/x",
        "https://release-assets.githubusercontent.com:8443/x",
        "//evil.example/x",
        "https://github.com.evil.example/x",
    ] {
        let (transport, log) = ScriptedTransport::new(vec![
            (302, Some(location), None, Vec::new()),
            (200, None, None, b"should not be read".to_vec()),
        ]);
        let error = format!(
            "{:#}",
            Fetcher::new(transport)
                .get(ASSET, MAX_LOCK_BYTES)
                .unwrap_err()
        );
        assert!(error.contains("refusing"), "{location}: {error}");
        assert_eq!(log.lock().unwrap().len(), 1, "{location} was followed");
    }
}

#[test]
fn a_redirect_to_github_release_storage_is_followed() {
    let (transport, log) = ScriptedTransport::new(vec![
        (302, Some("https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sp=r"), None, Vec::new()),
        (200, None, Some(5), b"hello".to_vec()),
    ]);
    let body = Fetcher::new(transport).get(ASSET, MAX_LOCK_BYTES).unwrap();
    assert_eq!(body, b"hello");
    assert_eq!(log.lock().unwrap().len(), 2);
}

#[test]
fn redirect_loops_are_bounded() {
    let responses = (0..10)
        .map(|_| {
            (
                302,
                Some("https://github.com/sayre4ux/capscode/x"),
                None,
                Vec::new(),
            )
        })
        .collect();
    let (transport, _) = ScriptedTransport::new(responses);
    let error = Fetcher::new(transport)
        .get(ASSET, MAX_LOCK_BYTES)
        .unwrap_err()
        .to_string();
    assert!(error.contains("too many redirects"), "{error}");
}

#[test]
fn an_oversized_body_is_refused() {
    let (transport, _) =
        ScriptedTransport::new(vec![(200, None, None, vec![b'a'; MAX_MANIFEST_BYTES + 1])]);
    let error = format!(
        "{:#}",
        Fetcher::new(transport)
            .get(&channel_name().manifest_url(), MAX_MANIFEST_BYTES)
            .unwrap_err()
    );
    assert!(error.contains("larger than"), "{error}");

    let (transport, _) = ScriptedTransport::new(vec![(
        200,
        None,
        Some(MAX_LOCK_BYTES as u64 + 1),
        Vec::new(),
    )]);
    let error = Fetcher::new(transport)
        .get(ASSET, MAX_LOCK_BYTES)
        .unwrap_err()
        .to_string();
    assert!(error.contains("larger than"), "{error}");

    let (transport, _) =
        ScriptedTransport::new(vec![(200, None, None, vec![b'a'; MAX_LOCK_BYTES])]);
    assert_eq!(
        Fetcher::new(transport)
            .get(ASSET, MAX_LOCK_BYTES)
            .unwrap()
            .len(),
        MAX_LOCK_BYTES
    );
}

#[test]
fn only_https_urls_on_github_hosts_are_fetched() {
    for url in [
        "http://raw.githubusercontent.com/sayre4ux/capscode/refs/heads/channel/channel.json",
        "https://example.com/channel.json",
        "https://raw.githubusercontent.com:444/x",
        "https://token@github.com/x",
        "file:///etc/passwd",
    ] {
        let (transport, log) = ScriptedTransport::new(vec![(200, None, None, Vec::new())]);
        assert!(
            Fetcher::new(transport).get(url, 10).is_err(),
            "fetched {url}"
        );
        assert!(
            log.lock().unwrap().is_empty(),
            "{url} reached the transport"
        );
    }
    assert!(fetch::checked_url("https://GitHub.com/sayre4ux/capscode").is_ok());
}

#[test]
fn a_non_success_status_is_an_error() {
    let (transport, _) = ScriptedTransport::new(vec![(404, None, None, b"not found".to_vec())]);
    let error = Fetcher::new(transport)
        .get(ASSET, MAX_LOCK_BYTES)
        .unwrap_err()
        .to_string();
    assert!(error.contains("HTTP 404"), "{error}");
}

// ---- passive check -----------------------------------------------------------

fn passive_inputs(home: &Path) -> PassiveInputs {
    PassiveInputs {
        env_disabled: false,
        config: Some(capscode_core::UpdateConfig::default()),
        lock_override: None,
        stderr_tty: true,
        eligible: true,
        now_unix: NOW,
        home: home.to_path_buf(),
        cli_update_command: "capscode update --cli".to_owned(),
    }
}

fn newer_toolchain_fetcher() -> Arc<FakeFetcher> {
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    Arc::new(FakeFetcher::serving(&manifest_for(2, &lock), &lock))
}

#[test]
fn the_passive_check_reports_a_newer_toolchain_in_one_line() {
    let home = tempfile::tempdir().unwrap();
    let fetcher = newer_toolchain_fetcher();
    let check = passive::start_with(passive_inputs(home.path()), fetcher.clone()).unwrap();
    let line = check.with_version("0.1.0").finish().unwrap();
    assert_eq!(line, "toolchain 2 is available — capscode update");
    assert!(!line.contains('\n'));
    assert_eq!(
        fetcher.requested(),
        vec![channel_name().manifest_url()],
        "only the manifest is fetched"
    );
    let stored = state::read_check_state(&state::check_state_path(home.path()));
    assert_eq!(stored.last_attempt_unix, NOW);
    assert!(stored.manifest_for(&channel_name()).is_some());
}

#[test]
fn the_passive_check_is_silent_on_network_errors_and_backs_off() {
    let home = tempfile::tempdir().unwrap();
    let fetcher = Arc::new(FakeFetcher::default());
    let check = passive::start_with(passive_inputs(home.path()), fetcher.clone()).unwrap();
    assert_eq!(check.finish(), None);
    assert_eq!(fetcher.requested().len(), 1);
    // The failed attempt still counts toward the daily interval.
    assert!(passive::start_with(passive_inputs(home.path()), fetcher.clone()).is_none());
    assert_eq!(fetcher.requested().len(), 1);
}

#[test]
fn the_passive_check_is_silent_on_malformed_json() {
    for body in [
        b"{not json".to_vec(),
        b"{\"schema\":1}".to_vec(),
        b"<html>rate limited</html>".to_vec(),
    ] {
        let home = tempfile::tempdir().unwrap();
        let mut fetcher = FakeFetcher::default();
        fetcher
            .bodies
            .insert(channel_name().manifest_url(), Ok(body));
        let check = passive::start_with(passive_inputs(home.path()), Arc::new(fetcher)).unwrap();
        assert_eq!(check.finish(), None);
        let stored = state::read_check_state(&state::check_state_path(home.path()));
        assert!(stored.manifest.is_none());
    }
}

#[test]
fn the_passive_check_runs_at_most_once_per_day() {
    let home = tempfile::tempdir().unwrap();
    let fetcher = newer_toolchain_fetcher();
    let recent = CheckState {
        last_attempt_unix: NOW - INTERVAL_SECS + 60,
        ..CheckState::default()
    };
    state::write_json(&state::check_state_path(home.path()), &recent).unwrap();
    assert!(passive::start_with(passive_inputs(home.path()), fetcher.clone()).is_none());
    assert!(fetcher.requested().is_empty());

    let stale = CheckState {
        last_attempt_unix: NOW - INTERVAL_SECS,
        ..CheckState::default()
    };
    state::write_json(&state::check_state_path(home.path()), &stale).unwrap();
    let check = passive::start_with(passive_inputs(home.path()), fetcher.clone()).unwrap();
    assert!(check.finish().is_some());
    assert_eq!(fetcher.requested().len(), 1);

    // A timestamp from the future does not suppress the check forever.
    let future = CheckState {
        last_attempt_unix: NOW + 10 * INTERVAL_SECS,
        ..CheckState::default()
    };
    state::write_json(&state::check_state_path(home.path()), &future).unwrap();
    assert!(passive::start_with(passive_inputs(home.path()), fetcher).is_some());
}

#[test]
fn the_passive_check_can_be_turned_off() {
    let home = tempfile::tempdir().unwrap();
    let fetcher = newer_toolchain_fetcher();
    let mut off_by_config = passive_inputs(home.path());
    off_by_config.config = Some(capscode_core::UpdateConfig {
        check: false,
        ..capscode_core::UpdateConfig::default()
    });
    let mut off_by_env = passive_inputs(home.path());
    off_by_env.env_disabled = true;
    let mut not_a_tty = passive_inputs(home.path());
    not_a_tty.stderr_tty = false;
    let mut json_output = passive_inputs(home.path());
    json_output.eligible = false;
    let mut bad_config = passive_inputs(home.path());
    bad_config.config = None;
    let mut bad_channel = passive_inputs(home.path());
    bad_channel.config = Some(capscode_core::UpdateConfig {
        channel: "not a channel".to_owned(),
        ..capscode_core::UpdateConfig::default()
    });
    for inputs in [
        off_by_config,
        off_by_env,
        not_a_tty,
        json_output,
        bad_config,
        bad_channel,
    ] {
        assert!(passive::start_with(inputs, fetcher.clone()).is_none());
    }
    assert!(
        fetcher.requested().is_empty(),
        "a disabled check reached the network"
    );
    assert!(
        !state::check_state_path(home.path()).exists(),
        "a disabled check wrote state"
    );
}

#[test]
fn the_notice_reports_withdrawn_cli_and_toolchains() {
    let lock = lock_text(2, |text| text);
    let mut manifest: channel::Channel = serde_json::from_value(manifest_for(2, &lock)).unwrap();
    manifest.cli.version = "0.1.1".to_owned();
    manifest.cli.yanked_versions = vec!["0.1.0".to_owned()];
    let active = toolchain::builtin_lock().unwrap();
    let digest = toolchain::digest(&active);
    let line = notice_line(
        &manifest,
        "0.1.0",
        "capscode update --cli",
        &digest,
        &active,
        1,
    )
    .unwrap();
    assert!(
        line.starts_with("CapsCode 0.1.0 was withdrawn; update to 0.1.1"),
        "{line}"
    );

    manifest.cli.yanked_versions.clear();
    manifest.toolchain.yanked = vec![format!("codex@{}", active.tools["codex"].version)];
    let line = notice_line(
        &manifest,
        "0.1.0",
        "capscode update --cli",
        &digest,
        &active,
        2,
    )
    .unwrap();
    assert!(
        line.contains("CapsCode 0.1.1 is available (you have 0.1.0)"),
        "{line}"
    );
    assert!(
        line.contains(&format!("your toolchain {digest} was withdrawn")),
        "{line}"
    );

    manifest.cli.version = "0.1.0".to_owned();
    manifest.toolchain.yanked.clear();
    assert_eq!(
        notice_line(
            &manifest,
            "0.1.0",
            "capscode update --cli",
            &digest,
            &active,
            2
        ),
        None
    );
    manifest.toolchain.min_cli_version = "0.2.0".to_owned();
    assert_eq!(
        notice_line(
            &manifest,
            "0.1.0",
            "capscode update --cli",
            &digest,
            &active,
            1
        ),
        None,
        "unusable toolchains are not advertised"
    );
}

#[test]
fn the_release_script_fixture_parses_with_the_rust_types() {
    // The manifest scripts/release/test-channel-json.sh expects channel-json.sh to
    // produce, plus the optional attestation hash and a digest-form yank entry.
    let fixture = r#"{
      "schema": 1,
      "generated_at": "2026-10-18T06:17:00Z",
      "cli": {
        "version": "0.1.0",
        "tag": "v0.1.0",
        "commit": "749dcba0c0e2aabb",
        "min_toolchain_sequence": 1,
        "notes_url": "https://github.com/sayre4ux/capscode/releases/tag/v0.1.0",
        "yanked_versions": ["0.0.9"]
      },
      "toolchain": {
        "sequence": 7,
        "tag": "toolchain-7",
        "digest": "fb24882402b5",
        "min_cli_version": "0.1.0",
        "lock_url": "https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml",
        "lock_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "attestation_url": "https://github.com/sayre4ux/capscode/releases/download/toolchain-7/toolchain.lock.toml.sigstore",
        "attestation_sha256": "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
        "yanked": ["codex@0.160.0", "6d07f8901234"]
      }
    }"#;
    let manifest = channel::parse(fixture.as_bytes(), &channel_name()).unwrap();
    assert_eq!(manifest.toolchain.sequence, 7);
    assert_eq!(manifest.toolchain.yanked.len(), 2);
    assert!(manifest.toolchain.attestation_sha256.is_some());
    let without_hash = fixture.replace(
        "\"attestation_sha256\": \"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210\",\n",
        "",
    );
    assert!(channel::parse(without_hash.as_bytes(), &channel_name())
        .unwrap()
        .toolchain
        .attestation_sha256
        .is_none());
}

#[test]
fn an_update_is_refused_when_versions_cannot_be_compared() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let mut context = context(home.path());
    context.own_version = HUGE.to_owned();
    let error = error_text(run(&context, &fetcher, apply()));
    assert!(error.contains("comparing"), "{error}");
    assert!(!home.path().join("toolchain.lock.toml").exists());
}

// ---- stored manifest ordering ------------------------------------------------

fn manifest_at(sequence: u64, generated_at: &str, yanked: &[&str]) -> channel::Channel {
    let lock = lock_text(sequence, |text| text);
    let mut manifest: channel::Channel =
        serde_json::from_value(manifest_for(sequence, &lock)).unwrap();
    manifest.generated_at = generated_at.to_owned();
    manifest.toolchain.yanked = yanked.iter().map(|entry| (*entry).to_owned()).collect();
    manifest
}

#[test]
fn an_older_manifest_never_replaces_the_stored_one() {
    let channel = channel_name();
    let newer = manifest_at(3, "2026-10-20T06:12:00Z", &["codex@0.150.0"]);
    let mut check = CheckState::default();
    assert!(check.remember(&channel, &newer));

    // A stale copy: lower sequence, or same sequence but generated earlier.
    for stale in [
        manifest_at(2, "2026-10-21T00:00:00Z", &[]),
        manifest_at(3, "2026-10-19T23:59:59Z", &[]),
        manifest_at(3, "2026-10-20T06:11:59.999Z", &[]),
    ] {
        assert!(!check.remember(&channel, &stale), "{}", stale.generated_at);
        assert_eq!(check.manifest_for(&channel), Some(&newer));
    }
    // Fractional seconds order after the whole second.
    let later = manifest_at(
        3,
        "2026-10-20T06:12:00.5Z",
        &["codex@0.150.0", "6d07f8901234"],
    );
    assert!(check.remember(&channel, &later));
    assert_eq!(check.manifest_for(&channel), Some(&later));
    // A manifest from another channel replaces whatever was stored.
    let fork = ChannelName::parse("fork-owner/capscode").unwrap();
    let mut fork_manifest = manifest_at(1, "2026-01-01T00:00:00Z", &[]);
    for url in [
        &mut fork_manifest.toolchain.lock_url,
        &mut fork_manifest.toolchain.attestation_url,
        &mut fork_manifest.cli.notes_url,
    ] {
        *url = url.replace(CHANNEL, "fork-owner/capscode");
    }
    assert!(check.remember(&fork, &fork_manifest));
}

#[test]
fn a_stale_manifest_does_not_roll_back_the_stored_yank_list() {
    let home = tempfile::tempdir().unwrap();
    let active = toolchain::builtin_lock().unwrap();
    let codex = format!("codex@{}", active.tools["codex"].version);
    let mut check = CheckState::default();
    assert!(check.remember(
        &channel_name(),
        &manifest_at(1, "2026-10-20T06:12:00Z", &[codex.as_str()])
    ));
    state::write_json(&state::check_state_path(home.path()), &check).unwrap();

    // `capscode update` fetches an older copy without the yank.
    let lock = lock_text(1, |text| text);
    let mut stale = manifest_for(1, &lock);
    stale["generated_at"] = serde_json::json!("2026-10-01T00:00:00Z");
    let fetcher = FakeFetcher::serving(&stale, &lock);
    let report = run(&context(home.path()), &fetcher, apply()).unwrap();
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.contains("was withdrawn")),
        "{:?}",
        report.lines
    );
    let yanks = known_yanks_in(home.path(), Some(&channel_name()), &active);
    assert!(yanks.contains(&codex), "{yanks:?}");

    // The passive check keeps the newer manifest and still reports the withdrawal.
    let mut due = state::read_check_state(&state::check_state_path(home.path()));
    due.last_attempt_unix = 0;
    state::write_json(&state::check_state_path(home.path()), &due).unwrap();
    let mut fetcher = FakeFetcher::default();
    fetcher.bodies.insert(
        channel_name().manifest_url(),
        Ok(serde_json::to_vec(&stale).unwrap()),
    );
    let line = passive::start_with(passive_inputs(home.path()), Arc::new(fetcher))
        .unwrap()
        .with_version("0.1.0")
        .finish()
        .unwrap();
    assert!(line.contains("was withdrawn"), "{line}");
    let stored = state::read_check_state(&state::check_state_path(home.path()));
    assert_eq!(
        stored.manifest_for(&channel_name()).unwrap().generated_at,
        "2026-10-20T06:12:00Z"
    );
}

#[test]
fn a_stale_manifest_cannot_offer_a_toolchain_the_stored_one_withdrew() {
    let home = tempfile::tempdir().unwrap();
    let lock = lock_text(2, |text| bump_codex(text, "0.160.0"));
    let mut check = CheckState::default();
    assert!(check.remember(
        &channel_name(),
        &manifest_at(3, "2026-10-21T00:00:00Z", &["codex@0.160.0"])
    ));
    state::write_json(&state::check_state_path(home.path()), &check).unwrap();
    let fetcher = FakeFetcher::serving(&manifest_for(2, &lock), &lock);
    let error = error_text(run(&context(home.path()), &fetcher, apply()));
    assert!(error.contains("withdrawn codex@0.160.0"), "{error}");
    assert!(!home.path().join("toolchain.lock.toml").exists());
}

// ---- runtime lock superseded by a newer built-in lock -----------------------

/// Apply a channel lock at `sequence` the way `capscode update` does.
fn apply_channel_lock(home: &Path, sequence: u64) -> ResolvedLock {
    let lock = lock_text(sequence, |text| bump_codex(text, "0.160.0"));
    let fetcher = FakeFetcher::serving(&manifest_for(sequence, &lock), &lock);
    run(&context(home), &fetcher, apply()).unwrap();
    toolchain::resolve_with_runtime_path(None, &home.join("toolchain.lock.toml")).unwrap()
}

fn builtin_at(sequence: u64) -> toolchain::ToolchainLock {
    let mut lock = toolchain::builtin_lock().unwrap();
    lock.sequence = Some(sequence);
    lock
}

#[test]
fn a_newer_builtin_lock_supersedes_a_runtime_lock_from_an_older_update() {
    use super::superseded::{decide, supersede_with, Choice};

    let home = tempfile::tempdir().unwrap();
    let runtime = apply_channel_lock(home.path(), 2);
    let digest = toolchain::digest(&runtime.lock);
    assert_eq!(
        decide(&runtime, home.path(), "0.1.0", &builtin_at(2)),
        Choice::Keep
    );
    let Choice::UseBuiltin(line) = decide(&runtime, home.path(), "0.1.0", &builtin_at(3)) else {
        panic!("the older update-written lock was kept");
    };
    assert!(
        line.contains(&format!("capscode toolchain use {digest}")),
        "{line}"
    );

    let (chosen, line) = supersede_with(runtime, home.path(), "0.1.0", builtin_at(3));
    assert!(line.is_some());
    assert_eq!(chosen.lock.sequence, Some(3));
    assert!(chosen.path.is_none());
    assert!(
        !home.path().join("toolchain.lock.toml").exists(),
        "the superseded runtime lock is removed, so the note appears once"
    );
    assert!(home
        .path()
        .join(format!("toolchains/{digest}.lock.toml"))
        .is_file());
}

#[test]
fn a_runtime_lock_the_user_chose_is_kept_with_a_warning() {
    use super::superseded::{decide, supersede_with, Choice};

    let home = tempfile::tempdir().unwrap();
    let runtime = apply_channel_lock(home.path(), 2);
    let runtime_path = home.path().join("toolchain.lock.toml");
    // `toolchain use` rewrites the file, even with the same lock.
    toolchain::write_runtime_lock_at(&runtime_path, &runtime.lock).unwrap();
    let runtime = toolchain::resolve_with_runtime_path(None, &runtime_path).unwrap();
    let choice = decide(&runtime, home.path(), "0.1.0", &builtin_at(3));
    assert!(
        matches!(&choice, Choice::KeepWithWarning(line) if line.contains("you selected it")),
        "{choice:?}"
    );
    let (chosen, line) = supersede_with(runtime, home.path(), "0.1.0", builtin_at(3));
    assert!(line.is_some());
    assert_eq!(chosen.lock.sequence, Some(2));
    assert!(runtime_path.is_file());

    // An untested (pinned) lock is the user's choice too.
    let mut pinned = toolchain::builtin_lock().unwrap();
    pinned.untested = true;
    pinned.sequence = Some(1);
    toolchain::write_runtime_lock_at(&runtime_path, &pinned).unwrap();
    let runtime = toolchain::resolve_with_runtime_path(None, &runtime_path).unwrap();
    assert!(matches!(
        decide(&runtime, home.path(), "0.1.0", &builtin_at(3)),
        Choice::KeepWithWarning(_)
    ));

    // A config override is never second-guessed.
    let elsewhere = home.path().join("mine.lock.toml");
    toolchain::write_runtime_lock_at(&elsewhere, &pinned).unwrap();
    let explicit = toolchain::resolve_with_runtime_path(Some(&elsewhere), &runtime_path).unwrap();
    assert_eq!(
        decide(&explicit, home.path(), "0.1.0", &builtin_at(3)),
        Choice::Keep
    );
}

#[test]
fn a_runtime_lock_replaced_after_resolution_is_the_lock_used() {
    use super::superseded::supersede_with;

    let home = tempfile::tempdir().unwrap();
    let runtime = apply_channel_lock(home.path(), 2);
    let runtime_path = home.path().join("toolchain.lock.toml");
    let mut selected = builtin_at(1);
    selected.untested = true;
    toolchain::write_runtime_lock_at(&runtime_path, &selected).unwrap();

    let (chosen, line) = supersede_with(runtime, home.path(), "0.1.0", builtin_at(3));
    assert_eq!(chosen.lock.sequence, selected.sequence);
    assert!(chosen.lock.untested);
    assert!(line.is_some_and(|line| line.contains("you selected it")));
    assert!(runtime_path.is_file());
}

#[test]
fn a_changed_quarantined_runtime_lock_is_restored_instead_of_deleted() {
    use super::superseded::{finish_retiring_runtime_lock, WrittenRuntimeLock};

    let home = tempfile::tempdir().unwrap();
    let runtime_path = home.path().join("toolchain.lock.toml");
    let update_lock = builtin_at(2);
    toolchain::write_runtime_lock_at(&runtime_path, &update_lock).unwrap();
    let expected = WrittenRuntimeLock::of(&runtime_path, &toolchain::digest(&update_lock)).unwrap();

    let quarantine_path = home.path().join(".toolchain-lock-superseded-test");
    std::fs::rename(&runtime_path, &quarantine_path).unwrap();
    let mut user_lock = builtin_at(1);
    user_lock.untested = true;
    toolchain::write_runtime_lock_at(&quarantine_path, &user_lock).unwrap();

    assert!(!finish_retiring_runtime_lock(&runtime_path, &quarantine_path, &expected).unwrap());
    let restored = toolchain::resolve_with_runtime_path(None, &runtime_path).unwrap();
    assert_eq!(restored.lock.sequence, user_lock.sequence);
    assert!(restored.lock.untested);
    assert!(!quarantine_path.exists());

    toolchain::write_runtime_lock_at(&runtime_path, &update_lock).unwrap();
    let expected = WrittenRuntimeLock::of(&runtime_path, &toolchain::digest(&update_lock)).unwrap();
    std::fs::rename(&runtime_path, &quarantine_path).unwrap();
    let mut newer_choice = builtin_at(3);
    newer_choice.untested = true;
    toolchain::write_runtime_lock_at(&runtime_path, &newer_choice).unwrap();
    assert!(!finish_retiring_runtime_lock(&runtime_path, &quarantine_path, &expected).unwrap());
    let current = toolchain::resolve_with_runtime_path(None, &runtime_path).unwrap();
    assert_eq!(current.lock.sequence, newer_choice.sequence);
    assert!(current.lock.untested);
    assert!(!quarantine_path.exists());
}

#[test]
fn the_channel_minimum_toolchain_sequence_for_this_cli_is_enforced() {
    use super::superseded::{decide, Choice};

    let home = tempfile::tempdir().unwrap();
    let runtime = apply_channel_lock(home.path(), 2);
    let mut check = state::read_check_state(&state::check_state_path(home.path()));
    let mut manifest = check.manifest_for(&channel_name()).unwrap().clone();
    manifest.cli.min_toolchain_sequence = 5;
    manifest.generated_at = "2027-01-01T00:00:00Z".to_owned();
    assert!(check.remember(&channel_name(), &manifest));
    state::write_json(&state::check_state_path(home.path()), &check).unwrap();

    // The built-in lock is no newer: keep the runtime lock, point at `capscode update`.
    let choice = decide(&runtime, home.path(), "0.1.0", &builtin_at(1));
    assert!(
        matches!(&choice, Choice::KeepWithWarning(line) if line.contains("capscode update")),
        "{choice:?}"
    );
    // A built-in lock newer than the runtime lock replaces it.
    assert!(matches!(
        decide(&runtime, home.path(), "0.1.0", &builtin_at(3)),
        Choice::UseBuiltin(_)
    ));
    // The minimum belongs to the CLI release the manifest names, not to this one.
    assert_eq!(
        decide(&runtime, home.path(), "0.0.9", &builtin_at(1)),
        Choice::Keep
    );
}

#[test]
fn the_notice_names_the_update_command_for_this_install() {
    let lock = lock_text(1, |text| text);
    let mut manifest: channel::Channel = serde_json::from_value(manifest_for(1, &lock)).unwrap();
    manifest.cli.version = "0.2.0".to_owned();
    let active = toolchain::builtin_lock().unwrap();
    let digest = toolchain::digest(&active);
    let git = "cd /src/capscode && git pull && ./scripts/install.sh";
    let line = notice_line(&manifest, "0.1.0", git, &digest, &active, 1).unwrap();
    assert!(line.contains(git), "{line}");
    assert!(!line.contains("update --cli"), "{line}");
    manifest.cli.yanked_versions = vec!["0.1.0".to_owned()];
    let line = notice_line(&manifest, "0.1.0", git, &digest, &active, 1).unwrap();
    assert!(line.contains(git), "{line}");

    let home = tempfile::tempdir().unwrap();
    let fetcher = FakeFetcher::serving(&manifest_for(1, &lock), &lock);
    let mut context = context(home.path());
    context.cli_update_command = git.to_owned();
    context.own_version = "0.0.9".to_owned();
    let report = run(&context, &fetcher, apply()).unwrap();
    assert!(
        report.lines.iter().any(|line| line.contains(git)),
        "{:?}",
        report.lines
    );
}
