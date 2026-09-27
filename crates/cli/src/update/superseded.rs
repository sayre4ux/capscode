//! A runtime lock that an older CLI's `capscode update` left behind.
//!
//! `~/.capscode/toolchain.lock.toml` wins over the lock built into the binary. That
//! is right while the runtime lock is newer, but after `capscode update --cli`
//! installs a CLI whose built-in lock has a higher release sequence, the older
//! channel lock would keep winning. This module notices that case and prefers the
//! built-in lock, but only for a runtime lock that `capscode update` wrote and that
//! nothing has rewritten since: a lock the user picked with `toolchain use` or
//! `toolchain pin` is kept, with a warning.
//
// DECISION: "written by `capscode update`" is recorded as the file's size, mtime,
// and inode right after the update writes it. `toolchain use` and `toolchain pin`
// replace the file atomically (new inode), so any later choice by the user no
// longer matches and is kept. Any doubt keeps the user's lock.
// DECISION: superseding removes the update-written runtime lock (what
// `capscode toolchain use built-in` does), so the note appears once rather than on
// every command. The lock stays in `~/.capscode/toolchains/` for `toolchain use`.

use std::path::Path;
use std::sync::Once;

use serde::{Deserialize, Serialize};

use super::state::{self, UpdateState};
use crate::toolchain::{self, ResolvedLock, ToolchainLock};

/// Identity of the runtime lock file as `capscode update` left it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct WrittenRuntimeLock {
    pub digest: String,
    pub len: u64,
    pub modified_secs: u64,
    pub modified_nanos: u32,
    pub inode: u64,
}

