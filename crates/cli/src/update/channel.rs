//! The release channel manifest (`channel.json`), its URLs, and its validation.
//!
//! The manifest is fetched over TLS from GitHub and is otherwise untrusted: every
//! field is type-checked, length-capped, and shape-checked here before anything
//! else in the CLI reads it. The format is specified in `docs/channel-format.md`.

use std::fmt;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::toolchain::{
    is_bounded_semver, is_digest, is_display_text, is_lower_hex, is_utc_timestamp, is_yank_entry,
};

pub(crate) const SCHEMA: u64 = 1;
pub(crate) const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LOCK_BYTES: usize = 256 * 1024;
/// Sequences must stay exact in every JSON implementation (2^53 - 1).
pub(crate) const MAX_JSON_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_URL_LEN: usize = 2048;
const MAX_LIST_ITEMS: usize = 256;
const LOCK_ASSET: &str = "toolchain.lock.toml";
const ATTESTATION_ASSET: &str = "toolchain.lock.toml.sigstore";

/// A validated GitHub `owner/repo` that serves the channel. Every update URL is
/// derived from it, which is what lets a fork take over publishing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelName(String);

impl ChannelName {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let Some((owner, repo)) = value.split_once('/') else {
            bail!("update.channel must be a GitHub `owner/repo`, for example sayre4ux/capscode");
        };
        let owner_ok = !owner.is_empty()
            && owner.len() <= 39
            && !owner.starts_with('-')
            && !owner.ends_with('-')
            && !owner.contains("--")
            && owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        let repo_ok = !repo.is_empty()
            && repo.len() <= 100
            && repo != "."
            && repo != ".."
            && repo
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !owner_ok || !repo_ok {
            bail!("update.channel must be a GitHub `owner/repo`, for example sayre4ux/capscode");
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The manifest lives on a branch only the release workflows write.
    pub(crate) fn manifest_url(&self) -> String {
        format!(
            "https://raw.githubusercontent.com/{}/refs/heads/channel/channel.json",
            self.0
        )
    }

    pub(crate) fn release_asset_url(&self, tag: &str, asset: &str) -> String {
        format!(
            "https://github.com/{}/releases/download/{tag}/{asset}",
            self.0
        )
    }
}

impl fmt::Display for ChannelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `channel.json`. Unknown fields are ignored so a newer manifest within the same
/// schema still parses; every known field is required.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Channel {
    pub schema: u64,
    pub generated_at: String,
    pub cli: CliRelease,
    pub toolchain: ToolchainRelease,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CliRelease {
    pub version: String,
    pub tag: String,
    pub commit: String,
    pub min_toolchain_sequence: u64,
    pub notes_url: String,
    pub yanked_versions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolchainRelease {
    pub sequence: u64,
    pub tag: String,
    pub digest: String,
    pub min_cli_version: String,
    pub lock_url: String,
    pub lock_sha256: String,
    pub attestation_url: String,
    // The toolchain release workflow writes this after attesting the lock. It stays
    // optional so a manifest written before the field existed still parses; the CLI
    // shows the value when a release carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation_sha256: Option<String>,
    pub yanked: Vec<String>,
}

/// Parse and validate manifest bytes for `channel`.
pub(crate) fn parse(bytes: &[u8], channel: &ChannelName) -> Result<Channel> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        bail!("the channel manifest is larger than {MAX_MANIFEST_BYTES} bytes");
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("the channel manifest is not valid JSON")?;
    // Check the schema before the typed parse so a future manifest gets an
    // "update CapsCode" message rather than a field error.
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(SCHEMA) => {}
        Some(other) => bail!(
            "the channel uses manifest schema {other}, which this CLI does not support; update CapsCode"
        ),
        None => bail!("the channel manifest has no integer `schema`"),
    }
    let manifest: Channel = serde_json::from_value(value)
        .context("the channel manifest has missing or mistyped fields")?;
    validate(&manifest, channel)?;
    Ok(manifest)
}

pub(crate) fn validate(manifest: &Channel, channel: &ChannelName) -> Result<()> {
    if manifest.schema != SCHEMA {
        bail!("the channel manifest schema must be {SCHEMA}");
    }
    if !is_utc_timestamp(&manifest.generated_at) {
        bail!("the channel manifest generated_at is not a UTC timestamp");
    }
    validate_cli(&manifest.cli, channel)?;
    validate_toolchain(&manifest.toolchain, channel)
}

fn validate_cli(cli: &CliRelease, channel: &ChannelName) -> Result<()> {
    if Version::parse(&cli.version).is_err() {
        bail!("the channel CLI version is not SemVer");
    }
    if cli.tag != format!("v{}", cli.version) {
        bail!("the channel CLI tag must be v<version>");
    }
    if !(7..=64).contains(&cli.commit.len()) || !is_lower_hex(&cli.commit, cli.commit.len()) {
        bail!("the channel CLI commit must be 7-64 lowercase hex characters");
    }
    check_sequence("cli.min_toolchain_sequence", cli.min_toolchain_sequence)?;
    let notes_prefix = format!("https://github.com/{channel}/");
    if !is_safe_url(&cli.notes_url) || !starts_with_ignore_case(&cli.notes_url, &notes_prefix) {
        bail!("the channel CLI notes_url must be an HTTPS link under {notes_prefix}");
    }
    if cli.yanked_versions.len() > MAX_LIST_ITEMS
        || cli
            .yanked_versions
            .iter()
            .any(|version| Version::parse(version).is_err())
    {
        bail!("the channel cli.yanked_versions must be SemVer versions");
    }
    Ok(())
}

