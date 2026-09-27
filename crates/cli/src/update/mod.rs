//! The update channel client: `capscode update`, the passive daily check, and the
//! yank list shared with `ls`, `toolchain use`, and `resume`.
//!
//! Trust model: there is no maintainer signing key. The client relies on TLS to
//! GitHub plus local policy: the lock's SHA-256 must match the manifest, package
//! and image names come from a compiled-in allowlist, every value is validated
//! before it reaches a Dockerfile or shell, sequences only move forward, the
//! minimum CLI version is honoured, yanked toolchains are refused, and any tool
//! downgrade needs a recorded reason plus the user's confirmation.

pub(crate) mod archive;
pub(crate) mod channel;
pub(crate) mod cli;
pub(crate) mod fetch;
pub(crate) mod gh;
pub(crate) mod passive;
pub(crate) mod state;
pub(crate) mod superseded;

#[cfg(test)]
mod cli_tests;
#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::toolchain::{self, ResolvedLock, ToolchainLock};
use crate::ui::{data, note};

use channel::{compare_versions, Channel, ChannelName};
use fetch::Fetch;
use state::{AcceptedToolchain, UpdateState};

/// Budget for `capscode update`, which the user is waiting on.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn own_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Where update state and locks live; tests point this at a temporary directory.
#[derive(Clone, Debug)]
pub(crate) struct UpdatePaths {
    pub home: PathBuf,
    pub runtime_lock: PathBuf,
    pub snapshots: PathBuf,
}

impl UpdatePaths {
    pub(crate) fn in_home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            runtime_lock: home.join("toolchain.lock.toml"),
            snapshots: home.join("toolchains"),
        }
    }

    fn check_state(&self) -> PathBuf {
        state::check_state_path(&self.home)
    }

    fn update_state(&self) -> PathBuf {
        state::update_state_path(&self.home)
    }
}

