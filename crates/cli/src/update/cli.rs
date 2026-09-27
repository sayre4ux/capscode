//! `capscode update --cli`: replace this binary with the channel's tagged release.
//!
//! Trust model, in order: the channel manifest names the release (TLS to GitHub,
//! validated as in `capscode update`); only a plain `vX.Y.Z` tag is accepted, never
//! a prerelease; the release's `SHA256SUMS` must list every downloaded asset and
//! each download must match its entry; with `update.verify_with_gh` the binary and
//! build context must also carry a GitHub attestation from the CLI release
//! workflow. Nothing is written until every download has passed, the build context
//! is swapped in atomically, and the binary is replaced with a rename of a synced
//! temporary file in the same directory, after the new binary has proven it runs.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use super::channel::{self, compare_versions, ChannelName};
use super::fetch::{self, Fetch};
use super::{gh, hex, own_version, Confirmer, TerminalConfirmer};
use crate::ui::{data, note};

pub(crate) const SUMS_ASSET: &str = "SHA256SUMS";
const MAX_SUMS_BYTES: usize = 16 * 1024;
const MAX_SUMS_LINES: usize = 64;
const MAX_NOTES_BYTES: usize = 64 * 1024;
const NOTES_SHOWN_LINES: usize = 60;
// DECISION: download caps of 64 MiB (binary) and 16 MiB (build context), and one
// 10-minute budget per download, so a slow link finishes but a stalled one does not hang.
/// The macOS universal binary (two architectures) is about 30 MiB; the Linux
/// binaries are about half that. 64 MiB is twice the largest, so ordinary growth
/// fits while a runaway or hostile response is cut off.
pub(crate) const MAX_BINARY_BYTES: usize = 64 * 1024 * 1024;
/// The compressed build context is well under 1 MiB.
pub(crate) const MAX_CONTEXT_BYTES: usize = 16 * 1024 * 1024;
/// The whole download, binary included, on a slow connection.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The release asset suffix for the platform this binary was built for.
pub(crate) fn platform() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("macos-universal")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("linux-aarch64")
    } else {
        None
    }
}

pub(crate) fn binary_asset(version: &str, platform: &str) -> String {
    format!("capscode-{version}-{platform}")
}

pub(crate) fn context_asset(version: &str) -> String {
    format!("capscode-{version}-build-context.tar.gz")
}

pub(crate) fn notes_asset(version: &str) -> String {
    format!("capscode-{version}-release-notes.md")
}