impl WrittenRuntimeLock {
    /// The current identity of `path`, or `None` when it cannot be read.
    pub(crate) fn of(path: &Path, digest: &str) -> Option<Self> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
        #[cfg(not(unix))]
        let inode = 0;
        Some(Self {
            digest: digest.to_owned(),
            len: metadata.len(),
            modified_secs: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
            inode,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    /// The runtime lock is current enough.
    Keep,
    /// The runtime lock is older, but the user chose it (or it cannot be replaced
    /// by the built-in lock); keep it and say so.
    KeepWithWarning(String),
    /// The runtime lock came from `capscode update` for an older CLI.
    UseBuiltin(String),
}

/// Decide what to do about `resolved`, which came from the runtime lock in `home`.
pub(crate) fn decide(
    resolved: &ResolvedLock,
    home: &Path,
    own_version: &str,
    builtin: &ToolchainLock,
) -> Choice {
    let runtime_path = home.join("toolchain.lock.toml");
    if resolved.path.as_deref() != Some(runtime_path.as_path()) {
        // A config override or the built-in lock itself.
        return Choice::Keep;
    }
    let Ok(update_state) = state::read_update_state(&state::update_state_path(home)) else {
        return Choice::Keep;
    };
    let runtime = &resolved.lock;
    let runtime_sequence = runtime.sequence.unwrap_or(0);
    let builtin_sequence = builtin.sequence.unwrap_or(0);
    let channel_floor = channel_floor(home, own_version);
    let floor = builtin_sequence.max(channel_floor.unwrap_or(0));
    if runtime_sequence >= floor {
        return Choice::Keep;
    }
    let digest = toolchain::digest(runtime);
    if !runtime.untested
        && written_by_update(&update_state, &runtime_path, &digest)
        && builtin_sequence > runtime_sequence
    {
        return Choice::UseBuiltin(format!(
            "Using the toolchain built into this CapsCode (sequence {builtin_sequence}) instead of toolchain {digest} (sequence {runtime_sequence}), which `capscode update` installed for an older CapsCode. Switch back with `capscode toolchain use {digest}`."
        ));
    }
    if builtin_sequence > runtime_sequence {
        return Choice::KeepWithWarning(format!(
            "warning: toolchain {digest} (sequence {runtime_sequence}) is older than the one built into this CapsCode (sequence {builtin_sequence}); keeping it because you selected it. Switch with `capscode toolchain use built-in`."
        ));
    }
    Choice::KeepWithWarning(format!(
        "warning: toolchain {digest} (sequence {runtime_sequence}) is older than CapsCode {own_version} expects (sequence {floor}); run `capscode update`."
    ))
}

/// `resolved`, or the built-in lock when the runtime lock is a stale one that
/// `capscode update` wrote. Prints at most one note per process.
pub(crate) fn supersede(resolved: ResolvedLock, home: &Path, own_version: &str) -> ResolvedLock {
    let Ok(builtin) = toolchain::builtin_lock() else {
        return resolved;
    };
    let (resolved, line) = supersede_with(resolved, home, own_version, builtin);
    if let Some(line) = line {
        static NOTED: Once = Once::new();
        NOTED.call_once(|| crate::ui::note(line));
    }
    resolved
}

pub(crate) fn supersede_with(
    resolved: ResolvedLock,
    home: &Path,
    own_version: &str,
    builtin: ToolchainLock,
) -> (ResolvedLock, Option<String>) {
    match decide(&resolved, home, own_version, &builtin) {
        Choice::Keep => (resolved, None),
        Choice::KeepWithWarning(_) => {
            let runtime_path = home.join("toolchain.lock.toml");
            let current =
                toolchain::resolve_with_runtime_path(None, &runtime_path).unwrap_or(resolved);
            let refreshed_line = match decide(&current, home, own_version, &builtin) {
                Choice::Keep => None,
                Choice::KeepWithWarning(line) => Some(line),
                Choice::UseBuiltin(_) => Some(changed_runtime_lock_warning(&current)),
            };
            (current, refreshed_line)
        }
        Choice::UseBuiltin(line) => {
            let runtime_path = home.join("toolchain.lock.toml");
            let digest = toolchain::digest(&resolved.lock);
            let expected = state::read_update_state(&state::update_state_path(home))
                .ok()
                .and_then(|update_state| update_state.runtime_lock)
                .filter(|written| written.digest == digest);
            if let Some(expected) = expected {
                if retire_runtime_lock(&runtime_path, &expected).unwrap_or(false) {
                    return (
                        ResolvedLock {
                            lock: builtin,
                            source: "built-in".to_owned(),
                            path: None,
                        },
                        Some(line),
                    );
                }
            }
            // DECISION: if the lock changes while it is being retired, use the
            // current file and leave any moved file in place when it cannot be
            // safely restored over a newer choice.
            let current =
                toolchain::resolve_with_runtime_path(None, &runtime_path).unwrap_or(resolved);
            let warning = changed_runtime_lock_warning(&current);
            (current, Some(warning))
        }
    }
}

fn changed_runtime_lock_warning(resolved: &ResolvedLock) -> String {
    format!(
        "warning: the runtime toolchain lock changed while selecting a toolchain; keeping toolchain {}. Run `capscode toolchain use built-in` to choose the built-in lock.",
        toolchain::digest(&resolved.lock)
    )
}

fn retire_runtime_lock(
    runtime_path: &Path,
    expected: &WrittenRuntimeLock,
) -> std::io::Result<bool> {
    if WrittenRuntimeLock::of(runtime_path, &expected.digest).as_ref() != Some(expected) {
        return Ok(false);
    }
    let parent = runtime_path.parent().unwrap_or_else(|| Path::new("."));
    let (placeholder, quarantine_path) = tempfile::Builder::new()
        .prefix(".toolchain-lock-superseded-")
        .tempfile_in(parent)?
        .keep()
        .map_err(|error| error.error)?;
    drop(placeholder);

    if let Err(error) = std::fs::rename(runtime_path, &quarantine_path) {
        let _ = std::fs::remove_file(&quarantine_path);
        if error.kind() == std::io::ErrorKind::NotFound && !runtime_path_may_exist(runtime_path) {
            return Ok(true);
        }
        return Err(error);
    }
    finish_retiring_runtime_lock(runtime_path, &quarantine_path, expected)
}

pub(super) fn finish_retiring_runtime_lock(
    runtime_path: &Path,
    quarantine_path: &Path,
    expected: &WrittenRuntimeLock,
) -> std::io::Result<bool> {
    if WrittenRuntimeLock::of(quarantine_path, &expected.digest).as_ref() == Some(expected) {
        let replacement_arrived = runtime_path_may_exist(runtime_path);
        match std::fs::remove_file(quarantine_path) {
            Ok(()) => {
                return Ok(!replacement_arrived && !runtime_path_may_exist(runtime_path));
            }
            Err(error) => {
                if !runtime_path_may_exist(runtime_path) {
                    let _ = std::fs::rename(quarantine_path, runtime_path);
                }
                return Err(error);
            }
        }
    }

    if !runtime_path_may_exist(runtime_path) {
        std::fs::rename(quarantine_path, runtime_path)?;
    }
    Ok(false)
}

fn runtime_path_may_exist(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

fn written_by_update(update_state: &UpdateState, runtime_path: &Path, digest: &str) -> bool {
    update_state.runtime_lock.as_ref().is_some_and(|written| {
        written.digest == digest
            && WrittenRuntimeLock::of(runtime_path, digest).as_ref() == Some(written)
    })
}

/// `cli.min_toolchain_sequence` from the stored manifest, when it describes this
/// CLI release.
fn channel_floor(home: &Path, own_version: &str) -> Option<u64> {
    let check = state::read_check_state(&state::check_state_path(home));
    let channel = super::channel::ChannelName::parse(&check.channel).ok()?;
    let manifest = check.manifest_for(&channel)?;
    (manifest.cli.version == own_version).then_some(manifest.cli.min_toolchain_sequence)
}
