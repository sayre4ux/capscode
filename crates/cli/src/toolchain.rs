//! Validated toolchain locks and the small file API used by CLI commands.
//!
//! Locks are untrusted input: validation happens here before a version or image
//! reference can reach a shell or the container runtime.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use capscode_core::Config;
use serde::{Deserialize, Serialize};

const BUILTIN_LOCK: &str = include_str!("../../../images/toolchain.lock.toml");
const LOCK_SCHEMA: u32 = 1;
const BUILTIN_SOURCE: &str = "built-in";
// Bump when the CLI and capsule entrypoint/keeper environment contract changes.
pub(crate) const IMAGE_CONTRACT_VERSION: u64 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ToolchainLock {
    pub schema: u32,
    /// Release sequence of a published toolchain; strictly increasing per release.
    pub sequence: Option<u64>,
    /// Oldest CLI version allowed to consume this lock.
    pub min_cli_version: Option<String>,
    /// Withdrawn `tool@version` entries or 12-character toolchain digests.
    pub yanked: Vec<String>,
    pub untested: bool,
    pub base: BTreeMap<String, toml::Value>,
    pub seeder: LockedImage,
    pub tools: BTreeMap<String, LockedTool>,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

impl Default for ToolchainLock {
    fn default() -> Self {
        Self {
            schema: LOCK_SCHEMA,
            sequence: None,
            min_cli_version: None,
            yanked: Vec::new(),
            untested: false,
            base: BTreeMap::new(),
            seeder: LockedImage::default(),
            tools: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct LockedImage {
    pub image: String,
    pub digest: String,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct LockedTool {
    pub package: String,
    pub version: String,
    pub integrity: String,
    pub resolved_before: String,
    pub channel: Option<String>,
    pub published_at: Option<String>,
    pub smoke: SmokeGrade,
    pub smoke_date: Option<String>,
    pub smoke_arch: Vec<String>,
    pub cooldown_waived: Option<String>,
    /// Why a release deliberately moves this tool to an older version.
    pub downgrade_reason: Option<String>,
    pub home_exec_paths: Vec<String>,
    pub daemon_seed_layout: Option<u64>,
    pub needs_exec_tmpdir: Option<bool>,
    pub path_tools: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SmokeGrade {
    #[default]
    None,
    Ci,
    Session,
}

impl std::fmt::Display for SmokeGrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Ci => "ci",
            Self::Session => "session",
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedLock {
    pub lock: ToolchainLock,
    pub source: String,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub digest: String,
    pub path: PathBuf,
    pub lock: ToolchainLock,
    pub modified: Option<SystemTime>,
}

pub(crate) fn config_home() -> PathBuf {
    Config::config_home()
}

pub(crate) fn runtime_lock_path() -> PathBuf {
    config_home().join("toolchain.lock.toml")
}

pub(crate) fn snapshots_dir() -> PathBuf {
    config_home().join("toolchains")
}

/// Config override wins; otherwise load the runtime lock if it exists, then the embedded lock.
pub(crate) fn resolve(config_override: Option<&Path>) -> Result<ResolvedLock> {
    // A newer CLI ships a newer built-in lock; a runtime lock left by an older
    // `capscode update` must not keep winning over it. Locks the user chose are kept.
    resolve_with_runtime_path(config_override, &runtime_lock_path()).map(|resolved| {
        crate::update::superseded::supersede(resolved, &config_home(), crate::update::own_version())
    })
}

pub(crate) fn resolve_with_runtime_path(
    config_override: Option<&Path>,
    runtime_path: &Path,
) -> Result<ResolvedLock> {
    if let Some(path) = config_override {
        let lock = read_validated(path)?;
        return Ok(ResolvedLock {
            lock,
            source: path.display().to_string(),
            path: Some(path.to_path_buf()),
        });
    }

    match fs::read_to_string(runtime_path) {
        Ok(contents) => {
            let lock = parse_validated(&contents).with_context(|| {
                format!(
                    "runtime toolchain lock {} is invalid; repair or remove it with `capscode toolchain use built-in`",
                    runtime_path.display()
                )
            })?;
            Ok(ResolvedLock {
                lock,
                source: runtime_path.display().to_string(),
                path: Some(runtime_path.to_path_buf()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ResolvedLock {
            lock: builtin_lock()?,
            source: BUILTIN_SOURCE.to_owned(),
            path: None,
        }),
        Err(error) => Err(error)
            .with_context(|| format!("reading runtime toolchain lock {}", runtime_path.display())),
    }
}

pub(crate) fn read_validated(path: &Path) -> Result<ToolchainLock> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading toolchain lock {}", path.display()))?;
    parse_validated(&contents)
        .with_context(|| format!("validating toolchain lock {}", path.display()))
}

pub(crate) fn parse_validated(contents: &str) -> Result<ToolchainLock> {
    let value: toml::Value = toml::from_str(contents).context("parsing toolchain lock TOML")?;
    if !value.get("schema").is_some_and(toml::Value::is_integer) {
        bail!("toolchain lock must declare an integer `schema` version");
    }
    let lock: ToolchainLock = value.try_into().context("parsing toolchain lock fields")?;
    validate_lock(&lock, true)?;
    Ok(lock)
}

pub(crate) fn builtin_lock() -> Result<ToolchainLock> {
    let lock: ToolchainLock =
        toml::from_str(BUILTIN_LOCK).context("parsing built-in toolchain lock")?;
    validate_lock(&lock, false)?;
    Ok(lock)
}

/// The base-image fields a lock may carry, with the only Docker Hub repository each
/// may name. A lock arrives as data from the release channel; letting it name any
/// other repository would make the user's Docker pull and run an image nobody
/// reviewed. The seeder image is pulled by the CLI itself, so it gets the same rule.
const BASE_IMAGES: [(&str, &str, &str); 3] = [
    ("image", "digest", "debian"),
    ("node_image", "node_digest", "node"),
    ("bun_image", "bun_digest", "oven/bun"),
];
const BASE_VERSIONS: [&str; 2] = ["node_version", "bun_version"];
const SEEDER_REPOSITORY: &str = "alpine";

/// Bounds for lock values. They keep every value short and printable before it can
/// reach a Dockerfile `ARG`, a shell script, or the terminal.
const MAX_LIST_ITEMS: usize = 256;
const MAX_VERSION_LEN: usize = 64;
const MAX_PACKAGE_LEN: usize = 214;
const MAX_INTEGRITY_LEN: usize = 128;
const MAX_TIMESTAMP_LEN: usize = 32;
const MAX_REASON_LEN: usize = 512;
const MAX_IMAGE_LEN: usize = 255;
const MAX_YANK_LEN: usize = 160;
const MAX_EXEC_PATH_LEN: usize = 1024;
const MAX_PATH_TOOLS: usize = 32;

fn validate_lock(lock: &ToolchainLock, compare_packages: bool) -> Result<()> {
    if lock.schema > LOCK_SCHEMA {
        bail!(
            "toolchain lock schema {} is newer than this CLI supports (schema {LOCK_SCHEMA}); update CapsCode",
            lock.schema
        );
    }
    if lock.schema != LOCK_SCHEMA {
        bail!(
            "unsupported toolchain lock schema {}; expected {LOCK_SCHEMA}",
            lock.schema
        );
    }
    let builtin = if compare_packages {
        Some(builtin_lock()?)
    } else {
        None
    };
    if lock.tools.is_empty() {
        bail!("toolchain lock has no [tools] entries");
    }
    if let Some(builtin) = &builtin {
        for name in lock.tools.keys() {
            if !builtin.tools.contains_key(name) {
                bail!("toolchain lock contains unknown tool '{name}'");
            }
        }
        for name in builtin.tools.keys() {
            if !lock.tools.contains_key(name) {
                bail!("toolchain lock is missing the known tool '{name}'");
            }
        }
    }
    if lock.sequence == Some(0) {
        bail!("toolchain lock sequence must be at least 1");
    }
    if let Some(version) = &lock.min_cli_version {
        if !is_bounded_semver(version) {
            bail!("toolchain lock min_cli_version is not valid SemVer");
        }
    }
    if lock.yanked.len() > MAX_LIST_ITEMS {
        bail!("toolchain lock lists more than {MAX_LIST_ITEMS} yanked entries");
    }
    for entry in &lock.yanked {
        if !is_yank_entry(entry, lock.tools.keys().map(String::as_str)) {
            bail!("toolchain lock yanked entry is not `tool@version` or a toolchain digest");
        }
    }
    validate_extra_keys("toolchain lock", &lock.extra)?;
    validate_base(&lock.base)?;
    validate_seeder(&lock.seeder)?;
    for (name, tool) in &lock.tools {
        if !is_identifier(name) {
            bail!("toolchain lock tool names must be lowercase identifiers");
        }
        if let Some(expected) = builtin.as_ref().and_then(|builtin| builtin.tools.get(name)) {
            if tool.package != expected.package {
                bail!(
                    "toolchain lock package for '{name}' must be '{}'",
                    expected.package
                );
            }
        }
        if tool.package.is_empty() {
            bail!("toolchain lock package for '{name}' is empty");
        }
        if !is_npm_package_name(&tool.package) {
            bail!("toolchain lock package for '{name}' is not a valid npm package name");
        }
        if !is_bounded_semver(&tool.version) {
            bail!("toolchain lock version for '{name}' is not valid SemVer");
        }
        if tool.integrity.len() > MAX_INTEGRITY_LEN {
            bail!("toolchain lock integrity for '{name}' is too long");
        }
        validate_integrity(name, &tool.integrity)?;
        if tool.resolved_before.is_empty() {
            bail!("toolchain lock resolved_before for '{name}' is empty");
        }
        if !is_utc_timestamp(&tool.resolved_before) {
            bail!("toolchain lock resolved_before for '{name}' must be a UTC timestamp such as 2026-10-01T04:51:24Z");
        }
        validate_tool_metadata(name, tool)?;
    }
    Ok(())
}

fn validate_tool_metadata(name: &str, tool: &LockedTool) -> Result<()> {
    if let Some(channel) = &tool.channel {
        if channel.is_empty()
            || channel.len() > 64
            || !channel
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("toolchain lock channel for '{name}' must be an npm dist-tag");
        }
    }
    if let Some(published) = tool
        .published_at
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        if !is_utc_timestamp(published) {
            bail!("toolchain lock published_at for '{name}' must be a UTC timestamp");
        }
    }
    if let Some(date) = tool.smoke_date.as_deref().filter(|value| !value.is_empty()) {
        if !is_calendar_date(date) {
            bail!("toolchain lock smoke_date for '{name}' must be YYYY-MM-DD");
        }
    }
    if tool.smoke_arch.len() > 2
        || tool
            .smoke_arch
            .iter()
            .any(|arch| !matches!(arch.as_str(), "amd64" | "arm64"))
    {
        bail!("toolchain lock smoke_arch for '{name}' may list only amd64 and arm64");
    }
    for (field, reason) in [
        ("cooldown_waived", &tool.cooldown_waived),
        ("downgrade_reason", &tool.downgrade_reason),
    ] {
        if let Some(reason) = reason {
            if !is_display_text(reason, MAX_REASON_LEN) {
                bail!("toolchain lock {field} for '{name}' must be printable text of at most {MAX_REASON_LEN} characters");
            }
        }
    }
    if tool.home_exec_paths.len() > MAX_LIST_ITEMS
        || tool.home_exec_paths.iter().any(|path| {
            !path.starts_with('/')
                || path.len() > MAX_EXEC_PATH_LEN
                || !path
                    .chars()
                    .all(|ch| ch.is_ascii_graphic() && !matches!(ch, '"' | '\\' | '`' | '$'))
        })
    {
        bail!("toolchain lock home_exec_paths for '{name}' must be absolute path patterns");
    }
    if tool.path_tools.len() > MAX_PATH_TOOLS
        || tool.path_tools.iter().any(|tool_name| {
            tool_name.is_empty()
                || tool_name.len() > 64
                || !tool_name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
    {
        bail!("toolchain lock path_tools for '{name}' must be plain executable names");
    }
    if tool.daemon_seed_layout == Some(0) {
        bail!("toolchain lock daemon_seed_layout for '{name}' must be positive");
    }
    validate_extra_keys(&format!("toolchain lock entry '{name}'"), &tool.extra)
}

fn validate_base(base: &BTreeMap<String, toml::Value>) -> Result<()> {
    let known: Vec<&str> = BASE_IMAGES
        .iter()
        .flat_map(|(image, digest, _)| [*image, *digest])
        .chain(BASE_VERSIONS)
        .collect();
    if let Some(unknown) = base.keys().find(|key| !known.contains(&key.as_str())) {
        bail!(
            "toolchain lock [base] has unknown key '{}'",
            printable(unknown)
        );
    }
    let text = |key: &str| -> Result<&str> {
        base.get(key)
            .with_context(|| format!("toolchain lock [base] is missing {key}"))?
            .as_str()
            .with_context(|| format!("toolchain lock [base].{key} must be a string"))
    };
    for (image_key, digest_key, repository) in BASE_IMAGES {
        validate_image_reference(&format!("[base].{image_key}"), text(image_key)?, repository)?;
        if !is_image_digest(text(digest_key)?) {
            bail!("toolchain lock [base].{digest_key} must be sha256: followed by 64 lowercase hex characters");
        }
    }
    for key in BASE_VERSIONS {
        if !is_bounded_semver(text(key)?) {
            bail!("toolchain lock [base].{key} is not valid SemVer");
        }
    }
    Ok(())
}

fn validate_seeder(seeder: &LockedImage) -> Result<()> {
    validate_image_reference("[seeder].image", &seeder.image, SEEDER_REPOSITORY)?;
    if !is_image_digest(&seeder.digest) {
        bail!("toolchain lock [seeder].digest must be sha256: followed by 64 lowercase hex characters");
    }
    if let Some(unknown) = seeder.extra.keys().next() {
        bail!(
            "toolchain lock [seeder] has unknown key '{}'",
            printable(unknown)
        );
    }
    Ok(())
}

fn validate_image_reference(field: &str, value: &str, repository: &str) -> Result<()> {
    let Some((name, tag)) = value.rsplit_once(':') else {
        bail!("toolchain lock {field} must be '{repository}:<tag>'");
    };
    if name != repository {
        bail!("toolchain lock {field} must use the '{repository}' image repository");
    }
    let tag_ok = !tag.is_empty()
        && tag.len() <= 128
        && tag
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !tag_ok || value.len() > MAX_IMAGE_LEN {
        bail!("toolchain lock {field} has an invalid image tag");
    }
    Ok(())
}

/// Keys this CLI does not know are tolerated for forward compatibility, but only as
/// plain identifiers with printable values. Their values are never used; the check
/// keeps a rewritten lock free of line breaks that a line-based reader could misparse.
fn validate_extra_keys(scope: &str, extra: &BTreeMap<String, toml::Value>) -> Result<()> {
    fn printable_value(value: &toml::Value, depth: usize) -> bool {
        match value {
            toml::Value::String(text) => is_display_text(text, MAX_EXEC_PATH_LEN),
            toml::Value::Array(items) => {
                depth < 4
                    && items.len() <= MAX_LIST_ITEMS
                    && items.iter().all(|item| printable_value(item, depth + 1))
            }
            toml::Value::Table(table) => {
                depth < 4
                    && table.len() <= MAX_LIST_ITEMS
                    && table
                        .iter()
                        .all(|(key, item)| is_identifier(key) && printable_value(item, depth + 1))
            }
            _ => true,
        }
    }
    if let Some(key) = extra.keys().find(|key| !is_identifier(key)) {
        bail!("{scope} has an invalid key '{}'", printable(key));
    }
    if let Some((key, _)) = extra.iter().find(|(_, value)| !printable_value(value, 0)) {
        bail!("{scope} key '{key}' must hold short printable values");
    }
    Ok(())
}

fn printable(value: &str) -> String {
    value
        .chars()
        .take(64)
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_npm_package_name(value: &str) -> bool {
    value.len() <= MAX_PACKAGE_LEN
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'@' | b'/' | b'.' | b'_' | b'-')
        })
}

pub(crate) fn is_bounded_semver(value: &str) -> bool {
    value.len() <= MAX_VERSION_LEN && is_semver(value)
}

pub(crate) fn is_image_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| is_lower_hex(hex, 64))
}

pub(crate) fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `YYYY-MM-DDTHH:MM:SS` with optional fractional seconds and a literal `Z`.
pub(crate) fn is_utc_timestamp(value: &str) -> bool {
    if value.len() > MAX_TIMESTAMP_LEN || !value.ends_with('Z') || value.len() < 20 {
        return false;
    }
    let bytes = value.as_bytes();
    let (date, rest) = value.split_at(10);
    if !is_calendar_date(date) || bytes[10] != b'T' {
        return false;
    }
    let time = &rest[1..rest.len() - 1];
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };
    let clock: Vec<&str> = clock.split(':').collect();
    let clock_ok = clock.len() == 3
        && clock
            .iter()
            .all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
        && clock[0] < "24"
        && clock[1] < "60"
        && clock[2] < "61";
    let fraction_ok = fraction.is_none_or(|digits| {
        (1..=9).contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    clock_ok && fraction_ok
}

pub(crate) fn is_calendar_date(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }
    let month: u32 = parts[1].parse().unwrap_or(0);
    let day: u32 = parts[2].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Printable text without control characters (no newlines or terminal escapes).
pub(crate) fn is_display_text(value: &str, max_chars: usize) -> bool {
    value.chars().count() <= max_chars && !value.chars().any(char::is_control)
}

/// A yank entry is `tool@version` for one withdrawn tool release, or the 12-character
/// digest of a withdrawn toolchain.
pub(crate) fn is_yank_entry<'a>(value: &str, mut tools: impl Iterator<Item = &'a str>) -> bool {
    if value.len() > MAX_YANK_LEN {
        return false;
    }
    if is_digest(value) {
        return true;
    }
    value.split_once('@').is_some_and(|(tool, version)| {
        tools.any(|known| known == tool) && is_bounded_semver(version)
    })
}

/// Whether a lock is withdrawn by any entry: its digest, or one of its tool versions.
pub(crate) fn yanked_by<'a>(
    lock_digest: &str,
    lock: Option<&ToolchainLock>,
    entries: impl IntoIterator<Item = &'a String>,
) -> Option<String> {
    entries.into_iter().find_map(|entry| {
        if entry == lock_digest {
            return Some(format!("toolchain {lock_digest}"));
        }
        let (tool, version) = entry.split_once('@')?;
        lock.and_then(|lock| lock.tools.get(tool))
            .filter(|locked| locked.version == version)
            .map(|_| entry.clone())
    })
}

fn validate_integrity(name: &str, integrity: &str) -> Result<()> {
    let encoded = integrity.strip_prefix("sha512-").ok_or_else(|| {
        anyhow::anyhow!("toolchain lock integrity for '{name}' must use sha512- base64")
    })?;
    let decoded = STANDARD.decode(encoded).map_err(|_| {
        anyhow::anyhow!("toolchain lock integrity for '{name}' is not valid sha512 base64")
    })?;
    if decoded.len() != 64 || STANDARD.encode(&decoded) != encoded {
        bail!("toolchain lock integrity for '{name}' is not a 64-byte sha512 digest");
    }
    Ok(())
}

pub(crate) fn validate_integrity_for_tool(name: &str, integrity: &str) -> Result<()> {
    validate_integrity(name, integrity)
}

/// Validate a SemVer 2.0.0 version without adding another dependency to the CLI.
pub(crate) fn is_semver(value: &str) -> bool {
    let (without_build, build) = match value.split_once('+') {
        Some((left, right)) if !right.is_empty() => (left, Some(right)),
        Some(_) => return false,
        None => (value, None),
    };
    if build.is_some_and(|part| !valid_identifiers(part, false)) {
        return false;
    }
    let (core, prerelease) = match without_build.split_once('-') {
        Some((left, right)) if !right.is_empty() => (left, Some(right)),
        Some(_) => return false,
        None => (without_build, None),
    };
    if prerelease.is_some_and(|part| !valid_identifiers(part, true)) {
        return false;
    }
    let numbers: Vec<_> = core.split('.').collect();
    numbers.len() == 3
        && numbers.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
                && part.parse::<u64>().is_ok()
        })
}

fn valid_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    value.split('.').all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && (!reject_numeric_leading_zero
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || part == "0"
                || !part.starts_with('0'))
    })
}