/// Everything the CLI update decides on, without reading process globals.
pub(crate) struct CliUpdateContext {
    pub channel: ChannelName,
    pub verify_with_gh: bool,
    pub gh_path: Option<OsString>,
    pub own_version: String,
    /// `current_exe()` with symlinks resolved: the file that gets replaced.
    pub exe: PathBuf,
    /// The CapsCode home; the build context goes to `<home>/build/<version>`.
    pub home: PathBuf,
    pub platform: Option<&'static str>,
    pub interactive: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CliUpdateOptions {
    pub check_only: bool,
    pub yes: bool,
    pub allow_downgrade: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CliOutcome {
    UpToDate,
    Available { version: String },
    Installed { version: String },
}

/// Checks a downloaded binary at a temporary path before it replaces this one.
pub(crate) type Probe<'a> = &'a dyn Fn(&Path, &str) -> Result<()>;

/// `capscode update --cli [--check] [--yes] [--allow-downgrade]`.
pub(crate) fn update_cli(options: CliUpdateOptions) -> Result<()> {
    let config = crate::config::load(None)?;
    let channel = ChannelName::parse(&config.update.channel)?;
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .context("locating the running capscode binary")?;
    let home =
        capscode_core::Config::ensure_config_home().context("preparing the CapsCode home")?;
    let context = CliUpdateContext {
        channel,
        verify_with_gh: config.update.verify_with_gh,
        gh_path: std::env::var_os("PATH"),
        own_version: own_version().to_owned(),
        exe,
        home,
        platform: platform(),
        interactive: io::stdin().is_terminal() && io::stderr().is_terminal(),
    };
    let fetcher = fetch::https_fetcher(DOWNLOAD_TIMEOUT);
    let mut say = |line: &str| note(line);
    let outcome = run_cli_update(
        &context,
        &fetcher,
        options,
        &mut TerminalConfirmer,
        &mut say,
        &probe_release_binary,
    )?;
    if let CliOutcome::Installed { version } = outcome {
        data(format!("CapsCode {version} is installed."));
    }
    Ok(())
}

/// The command that updates the running CLI: a source build is updated through its
/// checkout, a release binary with `capscode update --cli`.
pub(crate) fn update_command() -> String {
    let exe = std::env::current_exe().and_then(fs::canonicalize).ok();
    update_command_for(exe.as_deref())
}

pub(crate) fn update_command_for(exe: Option<&Path>) -> String {
    match exe.and_then(source_checkout_of) {
        Some(root) => source_update_command(&root),
        None => "capscode update --cli".to_owned(),
    }
}

fn source_update_command(root: &Path) -> String {
    format!("cd {} && git pull && ./scripts/install.sh", root.display())
}

/// The checkout root when `exe` is a cargo build artifact (`<root>/target/...`).
pub(crate) fn source_checkout_of(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().skip(1).find_map(|dir| {
        if dir.file_name()? != "target" {
            return None;
        }
        let root = dir.parent()?;
        root.join("Cargo.toml")
            .is_file()
            .then(|| root.to_path_buf())
    })
}

pub(crate) fn run_cli_update(
    context: &CliUpdateContext,
    fetcher: &dyn Fetch,
    options: CliUpdateOptions,
    confirmer: &mut dyn Confirmer,
    say: &mut dyn FnMut(&str),
    probe: Probe<'_>,
) -> Result<CliOutcome> {
    if let Some(root) = source_checkout_of(&context.exe) {
        if options.check_only {
            return check_source_build(context, fetcher, &root, say);
        }
        bail!(
            "this capscode was built from source in {} and is not replaced by a download; update the checkout instead:\n  {}",
            root.display(),
            source_update_command(&root)
        );
    }
    let Some(platform) = context.platform else {
        bail!("CapsCode publishes no prebuilt binary for this platform; build it from source with `git pull && ./scripts/install.sh` in a checkout");
    };
    let install_dir = context
        .exe
        .parent()
        .context("the capscode binary has no parent directory")?;
    if !options.check_only {
        tempfile::Builder::new()
            .prefix(".capscode-write-check-")
            .tempfile_in(install_dir)
            .with_context(|| {
                format!(
                    "cannot write to {}, where capscode is installed; rerun the installer with `--bin-dir` set to a directory you own",
                    install_dir.display()
                )
            })?;
    }

    let channel_name = &context.channel;
    let (release, ordering) = offered_release(context, fetcher)?;
    let version = release.version.as_str();
    let own = context.own_version.as_str();
    match ordering {
        Ordering::Equal => {
            say(&format!(
                "CapsCode {own} is the newest release on {channel_name}."
            ));
            return Ok(CliOutcome::UpToDate);
        }
        Ordering::Less if !options.allow_downgrade => bail!(
            "the channel offers CapsCode {version}, older than the {own} you have; pass `--allow-downgrade` to install it anyway"
        ),
        Ordering::Less | Ordering::Greater => {}
    }

    let tag = &release.tag;
    let asset_url = |asset: &str| channel_name.release_asset_url(tag, asset);
    let sums_bytes = fetcher
        .get(&asset_url(SUMS_ASSET), MAX_SUMS_BYTES)
        .with_context(|| format!("downloading {SUMS_ASSET} for {tag}"))?;
    let sums =
        parse_sums(&sums_bytes).with_context(|| format!("checking {SUMS_ASSET} for {tag}"))?;
    let binary_name = binary_asset(version, platform);
    let context_name = context_asset(version);
    let notes_name = notes_asset(version);
    let binary_sha = expected_sha(&sums, tag, &binary_name)?;
    let context_sha = expected_sha(&sums, tag, &context_name)?;
    let notes_sha = expected_sha(&sums, tag, &notes_name)?;

    let notes = fetcher
        .get(&asset_url(&notes_name), MAX_NOTES_BYTES)
        .context("downloading the release notes")?;
    check_sha(&notes, notes_sha, &notes_name)?;
    let direction = if ordering == Ordering::Less {
        "downgrade"
    } else {
        "update"
    };
    say(&format!(
        "CapsCode {own} -> {version} ({direction}) from {channel_name}"
    ));
    for line in notes_lines(&notes, &release.notes_url) {
        say(&line);
    }
    if options.check_only {
        say("Run `capscode update --cli` to install it.");
        return Ok(CliOutcome::Available {
            version: version.to_owned(),
        });
    }

    if !options.yes {
        if !context.interactive {
            bail!("replacing the CapsCode binary needs a terminal to confirm; rerun with `--yes` to accept");
        }
        if !confirmer.confirm(&format!(
            "Replace {} with CapsCode {version}?",
            context.exe.display()
        ))? {
            bail!("update cancelled");
        }
    }

    let binary = fetcher
        .get(&asset_url(&binary_name), MAX_BINARY_BYTES)
        .context("downloading the CapsCode binary")?;
    check_sha(&binary, binary_sha, &binary_name)?;
    let build_context = fetcher
        .get(&asset_url(&context_name), MAX_CONTEXT_BYTES)
        .context("downloading the image build context")?;
    check_sha(&build_context, context_sha, &context_name)?;
    if context.verify_with_gh {
        for (bytes, file_name, what) in [
            (&binary, &binary_name, "binary"),
            (&build_context, &context_name, "build context"),
        ] {
            gh::verify_bytes(
                &gh::Subject {
                    bytes,
                    file_name,
                    what,
                    workflow: gh::CLI_WORKFLOW,
                },
                channel_name,
                context.gh_path.as_deref(),
                gh::GH_TIMEOUT,
            )?;
        }
        say(&format!("GitHub attestations verified for {channel_name}."));
    }

    let build_dir = context.home.join("build");
    let unpacked = super::archive::install_build_context(&build_context, version, &build_dir)
        .context("installing the image build context")?;
    replace_executable(&context.exe, &binary, &|path| probe(path, version))
        .context("replacing the capscode binary")?;

    say(&format!(
        "Replaced {} ({own} -> {version}); image build files are in {}.",
        context.exe.display(),
        unpacked.display()
    ));
    say(&format!(
        "Build this version's images before your next session: `{}`.",
        crate::build_context::BUILD_COMMAND
    ));
    if cfg!(target_os = "macos") {
        say("macOS asks once for keychain access the next time capscode reads a stored key: the new binary has a new signature. Choose Always Allow in a terminal.");
    }
    Ok(CliOutcome::Installed {
        version: version.to_owned(),
    })
}

/// The channel's CLI release, checked as an install candidate, and how it orders
/// against the running version. A version pair that cannot be ordered is refused.
fn offered_release(
    context: &CliUpdateContext,
    fetcher: &dyn Fetch,
) -> Result<(channel::CliRelease, Ordering)> {
    let channel_name = &context.channel;
    let manifest_bytes = fetcher
        .get(&channel_name.manifest_url(), channel::MAX_MANIFEST_BYTES)
        .with_context(|| format!("fetching the release channel for {channel_name}"))?;
    let manifest = channel::parse(&manifest_bytes, channel_name)
        .with_context(|| format!("checking the release channel for {channel_name}"))?;
    let release = manifest.cli;
    let version = release.version.as_str();
    if version.contains(['-', '+']) || release.tag != format!("v{version}") {
        bail!(
            "the channel offers CapsCode {version} (tag {}), which is not a plain vX.Y.Z release; `capscode update --cli` installs tagged releases only",
            release.tag
        );
    }
    if release
        .yanked_versions
        .iter()
        .any(|yanked| yanked == version)
    {
        bail!("the channel lists CapsCode {version} as withdrawn; refusing to install it");
    }
    let ordering = compare_versions(version, &context.own_version)
        .context("comparing the channel's CapsCode version with this one; refusing it")?;
    Ok((release, ordering))
}

/// `--check` for a source build: report what the channel offers and the git
/// command that updates the checkout, since no download replaces this binary.
fn check_source_build(
    context: &CliUpdateContext,
    fetcher: &dyn Fetch,
    root: &Path,
    say: &mut dyn FnMut(&str),
) -> Result<CliOutcome> {
    let (release, ordering) = offered_release(context, fetcher)?;
    let own = &context.own_version;
    let channel_name = &context.channel;
    match ordering {
        Ordering::Equal => {
            say(&format!(
                "CapsCode {own} is the newest release on {channel_name}."
            ));
            return Ok(CliOutcome::UpToDate);
        }
        Ordering::Less => {
            say(&format!(
                "CapsCode {own}, built from source in {}, is newer than the newest release on {channel_name} ({}).",
                root.display(),
                release.version
            ));
            return Ok(CliOutcome::UpToDate);
        }
        Ordering::Greater => {}
    }
    say(&format!(
        "CapsCode {} is available on {channel_name} (you have {own}): {}",
        release.version, release.notes_url
    ));
    say(&format!(
        "This capscode was built from source in {}; update it with `{}`.",
        root.display(),
        source_update_command(root)
    ));
    Ok(CliOutcome::Available {
        version: release.version,
    })
}

/// Parse `sha256sum` output: `<64 lowercase hex>  <name>` (or ` *<name>`) per line.
pub(crate) fn parse_sums(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes).context("the file is not UTF-8")?;
    let mut sums = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        if index >= MAX_SUMS_LINES {
            bail!("the file has more than {MAX_SUMS_LINES} lines");
        }
        let (hash, rest) = line
            .split_at_checked(64)
            .with_context(|| format!("line {} is not `<sha256>  <file>`", index + 1))?;
        let name = rest
            .strip_prefix("  ")
            .or_else(|| rest.strip_prefix(" *"))
            .with_context(|| format!("line {} is not `<sha256>  <file>`", index + 1))?;
        if !crate::toolchain::is_lower_hex(hash, 64) || !is_asset_name(name) {
            bail!("line {} is not `<sha256>  <file>`", index + 1);
        }
        if sums.insert(name.to_owned(), hash.to_owned()).is_some() {
            bail!("{name} is listed more than once");
        }
    }
    Ok(sums)
}

