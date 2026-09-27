//! Small JSON state files under the CapsCode home.
//!
//! `update-check.json` remembers when the passive check last ran and the last
//! validated manifest it saw (for yank lists). `update-state.json` holds the
//! highest toolchain sequence this machine ever accepted, so a replayed older
//! manifest is refused even after `capscode toolchain use built-in`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::channel::{self, Channel, ChannelName};

const MAX_STATE_BYTES: u64 = 256 * 1024;
const MAX_ACCEPTED_RECORDS: usize = 32;

pub(crate) fn check_state_path(home: &Path) -> PathBuf {
    home.join("update-check.json")
}

pub(crate) fn update_state_path(home: &Path) -> PathBuf {
    home.join("update-state.json")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CheckState {
    /// Unix seconds of the last passive check attempt, successful or not.
    pub last_attempt_unix: u64,
    /// Channel the stored manifest came from.
    pub channel: String,
    pub manifest: Option<Channel>,
}

impl CheckState {
    /// The stored manifest, re-validated, if it came from `channel`.
    pub(crate) fn manifest_for(&self, channel: &ChannelName) -> Option<&Channel> {
        let manifest = self.manifest.as_ref()?;
        (self.channel == channel.as_str() && channel::validate(manifest, channel).is_ok())
            .then_some(manifest)
    }

    /// Keep `manifest` unless the stored one from the same channel is newer. A stale
    /// copy served by a cache must not roll back the yank lists, so the stored
    /// manifest is replaced only when neither its toolchain sequence nor its
    /// `generated_at` is ahead of the new one. Returns whether `manifest` was kept.
    pub(crate) fn remember(&mut self, channel: &ChannelName, manifest: &Channel) -> bool {
        if let Some(stored) = self.manifest_for(channel) {
            let newer_or_same = manifest.toolchain.sequence >= stored.toolchain.sequence
                && timestamp_key(&manifest.generated_at) >= timestamp_key(&stored.generated_at);
            if !newer_or_same {
                return false;
            }
        }
        self.channel = channel.to_string();
        self.manifest = Some(manifest.clone());
        true
    }
}

/// A validated `YYYY-MM-DDTHH:MM:SS[.fraction]Z` as a key that sorts in time order:
/// the fraction is padded so `00.5Z` sorts after `00Z`.
fn timestamp_key(value: &str) -> (&str, String) {
    let (seconds, rest) = value.split_at(value.len().min(19));
    let fraction = rest.trim_end_matches('Z').trim_start_matches('.');
    (seconds, format!("{fraction:0<9}"))
}

/// One accepted channel lock, kept so `toolchain show` can print its provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AcceptedToolchain {
    pub sequence: u64,
    pub digest: String,
    pub channel: String,
    pub lock_url: String,
    pub lock_sha256: String,
    pub attestation_url: String,
    pub attestation_sha256: Option<String>,
    pub accepted_unix: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct UpdateState {
    pub highest_accepted_sequence: u64,
    pub accepted: Vec<AcceptedToolchain>,
    /// The runtime lock as `capscode update` last wrote it.
    pub runtime_lock: Option<super::superseded::WrittenRuntimeLock>,
}

impl UpdateState {
    pub(crate) fn record(&mut self, accepted: AcceptedToolchain) {
        self.highest_accepted_sequence = self.highest_accepted_sequence.max(accepted.sequence);
        self.accepted
            .retain(|known| known.digest != accepted.digest);
        self.accepted.push(accepted);
        let excess = self.accepted.len().saturating_sub(MAX_ACCEPTED_RECORDS);
        self.accepted.drain(..excess);
    }

    pub(crate) fn accepted_for(&self, digest: &str) -> Option<&AcceptedToolchain> {
        self.accepted
            .iter()
            .rev()
            .find(|record| record.digest == digest)
    }
}

/// Missing or unreadable state reads as empty: it is a cache, not a control.
pub(crate) fn read_check_state(path: &Path) -> CheckState {
    read_json(path).unwrap_or_default()
}

/// The replay high-water mark is a control, so a corrupt file is an error rather
/// than silently resetting the mark to zero.
pub(crate) fn read_update_state(path: &Path) -> Result<UpdateState> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UpdateState::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        Ok(_) => read_json(path).with_context(|| {
            format!(
                "{} is damaged; remove it only if you trust the channel, since it guards against replayed updates",
                path.display()
            )
        }),
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_STATE_BYTES {
        anyhow::bail!("{} is unexpectedly large", path.display());
    }
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(value).context("serializing update state")?;
    crate::atomic_file::write(path, &bytes, Some(0o600))
        .with_context(|| format!("writing {}", path.display()))
}