pub(crate) fn digest(lock: &ToolchainLock) -> String {
    use sha2::{Digest, Sha256};

    let mut entries = Vec::new();
    for (key, value) in &lock.base {
        let value = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        entries.push(format!("base\t{key}\t{value}"));
    }
    entries.push(format!("seeder\tdigest\t{}", lock.seeder.digest));
    entries.push(format!("seeder\timage\t{}", lock.seeder.image));
    for (name, tool) in &lock.tools {
        entries.push(format!("tool\t{name}\tpackage\t{}", tool.package));
        entries.push(format!("tool\t{name}\tversion\t{}", tool.version));
        entries.push(format!("tool\t{name}\tintegrity\t{}", tool.integrity));
        entries.push(format!(
            "tool\t{name}\tresolved_before\t{}",
            tool.resolved_before
        ));
    }
    entries.sort();
    let canonical = entries.join("\n");
    Sha256::digest(canonical.as_bytes())
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn capsule_all_image(digest: &str) -> String {
    format!("capscode/capsule-all:{digest}")
}

pub(crate) fn snapshot_path(digest: &str) -> Result<PathBuf> {
    snapshot_path_in(&snapshots_dir(), digest)
}

fn snapshot_path_in(dir: &Path, digest: &str) -> Result<PathBuf> {
    if !is_digest(digest) {
        bail!("toolchain digest must be 12 lowercase hexadecimal characters");
    }
    Ok(dir.join(format!("{digest}.lock.toml")))
}

pub(crate) fn is_digest(value: &str) -> bool {
    value.len() == 12
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn write_runtime_lock_at(path: &Path, lock: &ToolchainLock) -> Result<()> {
    validate_lock(lock, true)?;
    write_lock(path, lock)
}

pub(crate) fn remove_runtime_lock() -> Result<()> {
    let path = runtime_lock_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

pub(crate) fn write_snapshot(lock: &ToolchainLock, expected_digest: &str) -> Result<PathBuf> {
    write_snapshot_in(&snapshots_dir(), lock, expected_digest)
}

pub(crate) fn write_snapshot_in(
    dir: &Path,
    lock: &ToolchainLock,
    expected_digest: &str,
) -> Result<PathBuf> {
    validate_lock(lock, true)?;
    let path = snapshot_path_in(dir, expected_digest)?;
    if self::digest(lock) != expected_digest {
        bail!("snapshot digest does not match its tool versions");
    }
    write_lock(&path, lock)?;
    Ok(path)
}

fn write_lock(path: &Path, lock: &ToolchainLock) -> Result<()> {
    let parent = path.parent().context("toolchain lock path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    set_private_directory(parent)?;
    let contents = toml::to_string_pretty(lock).context("serializing toolchain lock")?;
    crate::atomic_file::write(path, contents.as_bytes(), Some(0o600))
        .with_context(|| format!("atomically writing {}", path.display()))?;
    Ok(())
}

fn set_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn read_snapshot(digest: &str) -> Result<Snapshot> {
    read_snapshot_in(&snapshots_dir(), digest)
}

pub(crate) fn read_snapshot_in(dir: &Path, digest: &str) -> Result<Snapshot> {
    let path = snapshot_path_in(dir, digest)?;
    let lock = read_validated(&path)?;
    if self::digest(&lock) != digest {
        bail!(
            "toolchain snapshot {} does not match its filename",
            path.display()
        );
    }
    let modified = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok();
    Ok(Snapshot {
        digest: digest.to_owned(),
        path,
        lock,
        modified,
    })
}

pub(crate) fn list_snapshots() -> Result<Vec<Snapshot>> {
    let dir = snapshots_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut snapshots = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(digest) = name.strip_suffix(".lock.toml") else {
            continue;
        };
        if !is_digest(digest) {
            continue;
        }
        snapshots.push(read_snapshot(digest)?);
    }
    snapshots.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.digest.cmp(&right.digest))
    });
    Ok(snapshots)
}

