//! `capscode update --cli` tests. No network and no real `~/.capscode`: the
//! fetcher serves fixed bodies, and the "installed binary" and CapsCode home are
//! temporary paths.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use tar::EntryType;

use super::archive;
use super::channel::ChannelName;
use super::cli::*;
use super::fetch::Fetch;
use super::{hex, Confirmer};

const CHANNEL: &str = "sayre4ux/capscode";
const PLATFORM: &str = "linux-x86_64";
const OLD_BINARY: &[u8] = b"old capscode binary";

fn channel_name() -> ChannelName {
    ChannelName::parse(CHANNEL).unwrap()
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn manifest(version: &str, tag: &str, yanked: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "schema": 1,
        "generated_at": "2026-10-20T06:12:00Z",
        "cli": {
            "version": version,
            "tag": tag,
            "commit": "749dcba0c0e2",
            "min_toolchain_sequence": 1,
            "notes_url": format!("https://github.com/{CHANNEL}/releases/tag/{tag}"),
            "yanked_versions": yanked,
        },
        "toolchain": {
            "sequence": 1,
            "tag": "toolchain-1",
            "digest": "fb24882402b5",
            "min_cli_version": "0.1.0",
            "lock_url": format!("https://github.com/{CHANNEL}/releases/download/toolchain-1/toolchain.lock.toml"),
            "lock_sha256": "0".repeat(64),
            "attestation_url": format!("https://github.com/{CHANNEL}/releases/download/toolchain-1/toolchain.lock.toml.sigstore"),
            "yanked": []
        }
    })
}

// ---- archive fixtures ---------------------------------------------------------

fn raw_header(
    path: &str,
    kind: EntryType,
    size: u64,
    mode: u32,
    link: Option<&str>,
) -> tar::Header {
    // Written byte for byte: tar's safe setters refuse `..` and absolute paths,
    // which is exactly what these fixtures need to contain.
    let mut header = tar::Header::new_ustar();
    header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
    if let Some(link) = link {
        header.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
    }
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(mode);
    header.set_cksum();
    header
}

#[derive(Clone)]
enum Item {
    Dir(String),
    File(String, Vec<u8>, u32),
    Link(String, EntryType, String),
    Special(String, EntryType),
}