fn is_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn expected_sha<'a>(sums: &'a BTreeMap<String, String>, tag: &str, asset: &str) -> Result<&'a str> {
    sums.get(asset).map(String::as_str).with_context(|| {
        format!("{SUMS_ASSET} for {tag} has no entry for {asset}; refusing the release")
    })
}

fn check_sha(bytes: &[u8], expected: &str, asset: &str) -> Result<()> {
    if hex(&Sha256::digest(bytes)) != expected {
        bail!("the downloaded {asset} does not match its SHA-256 in {SUMS_ASSET}; refusing it");
    }
    Ok(())
}

/// The release notes, printable: control characters (terminal escapes) removed,
/// and long notes cut with a pointer to the full text.
pub(crate) fn notes_lines(notes: &[u8], notes_url: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(notes);
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect()
        })
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() > NOTES_SHOWN_LINES {
        lines.truncate(NOTES_SHOWN_LINES);
        lines.push(format!("… the full notes are at {notes_url}"));
    }
    lines
}

// DECISION: no `.old` copy of the previous binary: it would sit on PATH as a second
// command. Rolling back is `install.sh --version <old>`.
/// Write `bytes` to a temporary file beside `target`, sync it, make it executable,
/// let `probe` check it, then rename it over `target`. Any failure before the
/// rename leaves `target` untouched and removes the temporary file.
pub(crate) fn replace_executable(
    target: &Path,
    bytes: &[u8],
    probe: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    replace_executable_with(target, bytes, probe, |file, bytes| file.write_all(bytes))
}