/// Everything `run_update` needs to decide, without reading process globals.
pub(crate) struct UpdateContext {
    pub channel: ChannelName,
    pub verify_with_gh: bool,
    pub gh_path: Option<OsString>,
    pub own_version: String,
    pub active: ResolvedLock,
    pub config_override: bool,
    pub paths: UpdatePaths,
    pub interactive: bool,
    pub now_unix: u64,
    /// How this CLI is updated, shown when a newer CapsCode is available.
    pub cli_update_command: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UpdateOptions {
    pub check_only: bool,
    pub yes: bool,
}

/// Asks the user a yes/no question; the terminal implementation defaults to no.
pub(crate) trait Confirmer {
    fn confirm(&mut self, prompt: &str) -> Result<bool>;
}

struct TerminalConfirmer;

impl Confirmer for TerminalConfirmer {
    fn confirm(&mut self, prompt: &str) -> Result<bool> {
        dialoguer::Confirm::with_theme(&crate::ui::theme())
            .with_prompt(prompt)
            .default(false)
            .interact()
            .context("reading confirmation")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing newer than the active sequence.
    UpToDate,
    /// `--check`: a newer toolchain passed every check but was not applied.
    Available { digest: String },
    /// The new lock is now the runtime lock.
    Applied { digest: String },
}

#[derive(Debug)]
pub(crate) struct Report {
    pub lines: Vec<String>,
    pub outcome: Outcome,
}

/// One tool's version change between the active lock and the offered lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolChange {
    pub tool: String,
    pub from: Option<String>,
    pub to: String,
    pub downgrade_reason: Option<String>,
    /// `to` orders before `from`.
    pub downgrade: bool,
}

/// `capscode update [--check] [--yes]`.
pub(crate) fn update(options: UpdateOptions) -> Result<()> {
    let config = crate::config::load(None)?;
    let channel = ChannelName::parse(&config.update.channel)?;
    let home =
        capscode_core::Config::ensure_config_home().context("preparing the CapsCode home")?;
    let active = superseded::supersede(
        toolchain::resolve(config.image.toolchain_lock.as_deref())?,
        &home,
        own_version(),
    );
    let context = UpdateContext {
        channel,
        verify_with_gh: config.update.verify_with_gh,
        gh_path: std::env::var_os("PATH"),
        own_version: own_version().to_owned(),
        active,
        config_override: config.image.toolchain_lock.is_some(),
        paths: UpdatePaths::in_home(&home),
        interactive: io::stdin().is_terminal() && io::stderr().is_terminal(),
        now_unix: now_unix(),
        cli_update_command: cli::update_command(),
    };
    let fetcher = fetch::https_fetcher(UPDATE_TIMEOUT);
    let report = run_update(&context, &fetcher, options, &mut TerminalConfirmer)?;
    for line in &report.lines {
        data(line);
    }
    if let Outcome::Applied { digest } = &report.outcome {
        for line in box_lines(digest) {
            note(line);
        }
    }
    Ok(())
}

/// Fetch, verify, and (unless `check_only`) apply the channel's toolchain.
pub(crate) fn run_update(
    context: &UpdateContext,
    fetcher: &dyn Fetch,
    options: UpdateOptions,
    confirmer: &mut dyn Confirmer,
) -> Result<Report> {
    let channel_name = &context.channel;
    let manifest_bytes = fetcher
        .get(&channel_name.manifest_url(), channel::MAX_MANIFEST_BYTES)
        .with_context(|| format!("fetching the release channel for {channel_name}"))?;
    let manifest = channel::parse(&manifest_bytes, channel_name)
        .with_context(|| format!("checking the release channel for {channel_name}"))?;
    remember_manifest(&context.paths, channel_name, &manifest, context.now_unix);
    // The stored manifest is the newest one seen, so its yank list also covers a
    // fetched copy that a cache served stale.
    let stored_yanks = known_yanks_in(
        &context.paths.home,
        Some(channel_name),
        &context.active.lock,
    );

    let mut lines = cli_lines(&manifest, &context.own_version, &context.cli_update_command)?;
    let update_state = state::read_update_state(&context.paths.update_state())?;
    let active_digest = toolchain::digest(&context.active.lock);
    let active_sequence = active_sequence(&context.active.lock, &update_state);
    let offered = &manifest.toolchain;

    if offered.sequence <= active_sequence {
        lines.push(up_to_date_line(
            offered,
            active_sequence,
            &active_digest,
            &update_state,
        ));
        let yanks: Vec<String> = offered
            .yanked
            .iter()
            .chain(&stored_yanks)
            .cloned()
            .collect();
        if let Some(entry) =
            toolchain::yanked_by(&active_digest, Some(&context.active.lock), &yanks)
        {
            lines.push(format!(
                "Your toolchain {active_digest} was withdrawn ({entry}); pick another with `capscode toolchain ls` and `capscode toolchain use <digest>`."
            ));
        }
        return Ok(Report {
            lines,
            outcome: Outcome::UpToDate,
        });
    }
    if compare_versions(&offered.min_cli_version, &context.own_version)
        .context("checking the toolchain's minimum CapsCode version; refusing it")?
        == Ordering::Greater
    {
        bail!(
            "toolchain {} needs CapsCode {} or newer (you have {}); update the CLI first",
            offered.sequence,
            offered.min_cli_version,
            context.own_version
        );
    }
    if offered.yanked.iter().any(|entry| entry == &offered.digest) {
        bail!(
            "the channel offers toolchain {} but also lists it as withdrawn; refusing it",
            offered.digest
        );
    }

    let lock_bytes = fetcher
        .get(&offered.lock_url, channel::MAX_LOCK_BYTES)
        .context("downloading the toolchain lock")?;
    let lock = verify_lock(&lock_bytes, &manifest, &context.own_version)?;
    if let Some(entry) = toolchain::yanked_by(&offered.digest, Some(&lock), &stored_yanks) {
        bail!("the channel toolchain includes withdrawn {entry}; refusing it");
    }
    let changes = tool_changes(&context.active.lock, &lock)?;
    let downgrades: Vec<&ToolChange> = changes.iter().filter(|change| change.downgrade).collect();
    if let Some(change) = downgrades
        .iter()
        .find(|change| change.downgrade_reason.as_deref().is_none_or(str::is_empty))
    {
        bail!(
            "toolchain {} moves {} back from {} to {} without a recorded downgrade_reason; refusing it",
            offered.sequence,
            change.tool,
            change.from.as_deref().unwrap_or("?"),
            change.to
        );
    }
    if context.verify_with_gh {
        gh::verify_lock(
            &lock_bytes,
            channel_name,
            context.gh_path.as_deref(),
            gh::GH_TIMEOUT,
        )?;
        lines.push(format!("GitHub attestation verified for {channel_name}."));
    }

    lines.push(format!(
        "Toolchain {} (sequence {}) is available; you have {} (sequence {}).",
        offered.digest, offered.sequence, active_digest, active_sequence
    ));
    lines.extend(
        changes
            .iter()
            .filter(|change| change.from.as_deref() != Some(&change.to))
            .map(change_line),
    );
    if options.check_only {
        if !downgrades.is_empty() {
            lines.push(
                "Applying it asks for confirmation (or `--yes`) because it downgrades tools."
                    .to_owned(),
            );
        }
        lines.push("Run `capscode update` to apply it.".to_owned());
        return Ok(Report {
            lines,
            outcome: Outcome::Available {
                digest: offered.digest.clone(),
            },
        });
    }

    if !downgrades.is_empty() && !options.yes {
        if !context.interactive {
            bail!(
                "this update moves {} to an older version; rerun with `--yes` to accept the recorded reason",
                downgrades
                    .iter()
                    .map(|change| change.tool.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let listed = downgrades
            .iter()
            .map(|change| change_line(change).trim().to_owned())
            .collect::<Vec<_>>()
            .join("; ");
        if !confirmer.confirm(&format!(
            "Toolchain {} downgrades {listed}. Apply it?",
            offered.sequence
        ))? {
            bail!("update cancelled");
        }
    }

    toolchain::write_snapshot_in(&context.paths.snapshots, &lock, &offered.digest)?;
    toolchain::write_runtime_lock_at(&context.paths.runtime_lock, &lock)?;
    let mut update_state = update_state;
    update_state.runtime_lock =
        superseded::WrittenRuntimeLock::of(&context.paths.runtime_lock, &offered.digest);
    update_state.record(AcceptedToolchain {
        sequence: offered.sequence,
        digest: offered.digest.clone(),
        channel: channel_name.to_string(),
        lock_url: offered.lock_url.clone(),
        lock_sha256: offered.lock_sha256.clone(),
        attestation_url: offered.attestation_url.clone(),
        attestation_sha256: offered.attestation_sha256.clone(),
        accepted_unix: context.now_unix,
    });
    state::write_json(&context.paths.update_state(), &update_state)?;

    lines.push(format!(
        "Toolchain {} is now the default. Build its images with `capscode toolchain build`.",
        offered.digest
    ));
    if context.config_override {
        lines.push(
            "Note: `image.toolchain_lock` in your config still takes precedence over this toolchain."
                .to_owned(),
        );
    }
    Ok(Report {
        lines,
        outcome: Outcome::Applied {
            digest: offered.digest.clone(),
        },
    })
}

/// Every check that ties the downloaded lock bytes to the manifest that named them.
pub(crate) fn verify_lock(
    bytes: &[u8],
    manifest: &Channel,
    own_version: &str,
) -> Result<ToolchainLock> {
    let offered = &manifest.toolchain;
    let actual = hex(&Sha256::digest(bytes));
    if actual != offered.lock_sha256 {
        bail!("the downloaded toolchain lock does not match the SHA-256 in the channel manifest; refusing it");
    }
    let text = std::str::from_utf8(bytes).context("the downloaded toolchain lock is not UTF-8")?;
    let lock = toolchain::parse_validated(text)
        .context("the downloaded toolchain lock failed validation")?;
    if lock.sequence != Some(offered.sequence) {
        bail!(
            "the downloaded lock's sequence does not match channel sequence {}; refusing it",
            offered.sequence
        );
    }
    let Some(min_cli) = lock.min_cli_version.as_deref() else {
        bail!("the downloaded lock does not declare min_cli_version; refusing it");
    };
    if min_cli != offered.min_cli_version {
        bail!("the downloaded lock's min_cli_version does not match the channel manifest; refusing it");
    }
    if compare_versions(min_cli, own_version)
        .context("checking the lock's minimum CapsCode version; refusing it")?
        == Ordering::Greater
    {
        bail!("this toolchain needs CapsCode {min_cli} or newer (you have {own_version}); update the CLI first");
    }
    let digest = toolchain::digest(&lock);
    if digest != offered.digest {
        bail!(
            "the downloaded lock's tool versions do not match channel digest {}; refusing it",
            offered.digest
        );
    }
    if lock.untested {
        bail!("the channel lock is marked untested; refusing it");
    }
    let yanks: Vec<String> = offered.yanked.iter().chain(&lock.yanked).cloned().collect();
    if let Some(entry) = toolchain::yanked_by(&digest, Some(&lock), &yanks) {
        bail!("the channel toolchain includes withdrawn {entry}; refusing it");
    }
    Ok(lock)
}

/// Every tool's version change. A version pair that cannot be ordered is an error,
/// so a downgrade can never pass as "no change".
pub(crate) fn tool_changes(
    active: &ToolchainLock,
    offered: &ToolchainLock,
) -> Result<Vec<ToolChange>> {
    offered
        .tools
        .iter()
        .map(|(name, tool)| {
            let from = active.tools.get(name).map(|old| old.version.clone());
            let downgrade = match &from {
                Some(from) => {
                    compare_versions(&tool.version, from).with_context(|| {
                        format!(
                            "comparing {name} versions {from} and {}; refusing the toolchain",
                            tool.version
                        )
                    })? == Ordering::Less
                }
                None => false,
            };
            Ok(ToolChange {
                tool: name.clone(),
                from,
                to: tool.version.clone(),
                downgrade_reason: tool.downgrade_reason.clone(),
                downgrade,
            })
        })
        .collect()
}

fn change_line(change: &ToolChange) -> String {
    let from = change.from.as_deref().unwrap_or("not installed");
    if change.downgrade {
        format!(
            "  {} {from} -> {} (downgrade: {})",
            change.tool,
            change.to,
            change.downgrade_reason.as_deref().unwrap_or("")
        )
    } else {
        format!("  {} {from} -> {}", change.tool, change.to)
    }
}

fn up_to_date_line(
    offered: &channel::ToolchainRelease,
    active_sequence: u64,
    active_digest: &str,
    update_state: &UpdateState,
) -> String {
    if offered.digest != active_digest {
        if let Some(record) = update_state.accepted_for(&offered.digest) {
            return format!(
                "Toolchain {} (sequence {}) was already accepted on this machine; switch back with `capscode toolchain use {}`.",
                record.digest, record.sequence, record.digest
            );
        }
    }
    if offered.sequence < active_sequence {
        format!(
            "The channel offers toolchain sequence {}, older than sequence {active_sequence} already accepted here; ignoring it.",
            offered.sequence
        )
    } else {
        format!("Toolchain is up to date (sequence {active_sequence}).")
    }
}

/// The sequence the next channel lock must exceed.
pub(crate) fn active_sequence(lock: &ToolchainLock, update_state: &UpdateState) -> u64 {
    lock.sequence
        .unwrap_or(0)
        .max(update_state.highest_accepted_sequence)
}

/// `cli_update` is the command that updates this CLI: `capscode update --cli` for a
/// release binary, the checkout's git command for a source build.
fn cli_lines(manifest: &Channel, own: &str, cli_update: &str) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    if manifest
        .cli
        .yanked_versions
        .iter()
        .any(|version| version == own)
    {
        lines.push(format!(
            "CapsCode {own} was withdrawn; update to {} with `{cli_update}` ({}).",
            manifest.cli.version, manifest.cli.notes_url
        ));
    } else if compare_versions(&manifest.cli.version, own)
        .context("comparing the channel's CapsCode version with this one")?
        == Ordering::Greater
    {
        lines.push(format!(
            "CapsCode {} is available (you have {own}): {}; install it with `{cli_update}`.",
            manifest.cli.version, manifest.cli.notes_url
        ));
    }
    Ok(lines)
}

/// One line for the passive check, or `None` when there is nothing to say. A version
/// that cannot be compared is never advertised.
pub(crate) fn notice_line(
    manifest: &Channel,
    own: &str,
    cli_update: &str,
    active_digest: &str,
    active_lock: &ToolchainLock,
    active_sequence: u64,
) -> Option<String> {
    let mut parts = Vec::new();
    if manifest
        .cli
        .yanked_versions
        .iter()
        .any(|version| version == own)
    {
        parts.push(format!(
            "CapsCode {own} was withdrawn; update to {} — {cli_update}",
            manifest.cli.version
        ));
    } else if matches!(
        compare_versions(&manifest.cli.version, own),
        Ok(Ordering::Greater)
    ) {
        parts.push(format!(
            "CapsCode {} is available (you have {own}) — {cli_update}",
            manifest.cli.version
        ));
    }
    let yanks: Vec<String> = manifest
        .toolchain
        .yanked
        .iter()
        .chain(&active_lock.yanked)
        .cloned()
        .collect();
    let offered = &manifest.toolchain;
    let offered_usable = offered.sequence > active_sequence
        && matches!(
            compare_versions(&offered.min_cli_version, own),
            Ok(Ordering::Less | Ordering::Equal)
        )
        && !yanks.contains(&offered.digest);
    if toolchain::yanked_by(active_digest, Some(active_lock), &yanks).is_some() {
        parts.push(format!(
            "your toolchain {active_digest} was withdrawn — capscode update"
        ));
    } else if offered_usable {
        parts.push(format!(
            "toolchain {} is available — capscode update",
            offered.sequence
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn remember_manifest(paths: &UpdatePaths, channel: &ChannelName, manifest: &Channel, now: u64) {
    let path = paths.check_state();
    let mut check = state::read_check_state(&path);
    let remembered = check.remember(channel, manifest);
    let first_attempt = check.last_attempt_unix == 0;
    if first_attempt {
        check.last_attempt_unix = now;
    }
    // The stored manifest only refreshes yank lists; failing to save it must not
    // fail the command.
    if remembered || first_attempt {
        let _ = state::write_json(&path, &check);
    }
}

/// Yank entries known locally: the last validated manifest and the active lock.
pub(crate) fn known_yanks_in(
    home: &Path,
    channel: Option<&ChannelName>,
    active: &ToolchainLock,
) -> Vec<String> {
    let mut entries = active.yanked.clone();
    if let Some(channel) = channel {
        let check = state::read_check_state(&state::check_state_path(home));
        if let Some(manifest) = check.manifest_for(channel) {
            entries.extend(manifest.toolchain.yanked.iter().cloned());
        }
    }
    entries.sort();
    entries.dedup();
    entries
}

/// Yank entries for the user's configuration; empty when nothing can be read.
pub(crate) fn known_yanks(config: &capscode_core::Config, active: &ToolchainLock) -> Vec<String> {
    let channel = ChannelName::parse(&config.update.channel).ok();
    known_yanks_in(&toolchain::config_home(), channel.as_ref(), active)
}

/// Refuse a withdrawn toolchain unless the user forces it.
pub(crate) fn refuse_yanked(
    digest: &str,
    lock: Option<&ToolchainLock>,
    yanks: &[String],
    force: bool,
) -> Result<Option<String>> {
    let Some(entry) = toolchain::yanked_by(digest, lock, yanks) else {
        return Ok(None);
    };
    if !force {
        bail!(
            "toolchain {digest} was withdrawn ({entry}); pick another with `capscode toolchain ls`, or pass `--force` to use it anyway"
        );
    }
    Ok(Some(format!(
        "warning: using withdrawn toolchain {digest} ({entry}) because of --force"
    )))
}

/// Provenance lines for `toolchain show`, when the active lock came from the channel.
pub(crate) fn provenance_lines(home: &Path, digest: &str) -> Vec<String> {
    let Ok(update_state) = state::read_update_state(&state::update_state_path(home)) else {
        return Vec::new();
    };
    let Some(record) = update_state.accepted_for(digest) else {
        return Vec::new();
    };
    let mut lines = vec![
        format!(
            "Released as {} sequence {} from {}",
            "toolchain", record.sequence, record.channel
        ),
        format!("Lock {} (sha256 {})", record.lock_url, record.lock_sha256),
        format!("Attestation {}", record.attestation_url),
    ];
    if let Some(sha) = &record.attestation_sha256 {
        lines.push(format!("Attestation sha256 {sha}"));
    }
    lines.push(format!(
        "Verify by hand: download the lock, then run `gh attestation verify toolchain.lock.toml --repo {} --signer-workflow {}/{}`",
        record.channel,
        record.channel,
        gh::TOOLCHAIN_WORKFLOW
    ));
    lines
}

fn box_lines(new_digest: &str) -> Vec<String> {
    let Ok(entries) = crate::boxes::index::read() else {
        return Vec::new();
    };
    let older: Vec<_> = entries
        .iter()
        .filter(|entry| toolchain::is_digest(&entry.toolchain) && entry.toolchain != new_digest)
        .collect();
    if older.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        "Boxes on older toolchains are not changed now. Running boxes stay as they are; when you resume a stopped box you choose to switch or keep (keep is the default):".to_owned(),
    ];
    for entry in older {
        let pinned = if entry.toolchain_pinned {
            ", pinned"
        } else {
            ""
        };
        lines.push(format!(
            "  {} ({}{pinned}): `capscode resume {}`",
            entry.name,
            entry.toolchain.get(..7).unwrap_or(&entry.toolchain),
            entry.name
        ));
    }
    lines
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