fn archive_of(items: &[Item]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for item in items {
        match item {
            Item::Dir(path) => builder
                .append(
                    &raw_header(path, EntryType::Directory, 0, 0o755, None),
                    &[][..],
                )
                .unwrap(),
            Item::File(path, data, mode) => builder
                .append(
                    &raw_header(path, EntryType::Regular, data.len() as u64, *mode, None),
                    &data[..],
                )
                .unwrap(),
            Item::Link(path, kind, target) => builder
                .append(&raw_header(path, *kind, 0, 0o777, Some(target)), &[][..])
                .unwrap(),
            Item::Special(path, kind) => builder
                .append(&raw_header(path, *kind, 0, 0o644, None), &[][..])
                .unwrap(),
        }
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn good_items(version: &str) -> Vec<Item> {
    let top = archive::top_level_dir(version);
    vec![
        Item::Dir(format!("{top}/")),
        Item::Dir(format!("{top}/scripts/")),
        Item::File(
            format!("{top}/scripts/build-images.sh"),
            b"#!/bin/sh\necho build\n".to_vec(),
            0o755,
        ),
        Item::File(
            format!("{top}/Cargo.toml"),
            b"[workspace]\n".to_vec(),
            0o644,
        ),
    ]
}

fn good_archive(version: &str) -> Vec<u8> {
    archive_of(&good_items(version))
}

// ---- release fixture ------------------------------------------------------------

struct Release {
    version: String,
    tag: String,
    yanked: Vec<String>,
    binary: Vec<u8>,
    context: Vec<u8>,
    notes: Vec<u8>,
    /// Edits applied to the SHA256SUMS text after it is generated.
    sums_edit: fn(String) -> String,
    /// Bytes served instead of the binary (to simulate tampering in transit).
    served_binary: Option<Vec<u8>>,
}

impl Release {
    fn new(version: &str) -> Self {
        Self {
            version: version.to_owned(),
            tag: format!("v{version}"),
            yanked: Vec::new(),
            binary: format!("new capscode {version}").into_bytes(),
            context: good_archive(version),
            notes: format!("## [{version}]\n\n### Fixed\n- Something.\n").into_bytes(),
            sums_edit: |text| text,
            served_binary: None,
        }
    }

    fn asset_url(&self, name: &str) -> String {
        channel_name().release_asset_url(&self.tag, name)
    }

    fn fetcher(&self) -> FakeFetcher {
        let binary_name = binary_asset(&self.version, PLATFORM);
        let context_name = context_asset(&self.version);
        let notes_name = notes_asset(&self.version);
        let sums = format!(
            "{}  {binary_name}\n{}  {context_name}\n{}  {notes_name}\n{}  capscode-{}-src.tar.gz\n",
            sha(&self.binary),
            sha(&self.context),
            sha(&self.notes),
            "a".repeat(64),
            self.version
        );
        let sums = (self.sums_edit)(sums);
        let yanked: Vec<&str> = self.yanked.iter().map(String::as_str).collect();
        let mut bodies = HashMap::new();
        bodies.insert(
            channel_name().manifest_url(),
            serde_json::to_vec(&manifest(&self.version, &self.tag, &yanked)).unwrap(),
        );
        bodies.insert(self.asset_url(SUMS_ASSET), sums.into_bytes());
        bodies.insert(
            self.asset_url(&binary_name),
            self.served_binary
                .clone()
                .unwrap_or_else(|| self.binary.clone()),
        );
        bodies.insert(self.asset_url(&context_name), self.context.clone());
        bodies.insert(self.asset_url(&notes_name), self.notes.clone());
        FakeFetcher {
            bodies,
            log: Mutex::new(Vec::new()),
        }
    }
}

struct FakeFetcher {
    bodies: HashMap<String, Vec<u8>>,
    log: Mutex<Vec<String>>,
}

impl FakeFetcher {
    fn requested(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Fetch for FakeFetcher {
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        self.log.lock().unwrap().push(url.to_owned());
        match self.bodies.get(url) {
            Some(body) if body.len() <= limit => Ok(body.clone()),
            Some(_) => Err(anyhow!("over limit")),
            None => Err(anyhow!("not found: {url}")),
        }
    }
}

struct Answer(bool, usize);

impl Confirmer for Answer {
    fn confirm(&mut self, _prompt: &str) -> Result<bool> {
        self.1 += 1;
        Ok(self.0)
    }
}

struct Install {
    _temp: tempfile::TempDir,
    exe: PathBuf,
    home: PathBuf,
}

fn install() -> Install {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("capscode");
    fs::write(&exe, OLD_BINARY).unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    Install {
        _temp: temp,
        exe,
        home,
    }
}

fn context(install: &Install) -> CliUpdateContext {
    CliUpdateContext {
        channel: channel_name(),
        verify_with_gh: false,
        gh_path: None,
        own_version: "0.1.0".to_owned(),
        exe: install.exe.clone(),
        home: install.home.clone(),
        platform: Some(PLATFORM),
        interactive: false,
    }
}

fn yes() -> CliUpdateOptions {
    CliUpdateOptions {
        check_only: false,
        yes: true,
        allow_downgrade: false,
    }
}

fn accept_probe(_path: &Path, _version: &str) -> Result<()> {
    Ok(())
}

struct Run {
    result: Result<CliOutcome>,
    said: Vec<String>,
    asked: usize,
}

fn run_with(
    ctx: &CliUpdateContext,
    fetcher: &FakeFetcher,
    options: CliUpdateOptions,
    answer: bool,
    probe: Probe<'_>,
) -> Run {
    let mut said = Vec::new();
    let mut answer = Answer(answer, 0);
    let result = run_cli_update(
        ctx,
        fetcher,
        options,
        &mut answer,
        &mut |line: &str| said.push(line.to_owned()),
        probe,
    );
    Run {
        result,
        said,
        asked: answer.1,
    }
}

fn run(ctx: &CliUpdateContext, fetcher: &FakeFetcher, options: CliUpdateOptions) -> Run {
    run_with(ctx, fetcher, options, false, &accept_probe)
}

fn refused(run: Run) -> String {
    format!(
        "{:#}",
        run.result
            .expect_err("expected the CLI update to be refused")
    )
}

fn assert_untouched(install: &Install) {
    assert_eq!(
        fs::read(&install.exe).unwrap(),
        OLD_BINARY,
        "the binary changed"
    );
    assert!(
        !install.home.join("build").join("0.2.0").exists(),
        "a build context was installed"
    );
    let leftovers: Vec<_> = fs::read_dir(install.exe.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "capscode")
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

// ---- happy path -------------------------------------------------------------------

#[test]
fn a_verified_release_replaces_the_binary_and_installs_its_build_context() {
    let install = install();
    let release = Release::new("0.2.0");
    let fetcher = release.fetcher();
    let probed = Mutex::new(Vec::new());
    let probe = |path: &Path, version: &str| -> Result<()> {
        // The probe sees the complete, executable new binary before the rename.
        probed
            .lock()
            .unwrap()
            .push((fs::read(path)?, version.to_owned(), path.to_path_buf()));
        Ok(())
    };
    let outcome = run_with(&context(&install), &fetcher, yes(), false, &probe);
    assert_eq!(
        outcome.result.unwrap(),
        CliOutcome::Installed {
            version: "0.2.0".to_owned()
        }
    );
    assert_eq!(fs::read(&install.exe).unwrap(), release.binary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&install.exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    let probed = probed.into_inner().unwrap();
    assert_eq!(probed.len(), 1);
    assert_eq!(probed[0].0, release.binary);
    assert_eq!(probed[0].1, "0.2.0");
    assert_eq!(
        probed[0].2.parent(),
        install.exe.parent(),
        "temp file is beside the target"
    );

    let unpacked = install.home.join("build").join("0.2.0");
    assert!(unpacked.join("scripts/build-images.sh").is_file());
    assert!(unpacked.join("Cargo.toml").is_file());
    let said = outcome.said.join("\n");
    assert!(said.contains("CapsCode 0.1.0 -> 0.2.0 (update)"), "{said}");
    assert!(said.contains("### Fixed"), "{said}");
    assert!(said.contains("capscode toolchain build"), "{said}");
    let requested = fetcher.requested();
    assert!(requested.iter().all(|url| url.starts_with("https://")));
    assert!(requested
        .iter()
        .any(|url| url.ends_with("/releases/download/v0.2.0/capscode-0.2.0-linux-x86_64")));
}

#[test]
fn an_existing_build_context_for_the_version_is_replaced_whole() {
    let install = install();
    let stale = install.home.join("build").join("0.2.0");
    fs::create_dir_all(stale.join("scripts")).unwrap();
    fs::write(stale.join("stale-file"), b"old").unwrap();
    let release = Release::new("0.2.0");
    run(&context(&install), &release.fetcher(), yes())
        .result
        .unwrap();
    assert!(stale.join("scripts/build-images.sh").is_file());
    assert!(!stale.join("stale-file").exists());
    let names: Vec<_> = fs::read_dir(install.home.join("build"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["0.2.0".to_owned()],
        "staging or old copies left behind"
    );
}

#[test]
fn check_only_shows_the_change_and_notes_and_writes_nothing() {
    let install = install();
    let release = Release::new("0.2.0");
    let fetcher = release.fetcher();
    let mut options = yes();
    options.check_only = true;
    let outcome = run(&context(&install), &fetcher, options);
    let said = outcome.said.join("\n");
    assert_eq!(
        outcome.result.unwrap(),
        CliOutcome::Available {
            version: "0.2.0".to_owned()
        }
    );
    assert!(said.contains("0.1.0 -> 0.2.0"), "{said}");
    assert!(said.contains("Something."), "{said}");
    assert_untouched(&install);
    assert!(
        !fetcher
            .requested()
            .iter()
            .any(|url| url.ends_with(PLATFORM)),
        "check downloaded the binary"
    );
}

#[test]
fn the_same_version_is_up_to_date_and_downloads_no_assets() {
    let install = install();
    let release = Release::new("0.1.0");
    let fetcher = release.fetcher();
    let outcome = run(&context(&install), &fetcher, yes());
    assert_eq!(outcome.result.unwrap(), CliOutcome::UpToDate);
    assert_eq!(fetcher.requested(), vec![channel_name().manifest_url()]);
    assert_eq!(fs::read(&install.exe).unwrap(), OLD_BINARY);
}

// ---- refusals ---------------------------------------------------------------------

#[test]
fn a_binary_that_does_not_match_its_checksum_is_refused() {
    let install = install();
    let mut release = Release::new("0.2.0");
    release.served_binary = Some(b"tampered capscode".to_vec());
    let error = refused(run(&context(&install), &release.fetcher(), yes()));
    assert!(
        error.contains("capscode-0.2.0-linux-x86_64 does not match its SHA-256 in SHA256SUMS"),
        "{error}"
    );
    assert_untouched(&install);
}

#[test]
fn a_build_context_that_does_not_match_its_checksum_is_refused() {
    let install = install();
    let mut release = Release::new("0.2.0");
    release.sums_edit = |text| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines[1] = format!("{}  capscode-0.2.0-build-context.tar.gz", "b".repeat(64));
        lines.join("\n")
    };
    let error = refused(run(&context(&install), &release.fetcher(), yes()));
    assert!(
        error.contains("build-context.tar.gz does not match"),
        "{error}"
    );
    assert_untouched(&install);
}

#[test]
fn a_release_whose_sha256sums_lacks_an_asset_is_refused() {
    for missing in [
        "capscode-0.2.0-linux-x86_64",
        "capscode-0.2.0-build-context.tar.gz",
        "capscode-0.2.0-release-notes.md",
    ] {
        let install = install();
        let mut release = Release::new("0.2.0");
        release.sums_edit = match missing {
            "capscode-0.2.0-linux-x86_64" => |text: String| {
                text.lines()
                    .filter(|line| !line.ends_with("linux-x86_64"))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            "capscode-0.2.0-build-context.tar.gz" => |text: String| {
                text.lines()
                    .filter(|line| !line.ends_with("build-context.tar.gz"))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            _ => |text: String| {
                text.lines()
                    .filter(|line| !line.ends_with("release-notes.md"))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        };
        let error = refused(run(&context(&install), &release.fetcher(), yes()));
        assert!(
            error.contains(&format!("SHA256SUMS for v0.2.0 has no entry for {missing}")),
            "{error}"
        );
        assert_untouched(&install);
    }
}

#[test]
fn a_prerelease_or_build_tagged_release_is_refused() {
    for version in ["0.2.0-rc.1", "0.2.0+build.5"] {
        let install = install();
        let release = Release::new(version);
        let fetcher = release.fetcher();
        let error = refused(run(&context(&install), &fetcher, yes()));
        assert!(error.contains("not a plain vX.Y.Z release"), "{error}");
        assert_eq!(fetcher.requested().len(), 1, "assets were fetched");
        assert_eq!(fs::read(&install.exe).unwrap(), OLD_BINARY);
    }
}

#[test]
fn a_tag_without_the_v_prefix_is_refused() {
    let install = install();
    let mut release = Release::new("0.2.0");
    release.tag = "0.2.0".to_owned();
    let error = refused(run(&context(&install), &release.fetcher(), yes()));
    assert!(error.contains("tag must be v<version>"), "{error}");
    assert_untouched(&install);
}

#[test]
fn a_release_the_channel_lists_as_withdrawn_is_refused() {
    let install = install();
    let mut release = Release::new("0.2.0");
    release.yanked = vec!["0.2.0".to_owned()];
    let error = refused(run(&context(&install), &release.fetcher(), yes()));
    assert!(error.contains("as withdrawn"), "{error}");
    assert_untouched(&install);
}

#[test]
fn a_downgrade_is_refused_without_the_flag() {
    let install = install();
    let release = Release::new("0.0.9");
    let fetcher = release.fetcher();
    let error = refused(run(&context(&install), &fetcher, yes()));
    assert!(error.contains("--allow-downgrade"), "{error}");
    assert_eq!(fs::read(&install.exe).unwrap(), OLD_BINARY);
    assert_eq!(fetcher.requested().len(), 1, "assets were fetched");

    let mut options = yes();
    options.allow_downgrade = true;
    let outcome = run(&context(&install), &fetcher, options);
    assert!(outcome
        .said
        .join("\n")
        .contains("0.1.0 -> 0.0.9 (downgrade)"));
    outcome.result.unwrap();
    assert_eq!(fs::read(&install.exe).unwrap(), release.binary);
}

#[test]
fn without_a_terminal_the_replacement_needs_yes() {
    let install = install();
    let release = Release::new("0.2.0");
    let fetcher = release.fetcher();
    let mut options = yes();
    options.yes = false;
    let outcome = run(&context(&install), &fetcher, options);
    assert_eq!(outcome.asked, 0);
    let error = refused(outcome);
    assert!(error.contains("rerun with `--yes`"), "{error}");
    assert_untouched(&install);
    assert!(
        !fetcher
            .requested()
            .iter()
            .any(|url| url.ends_with(PLATFORM)),
        "the binary was downloaded before confirmation"
    );
}

#[test]
fn an_interactive_update_asks_and_defaults_to_no() {
    let install = install();
    let release = Release::new("0.2.0");
    let mut ctx = context(&install);
    ctx.interactive = true;
    let mut options = yes();
    options.yes = false;
    let outcome = run_with(&ctx, &release.fetcher(), options, false, &accept_probe);
    assert_eq!(outcome.asked, 1);
    assert!(refused(outcome).contains("cancelled"));
    assert_untouched(&install);

    let outcome = run_with(&ctx, &release.fetcher(), options, true, &accept_probe);
    assert_eq!(outcome.asked, 1);
    outcome.result.unwrap();
    assert_eq!(fs::read(&install.exe).unwrap(), release.binary);
}

#[test]
fn a_source_build_is_refused_with_the_source_update_commands() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("capscode");
    let exe = checkout.join("target/release/capscode");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(&exe, OLD_BINARY).unwrap();
    let home = temp.path().join("home");
    let release = Release::new("0.2.0");
    let fetcher = release.fetcher();
    let mut ctx = context(&install());
    ctx.exe = exe.clone();
    ctx.home = home.clone();
    let error = refused(run(&ctx, &fetcher, yes()));
    assert!(error.contains("built from source"), "{error}");
    assert!(
        error.contains(&format!(
            "cd {} && git pull && ./scripts/install.sh",
            checkout.display()
        )),
        "{error}"
    );
    assert!(fetcher.requested().is_empty(), "the network was used");
    assert_eq!(fs::read(&exe).unwrap(), OLD_BINARY);
    assert!(!home.exists());
    // A `target` directory that is not inside a cargo workspace is an ordinary path.
    assert_eq!(
        source_checkout_of(Path::new("/opt/target/bin/capscode")),
        None
    );
}

#[test]
fn check_on_a_source_build_reports_the_release_and_the_git_command() {
    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("capscode");
    let exe = checkout.join("target/release/capscode");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::write(checkout.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(&exe, OLD_BINARY).unwrap();
    let git_command = format!(
        "cd {} && git pull && ./scripts/install.sh",
        checkout.display()
    );
    let check = CliUpdateOptions {
        check_only: true,
        yes: false,
        allow_downgrade: false,
    };
    let release = Release::new("0.2.0");
    let fetcher = release.fetcher();
    let mut ctx = context(&install());
    ctx.exe = exe.clone();
    // Even where no prebuilt binary exists, a source build can ask the channel.
    ctx.platform = None;
    let outcome = run(&ctx, &fetcher, check);
    assert_eq!(
        outcome.result.unwrap(),
        CliOutcome::Available {
            version: "0.2.0".to_owned()
        }
    );
    let said = outcome.said.join("\n");
    assert!(said.contains("CapsCode 0.2.0 is available"), "{said}");
    assert!(said.contains(&git_command), "{said}");
    assert!(!said.contains("capscode update --cli"), "{said}");
    assert_eq!(
        fetcher.requested(),
        vec![channel_name().manifest_url()],
        "only the manifest is fetched"
    );
    assert_eq!(fs::read(&exe).unwrap(), OLD_BINARY);

    ctx.own_version = "0.2.0".to_owned();
    let outcome = run(&ctx, &release.fetcher(), check);
    assert_eq!(outcome.result.unwrap(), CliOutcome::UpToDate);

    assert_eq!(update_command_for(Some(&exe)), git_command);
    assert_eq!(
        update_command_for(Some(Path::new("/usr/local/bin/capscode"))),
        "capscode update --cli"
    );
}

#[test]
fn a_cli_version_that_cannot_be_compared_is_refused() {
    let install = install();
    let mut ctx = context(&install);
    // Fits the SemVer shape and the length cap, but not in u64.
    ctx.own_version = "99999999999999999999999.0.0".to_owned();
    let release = Release::new("0.2.0");
    let error = refused(run(&ctx, &release.fetcher(), yes()));
    assert!(error.contains("comparing"), "{error}");
    assert_untouched(&install);
}

#[test]
fn a_platform_without_a_prebuilt_binary_is_refused() {
    let install = install();
    let mut ctx = context(&install);
    ctx.platform = None;
    let error = refused(run(&ctx, &Release::new("0.2.0").fetcher(), yes()));
    assert!(
        error.contains("no prebuilt binary for this platform"),
        "{error}"
    );
}

#[test]
fn a_binary_that_fails_its_probe_is_not_installed() {
    let install = install();
    let release = Release::new("0.2.0");
    let failing = |_: &Path, _: &str| -> Result<()> { Err(anyhow!("wrong architecture")) };
    let error = refused(run_with(
        &context(&install),
        &release.fetcher(),
        yes(),
        false,
        &failing,
    ));
    assert!(error.contains("wrong architecture"), "{error}");
    assert_eq!(fs::read(&install.exe).unwrap(), OLD_BINARY);
    let leftovers: Vec<_> = fs::read_dir(install.exe.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec!["capscode"]);
}

// ---- atomic replacement --------------------------------------------------------------

#[test]
fn a_write_that_fails_midway_leaves_the_old_binary_intact() {
    let install = install();
    let new_binary = vec![b'x'; 64 * 1024];
    let error = replace_executable_with(&install.exe, &new_binary, &|_| Ok(()), |file, bytes| {
        file.write_all(&bytes[..bytes.len() / 2])?;
        Err(std::io::Error::other("disk full"))
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("disk full"));
    assert_untouched(&install);

    // The same call with a working writer replaces the file.
    replace_executable(&install.exe, &new_binary, &|_| Ok(())).unwrap();
    assert_eq!(fs::read(&install.exe).unwrap(), new_binary);
}

#[cfg(unix)]
#[test]
fn the_production_probe_runs_the_new_binary_and_checks_its_version() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("capscode-new");
    fs::write(&script, "#!/bin/sh\necho \"capscode 0.2.0\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    if cfg!(target_os = "macos") {
        // The script carries no code signature, so macOS refuses it before running it.
        let error = probe_release_binary(&script, "0.2.0")
            .unwrap_err()
            .to_string();
        assert!(error.contains("code signature does not verify"), "{error}");
        return;
    }
    probe_release_binary(&script, "0.2.0").unwrap();
    let error = probe_release_binary(&script, "0.3.0")
        .unwrap_err()
        .to_string();
    assert!(error.contains("did not report version 0.3.0"), "{error}");
}

// ---- SHA256SUMS and notes ---------------------------------------------------------------

#[test]
fn sha256sums_lines_are_parsed_strictly() {
    let good = format!(
        "{}  capscode-0.2.0-linux-x86_64\n{} *SHA-asset.tar.gz\n",
        "a".repeat(64),
        "b".repeat(64)
    );
    let sums = parse_sums(good.as_bytes()).unwrap();
    assert_eq!(sums.len(), 2);
    assert_eq!(sums["SHA-asset.tar.gz"], "b".repeat(64));
    for bad in [
        format!("{}  capscode", "A".repeat(64)),
        format!("{} capscode", "a".repeat(64)),
        format!("{}  ../capscode", "a".repeat(64)),
        format!("{}  .hidden", "a".repeat(64)),
        format!("{}  capscode\n{}  capscode", "a".repeat(64), "b".repeat(64)),
        "short  capscode".to_owned(),
    ] {
        assert!(parse_sums(bad.as_bytes()).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn release_notes_are_stripped_of_terminal_escapes_and_capped() {
    let notes =
        "## [0.2.0]\n\x1b]0;owned\x07- fix\x1b[31m red\n".to_owned() + &"line\n".repeat(200);
    let lines = notes_lines(
        notes.as_bytes(),
        "https://github.com/sayre4ux/capscode/releases/tag/v0.2.0",
    );
    assert!(lines
        .iter()
        .all(|line| !line.chars().any(|c| c.is_control() && c != '\t')));
    assert_eq!(lines[1], "]0;owned- fix[31m red");
    assert_eq!(lines.len(), 61);
    assert!(lines[60].contains("releases/tag/v0.2.0"));
}

// ---- build context archive ----------------------------------------------------------------

fn extract_error(items: &[Item]) -> String {
    let temp = tempfile::tempdir().unwrap();
    let build = temp.path().join("build");
    let error = archive::install_build_context(&archive_of(items), "0.2.0", &build)
        .expect_err("the archive should be refused");
    assert!(
        !build.join("0.2.0").exists(),
        "a refused archive was installed"
    );
    assert!(
        !temp.path().join("x").exists() && !temp.path().join("escaped").exists(),
        "an entry escaped the staging directory"
    );
    format!("{error:#}")
}

fn with(extra: Item) -> Vec<Item> {
    let mut items = good_items("0.2.0");
    items.push(extra);
    items
}

#[test]
fn a_good_build_context_unpacks_with_modes() {
    let temp = tempfile::tempdir().unwrap();
    let build = temp.path().join("build");
    let dir = archive::install_build_context(&good_archive("0.2.0"), "0.2.0", &build).unwrap();
    assert_eq!(dir, build.join("0.2.0"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("scripts/build-images.sh")), 0o755);
        assert_eq!(mode(&dir.join("Cargo.toml")), 0o644);
        assert_eq!(mode(&build), 0o700);
    }
}

#[test]
fn an_entry_with_a_parent_component_is_refused() {
    let top = archive::top_level_dir("0.2.0");
    let error = extract_error(&with(Item::File(
        format!("{top}/../../x"),
        b"x".to_vec(),
        0o644,
    )));
    assert!(error.contains("`..` is not allowed"), "{error}");
    let error = extract_error(&with(Item::File("../x".to_owned(), b"x".to_vec(), 0o644)));
    assert!(error.contains("must be under"), "{error}");
}

#[test]
fn an_absolute_entry_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("escaped");
    let error = extract_error(&with(Item::File(
        target.display().to_string(),
        b"x".to_vec(),
        0o644,
    )));
    assert!(error.contains("absolute paths are not allowed"), "{error}");
    assert!(!target.exists());
}

#[test]
fn links_are_refused_wherever_they_point() {
    let top = archive::top_level_dir("0.2.0");
    for (kind, target) in [
        (EntryType::Symlink, "../../../escaped"),
        (EntryType::Symlink, "/etc"),
        (EntryType::Symlink, "scripts/build-images.sh"),
        (EntryType::Link, "../../../escaped"),
    ] {
        let error = extract_error(&with(Item::Link(
            format!("{top}/scripts/link"),
            kind,
            target.to_owned(),
        )));
        assert!(error.contains("links are not allowed"), "{error}");
    }
    // A link followed by a file written "through" it never gets that far.
    let mut items = good_items("0.2.0");
    items.insert(
        1,
        Item::Link(format!("{top}/out"), EntryType::Symlink, "/tmp".to_owned()),
    );
    items.insert(
        2,
        Item::File(format!("{top}/out/escaped"), b"x".to_vec(), 0o644),
    );
    assert!(extract_error(&items).contains("links are not allowed"));
}

#[test]
fn device_and_fifo_entries_are_refused() {
    let top = archive::top_level_dir("0.2.0");
    for kind in [EntryType::Char, EntryType::Block, EntryType::Fifo] {
        let error = extract_error(&with(Item::Special(format!("{top}/dev"), kind)));
        assert!(error.contains("device and special files"), "{error}");
    }
}

#[test]
fn duplicate_entries_and_wrong_top_levels_are_refused() {
    let top = archive::top_level_dir("0.2.0");
    let error = extract_error(&with(Item::File(
        format!("{top}/Cargo.toml"),
        b"second".to_vec(),
        0o644,
    )));
    assert!(error.contains("Cargo.toml"), "{error}");
    let error = extract_error(&[Item::File(
        format!(
            "{}/scripts/build-images.sh",
            archive::top_level_dir("0.3.0")
        ),
        b"x".to_vec(),
        0o755,
    )]);
    assert!(error.contains("must be under"), "{error}");
    let error = extract_error(&[Item::File(format!("{top}/README"), b"x".to_vec(), 0o644)]);
    assert!(error.contains("no scripts/build-images.sh"), "{error}");
}

#[test]
fn a_bad_archive_leaves_an_earlier_build_context_in_place() {
    let temp = tempfile::tempdir().unwrap();
    let build = temp.path().join("build");
    archive::install_build_context(&good_archive("0.2.0"), "0.2.0", &build).unwrap();
    let top = archive::top_level_dir("0.2.0");
    let bad = archive_of(&with(Item::File(
        format!("{top}/../x"),
        b"x".to_vec(),
        0o644,
    )));
    assert!(archive::install_build_context(&bad, "0.2.0", &build).is_err());
    assert!(build.join("0.2.0/scripts/build-images.sh").is_file());
    let names: Vec<_> = fs::read_dir(&build)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["0.2.0".to_owned()]);
}

#[test]
fn a_version_string_that_is_not_a_plain_component_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    for version in ["..", "../x", "a/b", ""] {
        assert!(
            archive::install_build_context(&good_archive("0.2.0"), version, temp.path()).is_err()
        );
    }
}

// ---- gh attestation ---------------------------------------------------------------------

#[cfg(unix)]
fn fake_gh(dir: &Path, exit_code: i32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("gh-args.txt");
    let script = dir.join("gh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" >> '{}'\nprintf '\\n' >> '{}'\nexit {exit_code}\n",
            log.display(),
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    log
}

#[cfg(unix)]
#[test]
fn verify_with_gh_fails_closed_and_pins_the_cli_release_workflow() {
    let release = Release::new("0.2.0");

    let install_missing = install();
    let mut ctx = context(&install_missing);
    ctx.verify_with_gh = true;
    let empty = tempfile::tempdir().unwrap();
    ctx.gh_path = Some(empty.path().as_os_str().to_owned());
    let error = refused(run(&ctx, &release.fetcher(), yes()));
    assert!(error.contains("`gh` is not on PATH"), "{error}");
    assert_untouched(&install_missing);

    let install_rejected = install();
    let mut ctx = context(&install_rejected);
    ctx.verify_with_gh = true;
    let rejecting = tempfile::tempdir().unwrap();
    let log = fake_gh(rejecting.path(), 1);
    ctx.gh_path = Some(rejecting.path().as_os_str().to_owned());
    let error = refused(run(&ctx, &release.fetcher(), yes()));
    assert!(error.contains("rejected the downloaded binary"), "{error}");
    assert_untouched(&install_rejected);
    let args = fs::read_to_string(&log).unwrap();
    assert!(args.starts_with("attestation verify "), "{args}");
    assert!(
        args.contains("capscode-0.2.0-linux-x86_64 --repo sayre4ux/capscode"),
        "{args}"
    );
    assert!(
        args.contains("--signer-workflow sayre4ux/capscode/.github/workflows/release-cli.yml"),
        "{args}"
    );

    let install_accepted = install();
    let mut ctx = context(&install_accepted);
    ctx.verify_with_gh = true;
    let accepting = tempfile::tempdir().unwrap();
    let log = fake_gh(accepting.path(), 0);
    ctx.gh_path = Some(accepting.path().as_os_str().to_owned());
    let outcome = run(&ctx, &release.fetcher(), yes());
    outcome.result.unwrap();
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(calls.lines().count(), 2, "{calls}");
    assert!(
        calls.contains("capscode-0.2.0-build-context.tar.gz --repo"),
        "{calls}"
    );
    assert_eq!(fs::read(&install_accepted.exe).unwrap(), release.binary);
}

#[test]
fn the_archive_the_release_script_builds_unpacks_through_this_reader() {
    // Producer and consumer must agree on the format (ustar from the system tar).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join(".git").exists() {
        return; // A source tarball has no git metadata for the script to read.
    }
    let temp = tempfile::tempdir().unwrap();
    let out = temp.path().join("context.tar.gz");
    let status = std::process::Command::new("bash")
        .arg(root.join("scripts/release/build-context.sh"))
        .arg("--version")
        .arg("0.2.0")
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let build = temp.path().join("build");
    let dir = archive::install_build_context(&fs::read(&out).unwrap(), "0.2.0", &build).unwrap();
    for file in [
        "scripts/build-images.sh",
        "scripts/smoke-tui.sh",
        "images/gateway.Dockerfile",
        "images/toolchain.lock.toml",
        "crates/broker/Cargo.toml",
        "providers/registry.yaml",
        "Cargo.lock",
    ] {
        assert!(dir.join(file).is_file(), "{file} missing");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("scripts/build-images.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}