pub(crate) fn replace_executable_with(
    target: &Path,
    bytes: &[u8],
    probe: &dyn Fn(&Path) -> Result<()>,
    write: impl FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
) -> Result<()> {
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .context("the binary path has no parent directory")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("capscode");
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{name}.new-"))
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    write(temporary.as_file_mut(), bytes).context("writing the new binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .context("making the new binary executable")?;
    }
    temporary
        .as_file()
        .sync_all()
        .context("syncing the new binary")?;
    // Close the file before running it: Linux refuses to execute a file that is
    // still open for writing.
    let temporary = temporary.into_temp_path();
    probe(&temporary)?;
    temporary
        .persist(target)
        .map_err(|error| error.error)
        .with_context(|| format!("renaming the new binary over {}", target.display()))?;
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

// DECISION: run the new binary's `--version` before the rename, so a wrong-architecture
// or truncated download never replaces a working binary.
/// The production probe: on macOS the code signature must verify, and on every
/// platform the binary must run and report the expected version.
pub(crate) fn probe_release_binary(path: &Path, version: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        let mut codesign = Command::new("/usr/bin/codesign");
        codesign.arg("--verify").arg("--strict").arg(path);
        let output = run_with_timeout(&mut codesign, PROBE_TIMEOUT)
            .context("running codesign on the downloaded binary")?;
        if !output.status.success() {
            bail!("the downloaded binary's code signature does not verify (`codesign --verify --strict`); refusing it");
        }
    }
    let mut run = Command::new(path);
    run.arg("--version").env("CAPSCODE_NO_UPDATE_CHECK", "1");
    let output =
        run_with_timeout(&mut run, PROBE_TIMEOUT).context("running the downloaded binary")?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || reported.trim() != format!("capscode {version}") {
        bail!("the downloaded binary did not report version {version} when run; refusing it");
    }
    Ok(())
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("did not finish within {} seconds", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