/// Current, box-referenced, then N newest other snapshots are protected.
pub(crate) fn protected_snapshots(
    snapshots: &[Snapshot],
    current: &str,
    referenced: &std::collections::HashSet<String>,
    keep: usize,
) -> std::collections::HashSet<String> {
    let mut protected = referenced.clone();
    protected.insert(current.to_owned());
    let newest: Vec<_> = snapshots
        .iter()
        .filter(|snapshot| !protected.contains(&snapshot.digest))
        .take(keep)
        .map(|snapshot| snapshot.digest.clone())
        .collect();
    protected.extend(newest);
    protected
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn built_in_lock_is_valid_and_digest_matches_image_tag_contract() {
        let lock = builtin_lock().unwrap();
        assert_eq!(lock.schema, LOCK_SCHEMA);
        assert_eq!(digest(&lock).len(), 12);
    }

    #[test]
    fn shell_and_rust_compute_the_same_digest_for_shipped_and_fixture_locks() {
        use std::process::Command;

        fn check_digest(contents: &str, expected: &str, temp: &Path, helper: &Path) {
            let path = temp.join("fixture.lock.toml");
            fs::write(&path, contents).unwrap();
            let output = Command::new("bash")
                .arg("-c")
                .arg("source \"$1\"; toolchain_lock_digest \"$2\"")
                .arg("capscode-digest-test")
                .arg(helper)
                .arg(&path)
                .output()
                .expect("run the shipped shell digest function");
            assert!(
                output.status.success(),
                "shell digest failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let helper = root.join("scripts/lib/toolchain-lock.sh");
        let temp = tempfile::tempdir().unwrap();
        let shipped = builtin_lock().unwrap();
        check_digest(BUILTIN_LOCK, &digest(&shipped), temp.path(), &helper);

        let fixture_text = BUILTIN_LOCK
            .replacen(
                "resolved_before = \"2026-10-01T04:51:24Z\"",
                "resolved_before = \"2026-10-02T04:51:24Z\"",
                1,
            )
            .replacen("bun_version = \"1.3.14\"", "bun_version = \"1.3.15\"", 1);
        let fixture = parse_validated(&fixture_text).unwrap();
        assert_ne!(digest(&shipped), digest(&fixture));
        check_digest(&fixture_text, &digest(&fixture), temp.path(), &helper);
    }

    #[test]
    fn digest_changes_when_locked_package_or_base_inputs_change() {
        let mut lock = builtin_lock().unwrap();
        let original = digest(&lock);

        lock.tools.get_mut("codex").unwrap().resolved_before = "2026-10-02T04:51:25Z".into();
        assert_ne!(digest(&lock), original);
        lock = builtin_lock().unwrap();
        lock.tools.get_mut("codex").unwrap().integrity.push('A');
        assert_ne!(digest(&lock), original);
        lock = builtin_lock().unwrap();
        lock.base.insert(
            "bun_digest".into(),
            toml::Value::String(format!("sha256:{}", "a".repeat(64))),
        );
        assert_ne!(digest(&lock), original);
        lock = builtin_lock().unwrap();
        lock.seeder.digest = format!("sha256:{}", "b".repeat(64));
        assert_ne!(digest(&lock), original);
    }

    #[test]
    fn build_script_emits_the_contract_version_declared_by_the_cli() {
        let build_script = include_str!("../../../scripts/build-images.sh");
        assert!(build_script.contains(&format!("IMAGE_CONTRACT={IMAGE_CONTRACT_VERSION}")));
        for role in ["base", "capsule", "gateway"] {
            assert!(build_script.contains(&format!("capscode.role={role}")));
        }
        assert!(build_script.contains("capscode.contract=$IMAGE_CONTRACT"));
    }

    #[test]
    fn lock_semver_rejects_numeric_components_that_do_not_fit_u64() {
        assert!(!is_semver("1234567890123456789012345.0.0"));
        assert!(!is_semver("1.1234567890123456789012345.0"));
        assert!(!is_semver("1.0.1234567890123456789012345"));
        assert!(is_semver("18446744073709551615.0.0"));
    }

    #[test]
    fn schema_two_is_refused_with_update_guidance() {
        let mut lock: ToolchainLock = toml::from_str(BUILTIN_LOCK).unwrap();
        lock.schema = 2;
        let text = toml::to_string(&lock).unwrap();
        let error = parse_validated(&text).unwrap_err().to_string();
        assert!(error.contains("schema 2"));
        assert!(error.contains("update CapsCode"));
    }

    #[test]
    fn lock_without_schema_is_refused() {
        let lock = BUILTIN_LOCK.replacen("schema = 1\n", "", 1);
        let error = parse_validated(&lock).unwrap_err().to_string();
        assert!(error.contains("integer `schema` version"));
    }

    #[test]
    fn malformed_runtime_lock_is_not_replaced_by_builtin() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("toolchain.lock.toml");
        fs::write(
            &path,
            BUILTIN_LOCK.replace("version = \"0.159.3\"", "version = \"not-semver\""),
        )
        .unwrap();
        let error = format!("{:#}", resolve_with_runtime_path(None, &path).unwrap_err());
        assert!(error.contains("version for 'codex'"));
        assert!(error.contains("toolchain use built-in"));
    }

    #[test]
    fn lock_resolution_order_is_config_override_then_runtime_then_builtin() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime.lock.toml");
        let config = root.path().join("config.lock.toml");
        let runtime_text = BUILTIN_LOCK.replace("version = \"0.159.3\"", "version = \"0.180.0\"");
        let config_text = BUILTIN_LOCK.replace("version = \"0.159.3\"", "version = \"0.190.0\"");
        fs::write(&runtime, runtime_text).unwrap();
        fs::write(&config, config_text).unwrap();

        let explicit = resolve_with_runtime_path(Some(&config), &runtime).unwrap();
        assert_eq!(explicit.lock.tools["codex"].version, "0.190.0");
        assert_eq!(explicit.path.as_deref(), Some(config.as_path()));

        let active = resolve_with_runtime_path(None, &runtime).unwrap();
        assert_eq!(active.lock.tools["codex"].version, "0.180.0");
        assert_eq!(active.path.as_deref(), Some(runtime.as_path()));

        let builtin =
            resolve_with_runtime_path(None, &root.path().join("missing.lock.toml")).unwrap();
        assert_eq!(builtin.lock.tools["codex"].version, "0.159.3");
        assert_eq!(builtin.source, BUILTIN_SOURCE);
    }

    #[test]
    fn package_names_and_sha512_integrity_are_validated() {
        let mut lock: ToolchainLock = toml::from_str(BUILTIN_LOCK).unwrap();
        lock.tools.get_mut("codex").unwrap().package = "attacker-package".into();
        let error = validate_lock(&lock, true).unwrap_err().to_string();
        assert!(error.contains("must be '@openai/codex'"));
        lock.tools.get_mut("codex").unwrap().package = "@openai/codex".into();
        lock.tools.get_mut("codex").unwrap().integrity = "sha512-bad".into();
        assert!(validate_lock(&lock, true).is_err());
    }

    #[test]
    fn semver_parser_accepts_release_and_prerelease_shapes() {
        assert!(is_semver("1.2.3"));
        assert!(is_semver("1.2.3-rc.1+build.7"));
        assert!(!is_semver("01.2.3"));
        assert!(!is_semver("1.2"));
        assert!(!is_semver("1.2.3-01"));
    }

    #[test]
    fn prune_protects_current_referenced_and_newest_other_snapshots() {
        let snapshot = |digest: &str| Snapshot {
            digest: digest.into(),
            path: PathBuf::new(),
            lock: ToolchainLock::default(),
            modified: None,
        };
        let snapshots = [
            "000000000004",
            "000000000003",
            "000000000002",
            "000000000001",
        ]
        .map(snapshot);
        let referenced = HashSet::from(["000000000003".to_owned()]);
        let keep = protected_snapshots(&snapshots, "000000000001", &referenced, 1);
        assert_eq!(
            keep,
            HashSet::from([
                "000000000001".to_owned(),
                "000000000003".to_owned(),
                "000000000004".to_owned()
            ])
        );
    }
}