fn validate_toolchain(toolchain: &ToolchainRelease, channel: &ChannelName) -> Result<()> {
    check_sequence("toolchain.sequence", toolchain.sequence)?;
    if toolchain.tag != format!("toolchain-{}", toolchain.sequence) {
        bail!("the channel toolchain tag must be toolchain-<sequence>");
    }
    if !is_digest(&toolchain.digest) {
        bail!("the channel toolchain digest must be 12 lowercase hex characters");
    }
    if Version::parse(&toolchain.min_cli_version).is_err() {
        bail!("the channel toolchain min_cli_version is not SemVer");
    }
    let lock_url = channel.release_asset_url(&toolchain.tag, LOCK_ASSET);
    if !is_safe_url(&toolchain.lock_url) || !toolchain.lock_url.eq_ignore_ascii_case(&lock_url) {
        bail!("the channel lock_url must be {lock_url}");
    }
    if !is_lower_hex(&toolchain.lock_sha256, 64) {
        bail!("the channel lock_sha256 must be 64 lowercase hex characters");
    }
    let attestation_url = channel.release_asset_url(&toolchain.tag, ATTESTATION_ASSET);
    if !is_safe_url(&toolchain.attestation_url)
        || !toolchain
            .attestation_url
            .eq_ignore_ascii_case(&attestation_url)
    {
        bail!("the channel attestation_url must be {attestation_url}");
    }
    if let Some(sha) = &toolchain.attestation_sha256 {
        if !is_lower_hex(sha, 64) {
            bail!("the channel attestation_sha256 must be 64 lowercase hex characters");
        }
    }
    let builtin = crate::toolchain::builtin_lock()?;
    if toolchain.yanked.len() > MAX_LIST_ITEMS
        || toolchain
            .yanked
            .iter()
            .any(|entry| !is_yank_entry(entry, builtin.tools.keys().map(String::as_str)))
    {
        bail!("the channel toolchain.yanked entries must be `tool@version` or toolchain digests");
    }
    Ok(())
}

fn check_sequence(field: &str, value: u64) -> Result<()> {
    if value == 0 || value > MAX_JSON_INTEGER {
        bail!("the channel {field} must be between 1 and {MAX_JSON_INTEGER}");
    }
    Ok(())
}

/// HTTPS, bounded, and made only of visible ASCII so it can be printed safely.
fn is_safe_url(value: &str) -> bool {
    value.len() <= MAX_URL_LEN
        && value.starts_with("https://")
        && is_display_text(value, MAX_URL_LEN)
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// A SemVer 2.0.0 version, held in the form its precedence is computed from.
///
/// Every numeric part must fit in a `u64`. A version that does not parse cannot be
/// ordered, and callers treat that as a refusal: no decision about a minimum
/// version or a downgrade may rest on a value this CLI cannot compare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    core: [u64; 3],
    pre: Vec<PreRelease>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreRelease {
    Numeric(u64),
    Text(String),
}

impl Version {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let unsupported = || {
            anyhow::anyhow!(
                "`{}` is not a SemVer version CapsCode can compare",
                shown(value)
            )
        };
        if !is_bounded_semver(value) {
            return Err(unsupported());
        }
        let without_build = value.split('+').next().unwrap_or(value);
        let (core, pre) = match without_build.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (without_build, None),
        };
        let mut numbers = [0u64; 3];
        let mut parts = core.split('.');
        for slot in &mut numbers {
            *slot = parts
                .next()
                .and_then(|part| part.parse::<u64>().ok())
                .ok_or_else(unsupported)?;
        }
        if parts.next().is_some() {
            return Err(unsupported());
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|part| {
                    if part.bytes().all(|byte| byte.is_ascii_digit()) {
                        // SemVer orders numeric identifiers by value, so one that
                        // does not fit cannot be ordered at all.
                        part.parse::<u64>().map(PreRelease::Numeric).ok()
                    } else {
                        Some(PreRelease::Text(part.to_owned()))
                    }
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(unsupported)?,
        };
        Ok(Self { core: numbers, pre })
    }

    /// SemVer precedence; build metadata was dropped when parsing.
    pub(crate) fn precedence(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;

        match self.core.cmp(&other.core) {
            Ordering::Equal => {}
            other => return other,
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (left, right) in self.pre.iter().zip(&other.pre) {
            let ordering = match (left, right) {
                (PreRelease::Numeric(a), PreRelease::Numeric(b)) => a.cmp(b),
                (PreRelease::Numeric(_), PreRelease::Text(_)) => Ordering::Less,
                (PreRelease::Text(_), PreRelease::Numeric(_)) => Ordering::Greater,
                (PreRelease::Text(a), PreRelease::Text(b)) => a.cmp(b),
            };
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}

/// A bounded, printable rendering of an untrusted value for an error message.
fn shown(value: &str) -> String {
    value
        .chars()
        .take(80)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

/// SemVer 2.0.0 precedence of two version strings. Fails when either one does not
/// parse; every caller turns that failure into a refusal.
pub(crate) fn compare_versions(left: &str, right: &str) -> Result<std::cmp::Ordering> {
    Ok(Version::parse(left)?.precedence(&Version::parse(right)?))
}