#[cfg(test)]
mod release_metadata_tests {
    use super::*;

    #[test]
    fn built_in_lock_declares_release_metadata() {
        let lock = builtin_lock().unwrap();
        assert_eq!(lock.sequence, Some(1));
        assert_eq!(
            lock.min_cli_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(lock.yanked.is_empty());
    }

    #[test]
    fn yank_entries_match_digests_and_tool_versions() {
        let lock = builtin_lock().unwrap();
        let digest = digest(&lock);
        let codex = format!("codex@{}", lock.tools["codex"].version);
        assert_eq!(
            yanked_by(&digest, Some(&lock), std::slice::from_ref(&codex)),
            Some(codex.clone())
        );
        assert!(yanked_by(&digest, Some(&lock), std::slice::from_ref(&digest)).is_some());
        assert!(yanked_by(&digest, Some(&lock), &["codex@0.0.1".to_owned()]).is_none());
        assert!(yanked_by(&digest, None, &[codex]).is_none());
        let tools = || lock.tools.keys().map(String::as_str);
        assert!(is_yank_entry("codex@0.160.0", tools()));
        assert!(is_yank_entry("fb24882402b5", tools()));
        assert!(!is_yank_entry("evil@1.0.0", tools()));
        assert!(!is_yank_entry("codex@1.0.0;id", tools()));
    }

    #[test]
    fn a_lock_without_sequence_or_with_zero_sequence() {
        let text = BUILTIN_LOCK.replacen("sequence = 1\n", "", 1);
        assert_eq!(parse_validated(&text).unwrap().sequence, None);
        let text = BUILTIN_LOCK.replacen("sequence = 1\n", "sequence = 0\n", 1);
        assert!(parse_validated(&text).is_err());
        let text = BUILTIN_LOCK.replacen(
            "min_cli_version = \"0.1.0\"",
            "min_cli_version = \"0.1.0; id\"",
            1,
        );
        assert!(parse_validated(&text).is_err());
    }

    #[test]
    fn unknown_lock_keys_must_be_identifiers_with_printable_values() {
        let text = BUILTIN_LOCK.replacen("schema = 1\n", "schema = 1\nfuture_note = \"fine\"\n", 1);
        assert!(parse_validated(&text).is_ok());
        for bad in [
            "\"Bad Key\" = 1\n",
            "future_note = \"line\\nversion = \\\"1.0.0; id\\\"\"\n",
            "future_note = [\"\\u001b[2J\"]\n",
        ] {
            let text = BUILTIN_LOCK.replacen("schema = 1\n", &format!("schema = 1\n{bad}"), 1);
            assert!(parse_validated(&text).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn a_written_runtime_lock_keeps_one_field_per_line_for_the_build_script() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("toolchain.lock.toml");
        write_runtime_lock_at(&path, &builtin_lock().unwrap()).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[tools.codex]\n"), "{text}");
        assert!(text.contains("\nversion = \"0.159.3\"\n"), "{text}");
        assert!(text.contains("\nsequence = 1\n"), "{text}");
        let reread = read_validated(&path).unwrap();
        assert_eq!(digest(&reread), "fb24882402b5");
    }
}
