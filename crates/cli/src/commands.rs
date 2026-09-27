use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::io::{self, BufRead, IsTerminal, Read};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use capscode_broker::SessionCa;
use capscode_core::{
    ApiStyle, AuthScheme, Config, EgressPlan, EgressReport, ExecSpec, GitEngine, ImageRef,
    IngressReceipt, LocalImageInfo, NetContext, NetworkStack, OwnedObject, OwnedObjectKind,
    Provider, ProviderCredential, ProviderDefaults, ProviderSpec, RuntimeAdapter, RuntimeKind,
    RuntimePref, SessionId, Tool, LABEL_BOX, LABEL_CONTRACT, LABEL_OWNER, LABEL_OWNER_VALUE,
    LABEL_ROLE, ROLE_CACHE, ROLE_CAPSULE, ROLE_GATEWAY,
};
use capscode_git::DefaultGitEngine;
use capscode_net::NetStack;
use dialoguer::{Confirm, Input, Password, Select};
use git2::Repository;
use secrecy::SecretString;
use serde_json::json;
use tokio::process::Command as TokioCommand;
use tokio::sync::watch;
use zeroize::Zeroizing;

use clap_complete::Shell;

use crate::args::{
    ApiStyleArg, AuthSchemeArg, ConfigKeyArg, ProviderAddArgs, RunArgs, RuntimeArg, ToolArg,
    ToolchainCommand,
};
use crate::config::{self, runtime_name};
use crate::keystore;
use crate::session::shadow::ShadowOutcome;
use crate::session::{
    broker_aliases, inject_gateway_secrets, provider_manifest_json, read_gateway_ca,
    set_container_env, SessionDriver, SessionOptions,
};
use crate::ui::{data, glyph, kv, note, rule, styled, table, Caps, Status};

pub fn completions(shell: Shell) -> Result<()> {
    crate::shell_completion::write_script(shell, io::stdout())
}

pub(crate) async fn toolchain(
    command: ToolchainCommand,
    runtime_flag: Option<RuntimeArg>,
) -> Result<()> {
    match command {
        ToolchainCommand::Show => toolchain_show(),
        ToolchainCommand::Ls => toolchain_ls(runtime_flag).await,
        ToolchainCommand::Prune {
            keep,
            yes,
            build_cache,
        } => toolchain_prune(runtime_flag, keep, yes, build_cache).await,
        ToolchainCommand::Build { digest, no_smoke } => {
            toolchain_build(runtime_flag, digest.as_deref(), no_smoke).await
        }
        ToolchainCommand::Use { digest, force } => toolchain_use(&digest, force),
        ToolchainCommand::Pin {
            tool_version,
            untested,
            integrity,
        } => {
            let _ = runtime_flag;
            toolchain_pin(&tool_version, untested, integrity.as_deref()).await
        }
    }
}

fn toolchain_show() -> Result<()> {
    let config = config::load(None)?;
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let digest = crate::toolchain::digest(&resolved.lock);
    let yanks = crate::update::known_yanks(&config, &resolved.lock);
    let yanked = crate::toolchain::yanked_by(&digest, Some(&resolved.lock), &yanks);
    let mut lines = vec![format!(
        "Toolchain {digest} ({}){}{}",
        resolved.source,
        if resolved.lock.untested {
            " — untested"
        } else {
            ""
        },
        yanked
            .as_deref()
            .map(|entry| format!(" — withdrawn ({entry}); run `capscode update`"))
            .unwrap_or_default()
    )];
    if let Some(sequence) = resolved.lock.sequence {
        lines.push(format!(
            "Release sequence {sequence}{}",
            resolved
                .lock
                .min_cli_version
                .as_deref()
                .map(|version| format!(", needs CapsCode {version} or newer"))
                .unwrap_or_default()
        ));
    }
    lines.extend(crate::update::provenance_lines(
        &crate::toolchain::config_home(),
        &digest,
    ));
    for (name, tool) in &resolved.lock.tools {
        let published = tool.published_at.as_deref().unwrap_or("unknown");
        let smoke_date = tool
            .smoke_date
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("not recorded");
        let mut details = format!(
            "{name} {} | published {published} | dependencies before {} | smoke {} ({smoke_date})",
            tool.version, tool.resolved_before, tool.smoke
        );
        if let Some(reason) = tool.cooldown_waived.as_deref() {
            if !reason.is_empty() {
                details.push_str(&format!(" | cooldown waived: {reason}"));
            }
        }
        if let Some(reason) = tool.downgrade_reason.as_deref() {
            if !reason.is_empty() {
                details.push_str(&format!(" | downgrade: {reason}"));
            }
        }
        lines.push(details);
    }
    data(lines.join("\n"));
    Ok(())
}

async fn toolchain_ls(runtime_flag: Option<RuntimeArg>) -> Result<()> {
    let runtime = selected_runtime(runtime_flag).await?;
    let config = config::load(None)?;
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let current = crate::toolchain::digest(&resolved.lock);
    let snapshots = crate::toolchain::list_snapshots()?;
    let images = runtime.list_local_image_details().await?;
    let boxes = crate::boxes::index::read()?;
    let mut lines = vec![format!("Current toolchain: {current}")];
    lines.push("Snapshots:".to_owned());
    if snapshots.is_empty() {
        lines.push("  (none saved)".to_owned());
    }
    for snapshot in snapshots {
        let refs = boxes
            .iter()
            .filter(|entry| entry.toolchain == snapshot.digest)
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        let image = crate::toolchain::capsule_all_image(&snapshot.digest);
        let image_info = images
            .iter()
            .find(|image_info| image_info.reference == image);
        let size = image_info
            .and_then(|image| image.size_bytes)
            .map(format_size)
            .unwrap_or_else(|| "not built".to_owned());
        let built = image_info
            .and_then(|image| image.created_unix)
            .map(unix_date)
            .unwrap_or_else(|| "unknown".to_owned());
        let markers = [
            (snapshot.digest == current).then_some("current"),
            snapshot.lock.untested.then_some("untested"),
            image_info
                .is_some_and(|image| !image_contract_is_current(image))
                .then_some("rebuild needed"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        lines.push(format!(
            "  {}{} | {size} | built {built} | boxes: {}",
            snapshot.digest,
            if markers.is_empty() {
                String::new()
            } else {
                format!(" ({markers})")
            },
            if refs.is_empty() {
                "-".to_owned()
            } else {
                refs.join(", ")
            }
        ));
    }
    lines.push("Local images:".to_owned());
    for image in images
        .iter()
        .filter(|image| image.reference.starts_with("capscode/"))
    {
        let ownership = if !image_created_by_capscode(image) {
            " (not created by CapsCode)"
        } else if !image_contract_is_current(image)
            && image.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CAPSULE)
        {
            " (rebuild needed)"
        } else {
            ""
        };
        lines.push(format!(
            "  {} | {} | built {}{}",
            image.reference,
            image
                .size_bytes
                .map(format_size)
                .unwrap_or_else(|| "size unknown".to_owned()),
            image
                .created_unix
                .map(unix_date)
                .unwrap_or_else(|| "unknown".to_owned()),
            ownership
        ));
    }
    data(lines.join("\n"));
    Ok(())
}

async fn toolchain_prune(
    runtime_flag: Option<RuntimeArg>,
    keep: usize,
    yes: bool,
    build_cache: bool,
) -> Result<()> {
    let runtime = selected_runtime(runtime_flag).await?;
    let config = config::load(None)?;
    let active = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let current = crate::toolchain::digest(&active.lock);
    let snapshots = crate::toolchain::list_snapshots()?;
    let objects = runtime.list_owned().await?;
    let entries = crate::boxes::index::read()?;
    let referenced = referenced_toolchains(&entries, &objects);
    let protected = crate::toolchain::protected_snapshots(&snapshots, &current, &referenced, keep);
    let remove_snapshots: Vec<_> = snapshots
        .iter()
        .filter(|snapshot| !protected.contains(&snapshot.digest))
        .collect();

    let mut protected_images = HashSet::new();
    protect_lock_images(&mut protected_images, &active.lock, &current);
    for snapshot in &snapshots {
        if protected.contains(&snapshot.digest) {
            protect_lock_images(&mut protected_images, &snapshot.lock, &snapshot.digest);
        }
    }
    for digest in &referenced {
        protected_images.insert(crate::toolchain::capsule_all_image(digest));
    }
    for object in &objects {
        if object.kind == OwnedObjectKind::Container && object.role.as_deref() == Some(ROLE_CAPSULE)
        {
            if let Some(image) = &object.image {
                protected_images.insert(image.clone());
            }
        }
    }
    if let Ok(image) = std::env::var("CAPSCODE_CAPSULE_ALL_IMAGE") {
        protected_images.insert(image);
    }
    let images = runtime.list_local_image_details().await?;
    for image in images.iter().filter(|image| {
        image.reference.starts_with("capscode/") && !image_created_by_capscode(image)
    }) {
        note(format!(
            "Leaving image {} alone because it was not created by CapsCode.",
            image.reference
        ));
    }
    let remove: Vec<_> = images
        .into_iter()
        .filter(is_capsule_toolchain_image)
        .filter(|image| !protected_images.contains(&image.reference))
        .collect();
    if remove.is_empty() && remove_snapshots.is_empty() {
        data("No unreferenced CapsCode toolchain snapshots or images to prune.");
        if build_cache {
            note("Build cache guidance: `docker builder prune --filter label=capscode` (review its effect before running).");
        }
        return Ok(());
    }
    let image_total: u64 = remove.iter().filter_map(|image| image.size_bytes).sum();
    let snapshot_total: u64 = remove_snapshots
        .iter()
        .filter_map(|snapshot| snapshot.path.metadata().ok().map(|metadata| metadata.len()))
        .sum();
    let total = image_total.saturating_add(snapshot_total);
    let unknown_sizes = remove
        .iter()
        .filter(|image| image.size_bytes.is_none())
        .count();
    let size_summary = if unknown_sizes == 0 {
        format_size(total)
    } else {
        format!(
            "at least {} ({} image sizes unknown)",
            format_size(total),
            unknown_sizes
        )
    };
    note(format!(
        "Prune {} unreferenced snapshot(s) and {} image(s), freeing {size_summary}?",
        remove_snapshots.len(),
        remove.len()
    ));
    for snapshot in &remove_snapshots {
        note(format!("  snapshot {}", snapshot.path.display()));
    }
    for image in &remove {
        note(format!("  image {}", image.reference));
    }
    if !yes {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!(
                "toolchain prune needs a terminal; rerun with `--yes` to remove the listed images"
            );
        }
        if !Confirm::with_theme(&crate::ui::theme())
            .with_prompt("Remove these images?")
            .default(false)
            .interact()
            .context("reading prune confirmation")?
        {
            bail!("toolchain prune cancelled");
        }
    }
    for image in &remove {
        runtime.remove_image(&image.reference).await?;
        note(format!("Removed {}", image.reference));
    }
    for snapshot in remove_snapshots {
        std::fs::remove_file(&snapshot.path)
            .with_context(|| format!("removing snapshot {}", snapshot.path.display()))?;
        note(format!("Removed snapshot {}", snapshot.digest));
    }
    if build_cache {
        note("Build cache guidance: `docker builder prune --filter label=capscode` (review its effect before running).");
    }
    Ok(())
}

async fn toolchain_build(
    runtime_flag: Option<RuntimeArg>,
    digest: Option<&str>,
    no_smoke: bool,
) -> Result<()> {
    let config = config::load(None)?;
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let current_digest = crate::toolchain::digest(&resolved.lock);
    let files = crate::build_context::locate()?;
    let (lock_path, digest) = if let Some(digest) = digest {
        let snapshot = crate::toolchain::read_snapshot(digest)?;
        (snapshot.path, snapshot.digest)
    } else {
        let builtin = crate::toolchain::builtin_lock()?;
        let path =
            if current_digest == crate::toolchain::digest(&builtin) && resolved.path.is_none() {
                // DECISION: the built-in lock is compiled in. Build from the copy shipped with the
                // build files when it is the same toolchain; otherwise save the compiled-in
                // lock as a snapshot and build from that.
                match files.lock_for_digest(&current_digest) {
                    Some(path) => path,
                    None => crate::toolchain::write_snapshot(&builtin, &current_digest)?,
                }
            } else if let Some(path) = resolved.path {
                path
            } else {
                bail!("could not locate the active toolchain lock; run `capscode toolchain show`");
            };
        (path, current_digest)
    };
    let runtime = selected_runtime(runtime_flag).await?;
    run_build_images(&files, &lock_path, no_smoke).await?;
    let image = ImageRef(crate::toolchain::capsule_all_image(&digest));
    if runtime.image_present(&image).await? {
        let lock = crate::toolchain::read_validated(&lock_path)?;
        crate::toolchain::write_snapshot(&lock, &digest)?;
    }
    data(format!("Built toolchain {digest}"));
    Ok(())
}

fn toolchain_use(digest: &str, force: bool) -> Result<()> {
    if digest == "built-in" {
        crate::toolchain::remove_runtime_lock()?;
        data("Using the built-in toolchain lock.");
        return Ok(());
    }
    let config = config::load(None)?;
    let active = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let yanks = crate::update::known_yanks(&config, &active.lock);
    let paths = crate::update::UpdatePaths::in_home(&crate::toolchain::config_home());
    for line in toolchain_use_in(&paths, digest, force, &yanks)? {
        note(line);
    }
    data(format!("Using toolchain {digest} as the global default."));
    Ok(())
}

/// Select a saved snapshot as the runtime lock, refusing a withdrawn one unless forced.
pub(crate) fn toolchain_use_in(
    paths: &crate::update::UpdatePaths,
    digest: &str,
    force: bool,
    yanks: &[String],
) -> Result<Vec<String>> {
    let snapshot = crate::toolchain::read_snapshot_in(&paths.snapshots, digest)?;
    let warning = crate::update::refuse_yanked(digest, Some(&snapshot.lock), yanks, force)?;
    crate::toolchain::write_runtime_lock_at(&paths.runtime_lock, &snapshot.lock)?;
    Ok(warning.into_iter().collect())
}

async fn toolchain_pin(spec: &str, untested: bool, integrity: Option<&str>) -> Result<()> {
    let (tool_name, version) = spec
        .split_once('@')
        .filter(|(name, version)| !name.is_empty() && !version.is_empty())
        .context("pin expects <tool>@<version>, for example codex@0.159.3")?;
    require_untested_pin(untested)?;
    if !crate::toolchain::is_semver(version) {
        bail!("toolchain version must use SemVer, for example 1.2.3");
    }
    let config = config::load(None)?;
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let mut lock = resolved.lock;
    let tool = lock
        .tools
        .get_mut(tool_name)
        .with_context(|| format!("unknown tool '{tool_name}'"))?;
    // DECISION: pin requires caller-supplied npm integrity so it does not widen the update
    // fetcher's provider allowlist or create a second package-download trust path.
    let integrity = integrity.with_context(|| {
        format!(
            "integrity is required; fetch it with `npm view {}@{} dist.integrity` and pass `--integrity`",
            tool.package, version
        )
    })?;
    crate::toolchain::validate_integrity_for_tool(tool_name, integrity)?;
    apply_toolchain_pin(tool, version, integrity, current_utc_timestamp()?);
    lock.untested = true;
    let digest = crate::toolchain::digest(&lock);
    let snapshot = crate::toolchain::write_snapshot(&lock, &digest)?;
    let files = crate::build_context::locate()?;
    let build_result = run_build_images(&files, &snapshot, false).await;
    activate_runtime_lock_after_build(build_result, &lock, &crate::toolchain::runtime_lock_path())?;
    data(format!(
        "Pinned {tool_name}@{version} in untested toolchain {digest}"
    ));
    Ok(())
}

pub(crate) fn apply_toolchain_pin(
    tool: &mut crate::toolchain::LockedTool,
    version: &str,
    integrity: &str,
    resolved_before: String,
) {
    tool.version = version.to_owned();
    tool.integrity = integrity.to_owned();
    tool.resolved_before = resolved_before;
    tool.smoke = crate::toolchain::SmokeGrade::None;
    tool.smoke_date = Some(String::new());
    tool.smoke_arch.clear();
    tool.published_at = None;
    tool.cooldown_waived = None;
}

fn current_utc_timestamp() -> Result<String> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("reading the current time for the toolchain pin")?
        .as_secs();
    let seconds = i64::try_from(seconds).context("current time is outside the supported range")?;
    Ok(format!(
        "{}:{:02}Z",
        unix_date(seconds).trim_end_matches('Z'),
        seconds.rem_euclid(60)
    ))
}

fn activate_runtime_lock_after_build(
    build_result: Result<()>,
    lock: &crate::toolchain::ToolchainLock,
    runtime_path: &Path,
) -> Result<()> {
    build_result?;
    crate::toolchain::write_runtime_lock_at(runtime_path, lock)
}

fn require_untested_pin(untested: bool) -> Result<()> {
    if !untested {
        bail!("toolchain pin requires `--untested` because this version set has not passed a session smoke");
    }
    Ok(())
}

async fn run_build_images(
    files: &crate::build_context::BuildFiles,
    lock: &Path,
    no_smoke: bool,
) -> Result<()> {
    let script = &files.script;
    note(files.describe());
    let mut command = TokioCommand::new("bash");
    command.arg(script).args(build_script_args(lock, no_smoke));
    let status = command
        .status()
        .await
        .with_context(|| format!("starting image build script {}", script.display()))?;
    if !status.success() {
        bail!("toolchain image build failed; inspect the build output and retry, or run `capscode toolchain build --no-smoke` to skip the startup smoke");
    }
    Ok(())
}

fn build_script_args(lock: &Path, no_smoke: bool) -> Vec<std::ffi::OsString> {
    let mut args = vec!["--lock".into(), lock.as_os_str().to_owned()];
    if no_smoke {
        args.push("--no-smoke".into());
    }
    args
}

pub(crate) fn runtime_kind_label(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::Docker => "Docker",
        RuntimeKind::OrbStack => "OrbStack",
        RuntimeKind::AppleContainer => "Apple Container",
    }
}

fn referenced_toolchains(
    entries: &[crate::boxes::index::Entry],
    objects: &[OwnedObject],
) -> HashSet<String> {
    let mut referenced: HashSet<String> = entries
        .iter()
        .filter(|entry| crate::toolchain::is_digest(&entry.toolchain))
        .map(|entry| entry.toolchain.clone())
        .collect();
    for object in objects.iter().filter(|object| {
        object.kind == OwnedObjectKind::Container
            && object.role.as_deref() == Some(ROLE_CAPSULE)
            && object.labels.contains_key(LABEL_BOX)
    }) {
        let digest = object
            .labels
            .get(capscode_core::LABEL_TOOLCHAIN)
            .filter(|value| crate::toolchain::is_digest(value))
            .cloned()
            .or_else(|| object.image.as_deref().and_then(image_digest));
        if let Some(digest) = digest {
            referenced.insert(digest);
        }
    }
    referenced
}

fn image_digest(image: &str) -> Option<String> {
    let digest = image.strip_prefix("capscode/capsule-all:")?;
    crate::toolchain::is_digest(digest).then(|| digest.to_owned())
}

fn protect_lock_images(
    protected: &mut HashSet<String>,
    lock: &crate::toolchain::ToolchainLock,
    digest: &str,
) {
    protected.insert(crate::toolchain::capsule_all_image(digest));
    for name in ["claude", "codex", "opencode", "pi", "omp"] {
        if let Some(tool) = lock.tools.get(name) {
            protected.insert(format!("capscode/capsule-{name}:{}", tool.version));
        }
    }
}

fn is_toolchain_image(reference: &str) -> bool {
    reference.starts_with("capscode/capsule-") && !reference.starts_with("capscode/capsule-base:")
}

fn image_created_by_capscode(image: &LocalImageInfo) -> bool {
    image.labels.get(LABEL_OWNER).map(String::as_str) == Some(LABEL_OWNER_VALUE)
        && matches!(
            image.labels.get(LABEL_ROLE).map(String::as_str),
            Some(ROLE_CAPSULE | ROLE_GATEWAY | "base")
        )
}

fn is_capsule_toolchain_image(image: &LocalImageInfo) -> bool {
    is_toolchain_image(&image.reference)
        && image_created_by_capscode(image)
        && image.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CAPSULE)
}

pub(crate) fn image_contract_is_current(image: &LocalImageInfo) -> bool {
    image_created_by_capscode(image)
        && image.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CAPSULE)
        && image
            .labels
            .get(LABEL_CONTRACT)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|version| version >= crate::toolchain::IMAGE_CONTRACT_VERSION)
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

fn unix_date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400) as u64;
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
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}Z",
        day_seconds / 3600,
        (day_seconds / 60) % 60
    )
}

pub(crate) async fn sync_then_record<S, SFut, R, RFut>(sync: S, record: R) -> Result<()>
where
    S: FnOnce() -> SFut,
    SFut: Future<Output = Result<Option<crate::boxes::SyncIdentity>>>,
    R: FnOnce(crate::boxes::SyncIdentity) -> RFut,
    RFut: Future<Output = Result<()>>,
{
    if let Some(identity) = sync().await? {
        record(identity).await?;
    }
    Ok(())
}

pub fn config_list(as_json: bool) -> Result<()> {
    config::config_list(as_json)
}

pub fn config_get(key: ConfigKeyArg) -> Result<()> {
    config::config_get(key)
}

pub fn config_set(key: ConfigKeyArg, value: &str) -> Result<()> {
    config::config_set(key, value)
}

pub fn config_unset(key: ConfigKeyArg) -> Result<()> {
    config::config_unset(key)
}

pub fn config_edit() -> Result<()> {
    config::config_edit()
}

pub(crate) async fn selected_runtime(
    runtime_flag: Option<RuntimeArg>,
) -> Result<Arc<dyn RuntimeAdapter>> {
    let config = config::load(runtime_flag)?;
    let (runtime, endpoint_error) = selected_runtime_pref(config.runtime).await?;
    if let Some(error) = endpoint_error {
        return Err(error);
    }
    Ok(runtime)
}

async fn selected_runtime_pref(
    runtime_pref: RuntimePref,
) -> Result<(Arc<dyn RuntimeAdapter>, Option<anyhow::Error>)> {
    let runtime: Arc<dyn RuntimeAdapter> = Arc::from(capscode_runtime::detect(runtime_pref).await?);
    Ok(prepare_selected_runtime(runtime).await)
}

async fn prepare_selected_runtime(
    runtime: Arc<dyn RuntimeAdapter>,
) -> (Arc<dyn RuntimeAdapter>, Option<anyhow::Error>) {
    let endpoint_error = set_docker_cli_host(runtime.as_ref()).await.err();
    (runtime, endpoint_error)
}

pub async fn run(args: RunArgs, runtime_flag: Option<RuntimeArg>, verbose: bool) -> Result<i32> {
    crate::session::timing::prelude_lap("cli-startup");
    let interactive_handoff =
        args.interactive_handoff(io::stdin().is_terminal(), io::stdout().is_terminal());
    let mut resolved = config::resolve_run(args.clone(), runtime_flag)?;
    crate::session::timing::prelude_lap("config-resolve");
    enforce_experimental_provider(
        resolved.selected_provider.experimental,
        resolved.allow_experimental,
        io::stdin().is_terminal(),
    )?;
    note(egress_intent_line(&resolved.config.network).to_string());
    crate::session::timing::prelude_lap("egress-intent");
    if resolved.selected_credential.is_none() {
        // A session brokers exactly one provider, and its keys.env carries only
        // that provider's credential — so with none, the broker refuses to start
        // and never publishes its CA cert. Fail fast with the fix instead of
        // spawning a gateway that will dead-end on a cryptic CA-timeout error.
        let id = resolved.selected_provider.id.clone();
        let key = resolved.selected_provider.key_env.clone();
        let provider_is_anthropic = id.eq_ignore_ascii_case("anthropic");
        // A keystore that exists but is unreadable (denied/corrupt/too new)
        // must fail the launch with its own diagnosis, not pretend the key
        // was never configured — the recovery commands differ.
        let keystore_problem = if provider_is_anthropic {
            None
        } else {
            config::provider_keystore_problem()
        };
        if let Some(problem) = keystore_problem.as_deref() {
            bail!(
                "provider '{id}' has no usable credential: {problem}\n\
                 As a workaround, set {key} in your environment:\n    \
                 export {key}='<your {id} key>'"
            );
        }
        let mut message = format!(
            "provider '{id}' has no credential; the gateway broker cannot start without it.\n\
             Set {key} in your environment, then re-run:\n    \
             export {key}='<your {id} key>'"
        );
        // Env vars remain the default path; point Anthropic users at the
        // opt-in keychain import only when that lookup is disabled.
        if provider_is_anthropic && !config::keychain_lookup_enabled() {
            message.push_str(
                "\nAlternatively, run 'capscode key set anthropic' to store a setup-token \
                 in the keychain and enable keychain lookup.",
            );
        } else if !provider_is_anthropic {
            message.push_str(&format!("\nOr: capscode key set {id}"));
            if !config::keychain_lookup_enabled() {
                message.push_str(
                    "\nNote: [auth] keychain is false, so stored provider keys are \
                     ignored; 'capscode key set' re-enables the lookup.",
                );
            }
        }
        if should_offer_inline_store(
            provider_is_anthropic,
            io::stdin().is_terminal(),
            keystore_problem.is_some(),
        ) {
            note(message.to_string());
            let confirmed = {
                let _cursor = crate::terminal::CursorGuard::new();
                Confirm::with_theme(&crate::ui::theme())
                    .with_prompt(format!("Store a key for '{id}' now and continue?"))
                    .default(false)
                    .interact()
                    .context("reading inline provider key confirmation")?
            };
            if !confirmed {
                bail!("{message}");
            }
            let secret = Zeroizing::new({
                let _cursor = crate::terminal::CursorGuard::new();
                Password::with_theme(&crate::ui::theme())
                    .with_prompt(format!("Enter API key for {id}"))
                    .interact()
                    .context("reading provider API key")?
            });
            store_provider_key(
                &resolved.selected_provider,
                &secret,
                keystore::set_key,
                || config::set_keychain_enabled(true),
            )?;
            resolved = config::resolve_run(args, runtime_flag)?;
            if resolved.selected_credential.is_none() {
                bail!("{message}");
            }
        } else {
            bail!("{message}");
        }
    }
    // Aggregator providers publish their catalog over the API rather than in the
    // registry; fold it in before the tool config is generated. Best-effort and
    // host-side: a failure leaves the static registry data in place.
    crate::discovery::augment_provider_models(
        &mut resolved.selected_provider,
        resolved.selected_credential.as_ref(),
    )
    .await;
    crate::session::timing::prelude_lap("model-discovery");
    let runtime: Arc<dyn RuntimeAdapter> = Arc::from(
        capscode_runtime::detect(resolved.config.runtime.clone())
            .await
            .with_context(|| {
                format!(
                    "selecting {} runtime",
                    runtime_name(&resolved.config.runtime)
                )
            })?,
    );
    set_docker_cli_host(runtime.as_ref()).await?;
    let active_lock = crate::toolchain::resolve(resolved.config.image.toolchain_lock.as_deref())?;
    let active_digest = crate::toolchain::digest(&active_lock.lock);
    let merged_image = ImageRef(crate::toolchain::capsule_all_image(&active_digest));
    if runtime.image_present(&merged_image).await? {
        crate::toolchain::write_snapshot(&active_lock.lock, &active_digest)?;
    }
    crate::session::timing::prelude_lap("runtime-detect");
    let session = SessionId::new();
    if interactive_handoff {
        data(format!("Session {session}"));
    } else {
        note(format!("Session {session}"));
    }
    let network: Arc<dyn NetworkStack> = Arc::new(NetStack::new(resolved.config.network.clone()));
    let git: Arc<dyn GitEngine> = Arc::new(DefaultGitEngine::new());
    let options = SessionOptions {
        session,
        config: resolved.config,
        host_repo: resolved.host_repo,
        tool_command: resolved.tool_command,
        tool_args: resolved.tool_args,
        gateway_image: resolved.gateway_image,
        capsule_image: resolved.capsule_image,
        seeder_image: resolved.seeder_image,
        selected_provider: resolved.selected_provider,
        selected_credential: resolved.selected_credential,
        selected_credential_source: resolved.selected_credential_source,
        extra_providers: resolved.extra_providers,
        interactive_handoff,
        verbose,
        r#box: None,
    };
    let mut driver = SessionDriver::new(options, runtime, git, network)?;
    let shutdown = signal_receiver()?;
    // Tokio now owns SIGINT+SIGTERM for graceful container teardown. Flip the
    // flag so the prompt restorer restores the cursor/termios but does not
    // re-raise (which would hard-kill and leak the gateway).
    crate::terminal::set_session_signal_owner(true);
    let result = driver.drive(shutdown).await;
    debug_assert_eq!(driver.state(), crate::session::state::State::Destroyed);
    result
}

async fn set_docker_cli_host(runtime: &dyn RuntimeAdapter) -> Result<()> {
    let info = runtime
        .preflight()
        .await
        .context("reading the selected runtime endpoint")?;
    let endpoint = docker_cli_endpoint(info.socket_path, std::env::var("DOCKER_HOST").ok())?;
    // The git engine shells out to `docker`; give it the same endpoint the
    // selected adapter is using instead of the user's unrelated CLI context.
    // SAFETY: this runs immediately after runtime selection and before session
    // tasks or children are spawned, so no other application thread reads or
    // writes these variables concurrently.
    unsafe {
        std::env::set_var("CAPSCODE_DOCKER_HOST", &endpoint);
        std::env::set_var("DOCKER_HOST", endpoint);
    }
    Ok(())
}

fn docker_cli_endpoint(socket_path: Option<String>, docker_host: Option<String>) -> Result<String> {
    Ok(match socket_path {
        Some(socket) if socket.contains("://") => socket,
        Some(socket) => {
            #[cfg(windows)]
            {
                format!("npipe://{socket}")
            }
            #[cfg(not(windows))]
            {
                format!("unix://{socket}")
            }
        }
        None => docker_host
            .context("the selected runtime did not report a socket and DOCKER_HOST is unset")?,
    })
}

fn should_offer_inline_store(
    provider_is_anthropic: bool,
    stdin_is_terminal: bool,
    keystore_problem_present: bool,
) -> bool {
    !provider_is_anthropic && stdin_is_terminal && !keystore_problem_present
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExperimentalGate {
    Allow,
    Prompt,
    Refuse,
}

fn experimental_gate(
    provider_is_experimental: bool,
    approved: bool,
    stdin_is_terminal: bool,
) -> ExperimentalGate {
    if !provider_is_experimental || approved {
        ExperimentalGate::Allow
    } else if stdin_is_terminal {
        ExperimentalGate::Prompt
    } else {
        ExperimentalGate::Refuse
    }
}

fn enforce_experimental_provider(
    provider_is_experimental: bool,
    approved: bool,
    stdin_is_terminal: bool,
) -> Result<()> {
    match experimental_gate(provider_is_experimental, approved, stdin_is_terminal) {
        ExperimentalGate::Allow => Ok(()),
        ExperimentalGate::Prompt => {
            let confirmed = {
                let _cursor = crate::terminal::CursorGuard::new();
                Confirm::with_theme(&crate::ui::theme())
                    .with_prompt(
                        "This tool+provider combo is experimental and may not work. Continue?",
                    )
                    .default(false)
                    .interact()
                    .context("reading experimental provider confirmation")?
            };
            if confirmed {
                Ok(())
            } else {
                bail!("experimental provider launch cancelled")
            }
        }
        ExperimentalGate::Refuse => bail!(
            "this tool+provider combo is experimental; non-interactive runs require --yes or --experimental"
        ),
    }
}

pub fn provider_list() -> Result<()> {
    let (built_in_rows, user_rows) = provider_list_rows()?;
    data(format_provider_list(&built_in_rows, &user_rows).to_string());
    Ok(())
}

fn provider_row(provider: config::ProviderView) -> [String; 4] {
    [
        provider.spec.id,
        config::api_style_name(provider.spec.api_style).to_string(),
        if provider.configured { "yes" } else { "no" }.to_string(),
        format!(
            "{}{}",
            provider.spec.base_url,
            if provider.spec.experimental {
                " (experimental)"
            } else {
                ""
            }
        ),
    ]
}

type ProviderTableRow = [String; 4];
type ProviderListRows = (Vec<ProviderTableRow>, Vec<ProviderTableRow>);

fn provider_list_rows() -> Result<ProviderListRows> {
    let bundled_ids = capscode_core::load_bundled_provider_registry()?
        .into_iter()
        .map(|provider| provider.id)
        .collect::<HashSet<_>>();
    let mut built_in_rows = Vec::new();
    let mut user_rows = Vec::new();
    for provider in config::provider_views()? {
        let row = provider_row(provider);
        if bundled_ids.contains(&row[0]) {
            built_in_rows.push(row);
        } else {
            user_rows.push(row);
        }
    }
    Ok((built_in_rows, user_rows))
}

fn format_provider_list(
    built_in_rows: &[ProviderTableRow],
    user_rows: &[ProviderTableRow],
) -> String {
    let mut output = format!(
        "BUILT-IN PROVIDERS\n{}\n\nUSER-DEFINED PROVIDERS\n",
        format_provider_table(built_in_rows)
    );
    if user_rows.is_empty() {
        output.push_str("(none)");
    } else {
        output.push_str(&format_provider_table(user_rows));
    }
    output
}

fn format_provider_table(rows: &[ProviderTableRow]) -> String {
    table(
        &["ID", "API STYLE", "CONFIGURED", "ENDPOINT"],
        &rows.iter().map(|row| row.to_vec()).collect::<Vec<_>>(),
        Caps::PLAIN,
    )
}

pub fn provider_show(id: &str) -> Result<()> {
    let provider = config::provider_views()?
        .into_iter()
        .find(|provider| provider.spec.id.eq_ignore_ascii_case(id))
        .with_context(|| format!("unknown provider '{id}'"))?;
    data(format!("Provider: {}", provider.spec.id));
    data(format!(
        "API style: {}",
        config::api_style_name(provider.spec.api_style)
    ));
    data(format!("Base URL: {}", provider.spec.base_url));
    data(format!(
        "Auth injection: {}",
        config::auth_scheme_name(provider.spec.auth)
    ));
    data(format!("Credential env: {}", provider.spec.key_env));
    data(format!(
        "Default model: {}",
        provider.spec.default_model.as_deref().unwrap_or("not set")
    ));
    data(format!(
        "Configured: {}{}",
        if provider.configured { "yes" } else { "no" },
        provider
            .credential_source
            .as_deref()
            .map(|source| format!(" ({source})"))
            .unwrap_or_default()
    ));
    data(format!("Experimental: {}", provider.spec.experimental));
    Ok(())
}

pub fn provider_add(args: ProviderAddArgs) -> Result<()> {
    let spec = provider_spec_from_args(args)?;
    config::add_user_provider(&spec)?;
    data(format!("Provider '{}' added.", spec.id));
    let store_now = if io::stdin().is_terminal() {
        let _cursor = crate::terminal::CursorGuard::new();
        Confirm::with_theme(&crate::ui::theme())
            .with_prompt(format!("Store the credential for '{}' now?", spec.id))
            .default(true)
            .interact()
            .context("reading provider key confirmation")?
    } else {
        false
    };
    if store_now {
        provider_key_set(&spec.id)?;
    } else {
        data(format!(
            "Set its credential later with `capscode key set {}` or in {}.",
            spec.id, spec.key_env
        ));
    }
    Ok(())
}

pub fn provider_remove(id: &str) -> Result<()> {
    config::remove_user_provider(id)?;
    data(format!("Provider '{id}' removed from the user registry."));
    Ok(())
}

fn store_provider_key(
    spec: &ProviderSpec,
    secret: &str,
    write: impl FnOnce(&str, &str) -> Result<()>,
    enable_keychain: impl FnOnce() -> Result<()>,
) -> Result<()> {
    config::validate_credential_line(secret, &spec.key_env)?;
    write(&spec.id, secret)?;
    enable_keychain()?;
    config::clear_keystore_cache();
    config::clear_keychain_cache();
    Ok(())
}

fn ensure_provider_key_supported(spec: &ProviderSpec) -> Result<()> {
    if spec.id.eq_ignore_ascii_case("anthropic") {
        bail!(
            "Anthropic credentials use 'capscode key set anthropic'; provider key bundles store non-Anthropic API keys only"
        );
    }
    Ok(())
}

pub(crate) fn store_provider_key_in_keychain(spec: &ProviderSpec, secret: &str) -> Result<()> {
    ensure_provider_key_supported(spec)?;
    store_provider_key(spec, secret, keystore::set_key, || {
        config::set_keychain_enabled(true)
    })
}

pub fn provider_key_set(id: &str) -> Result<()> {
    let registry = config::providers::load_registry()?;
    let spec = registry
        .iter()
        .find(|provider| provider.id.eq_ignore_ascii_case(id))
        .with_context(|| {
            let valid = registry
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("unknown provider '{id}'; valid provider ids: {valid}")
        })?;
    let secret = if io::stdin().is_terminal() {
        Zeroizing::new({
            let _cursor = crate::terminal::CursorGuard::new();
            Password::with_theme(&crate::ui::theme())
                .with_prompt(format!("Enter API key for {}", spec.id))
                .interact()
                .context("reading provider API key")?
        })
    } else {
        let mut value = String::new();
        io::stdin()
            .lock()
            .read_line(&mut value)
            .context("reading provider API key from stdin")?;
        Zeroizing::new(value.trim_end_matches(['\r', '\n']).to_string())
    };
    store_provider_key_in_keychain(spec, &secret)?;
    data(format!(
        "Provider key for '{}' stored in keychain service '{}' (account '{}'). Environment variable {} still takes precedence.",
        spec.id,
        keystore::SERVICE,
        keystore::ACCOUNT,
        spec.key_env
    ));
    Ok(())
}

pub fn provider_key_remove(id: &str) -> Result<()> {
    // `set` stores under the registry's canonical id, so `rm` must resolve the
    // same way or a case-variant id silently removes nothing. Ids not in the
    // registry are passed through verbatim (preserved unknown entries).
    let registry = config::providers::load_registry()?;
    let canonical = registry
        .iter()
        .find(|provider| provider.id.eq_ignore_ascii_case(id))
        .map(|provider| provider.id.clone())
        .unwrap_or_else(|| id.to_string());
    let removed = keystore::remove_key(&canonical)?;
    config::clear_keystore_cache();
    if removed {
        data(format!(
            "Provider key for '{canonical}' removed from the keychain bundle."
        ));
    } else {
        data(format!(
            "No provider key stored for '{canonical}'; nothing removed."
        ));
    }
    Ok(())
}

pub fn provider_key_list() -> Result<()> {
    // DECISION: keychain reads are opt-in; listing must not trigger an OS authorization dialog
    // either, so with lookup off `key ls` explains instead of probing.
    if !config::keychain_lookup_enabled() {
        data(
            "Keychain lookup is off ([auth] keychain = false), so CapsCode does not read the OS \
             keychain and sessions use environment variables only.",
        );
        data(
            "Store a key with `capscode key set <provider>`, or read existing items with \
             `capscode config set auth.keychain true`.",
        );
        return Ok(());
    }
    let registry = config::providers::load_registry()?;
    let registry_ids: HashSet<String> = registry.into_iter().map(|provider| provider.id).collect();
    let keys = keystore::list(&registry_ids)?;
    let anthropic = config::anthropic_auth_status().status_line().to_string();
    let (service, account) = config::keychain_target()?;
    let entry = keyring::Entry::new(&service, &account).context("opening OS keychain entry")?;
    let anthropic_stored = match entry.get_password() {
        Ok(value) if !value.is_empty() => "stored".to_owned(),
        Ok(_) | Err(keyring::Error::NoEntry) => "not stored".to_owned(),
        Err(error) => format!("unreadable ({error})"),
    };
    let mut rows = Vec::new();
    for key in keys {
        rows.push(vec![
            key.id,
            key.kind,
            keystore::stored_at_iso(key.stored_at),
            if key.unknown { "unknown id" } else { "known" }.to_string(),
        ]);
    }
    if rows.is_empty() {
        data("(no provider API keys stored)");
    } else {
        data(table(
            &["ID", "KIND", "STORED AT", "STATUS"],
            &rows,
            Caps::PLAIN,
        ));
    }
    data(format!("Anthropic effective credential: {anthropic}"));
    data(format!(
        "Anthropic setup token: {anthropic_stored} in OS keychain service '{service}'"
    ));
    Ok(())
}

pub fn provider_key_reset() -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!("`capscode key reset` requires an interactive confirmation");
    }
    let confirmed = {
        let _cursor = crate::terminal::CursorGuard::new();
        Confirm::with_theme(&crate::ui::theme())
            .with_prompt(
                "Delete the provider key bundle and Anthropic setup token? This cannot be undone.",
            )
            .default(false)
            .interact()
            .context("reading provider key reset confirmation")?
    };
    if !confirmed {
        data("Provider keychain reset cancelled.");
        return Ok(());
    }
    keystore::reset()?;
    config::clear_keystore_cache();
    auth_remove()?;
    data("Provider keychain bundle deleted.");
    Ok(())
}

pub fn alias_help(tool: ToolArg, provider: Option<&str>) -> Result<()> {
    if let Some(provider) = provider {
        let config = config::load(None)?;
        let resolved =
            config::resolve_provider_spec(Some(provider), tool.into(), &config.defaults)?;
        data(format!(
            "Provider: {} ({}{})",
            resolved.id,
            config::api_style_name(resolved.api_style),
            if resolved.experimental {
                ", experimental"
            } else {
                ""
            }
        ));
    }
    data(format!(
        "Usage: capscode [--runtime <runtime>] {} [--provider <id>] [--providers <ids>] [--mount|--copy] [--pick] [--history <mode>] [--include <path>] [--yes] [--experimental] [-v] [ARGS]\n\nRuns {} in a disposable capsule.\nPass tool arguments after '--'.",
        tool.name(),
        tool.name()
    ));
    Ok(())
}

fn provider_spec_from_args(args: ProviderAddArgs) -> Result<ProviderSpec> {
    let interactive = io::stdin().is_terminal();
    if interactive
        && args.id.is_none()
        && args.base_url.is_none()
        && args.api_style.is_none()
        && args.key_env.is_none()
        && args.default_model.is_none()
        && args.auth.is_none()
    {
        return config::prompt_provider_spec(args.experimental);
    }
    let id = required_or_prompt(args.id, "Provider ID", "--id", interactive)?;
    let api_style = match args.api_style {
        Some(ApiStyleArg::Anthropic) => ApiStyle::Anthropic,
        Some(ApiStyleArg::Openai) => ApiStyle::OpenAI,
        None if interactive => {
            let choice = {
                let _cursor = crate::terminal::CursorGuard::new();
                Select::with_theme(&crate::ui::theme())
                    .with_prompt("API style")
                    .items(["anthropic", "openai"])
                    .default(0)
                    .interact()
                    .context("reading provider API style")?
            };
            if choice == 0 {
                ApiStyle::Anthropic
            } else {
                ApiStyle::OpenAI
            }
        }
        None => bail!("--api-style is required when stdin is not a terminal"),
    };
    let base_url = required_or_prompt(args.base_url, "Base URL", "--base-url", interactive)?;
    config::validate_provider_endpoint_suffix(api_style, &base_url)?;
    let key_env = required_or_prompt(
        args.key_env,
        "Credential environment variable",
        "--key-env",
        interactive,
    )?;
    let default_model = args.default_model;
    let auth = match args.auth {
        Some(AuthSchemeArg::Bearer) => AuthScheme::Bearer,
        Some(AuthSchemeArg::XApiKey) => AuthScheme::XApiKey,
        None => match api_style {
            ApiStyle::Anthropic => AuthScheme::XApiKey,
            ApiStyle::OpenAI => AuthScheme::Bearer,
        },
    };
    Ok(ProviderSpec {
        id,
        label: None,
        api_style,
        base_url,
        auth,
        key_env,
        default_model,
        models: Vec::new(),
        experimental: args.experimental,
    })
}

fn required_or_prompt(
    value: Option<String>,
    prompt: &str,
    flag: &str,
    interactive: bool,
) -> Result<String> {
    match value {
        Some(value) => Ok(value),
        None if interactive => {
            let _cursor = crate::terminal::CursorGuard::new();
            Input::with_theme(&crate::ui::theme())
                .with_prompt(prompt)
                .interact_text()
                .with_context(|| format!("reading {prompt}"))
        }
        None => bail!("{flag} is required when stdin is not a terminal"),
    }
}

pub fn vpn_import(path: &Path) -> Result<()> {
    let config_path =
        capscode_core::Config::prepare_config_file().context("preparing config home")?;
    let validation = vpn_import_at(path, &config_path)?;
    data(format!("VPN configured: endpoint {}", validation.endpoint));
    Ok(())
}

fn vpn_import_at(path: &Path, config_path: &Path) -> Result<VpnValidation> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolving WireGuard config path {}", path.display()))?;
    let validation = validate_wireguard_file(&path)?;
    config::set_wireguard_conf_at(config_path, Some(&path))?;
    Ok(validation)
}

pub fn vpn_status(runtime_flag: Option<RuntimeArg>) -> Result<()> {
    let config = config::load(runtime_flag)?;
    let Some(path) = config.network.wireguard_conf.as_deref() else {
        data("VPN: not configured");
        return Ok(());
    };
    data(format!("VPN config: {}", path.display()));
    match validate_wireguard_file(path) {
        Ok(validation) => data(format!("VPN: valid (endpoint {})", validation.endpoint)),
        Err(error) => data(format!("VPN: invalid ({error:#})")),
    }
    Ok(())
}

pub fn vpn_remove() -> Result<()> {
    config::set_wireguard_conf(None)?;
    data("VPN configuration removed; WireGuard file left untouched.");
    Ok(())
}

pub fn auth_import() -> Result<()> {
    let token = if io::stdin().is_terminal() {
        Zeroizing::new({
            let _cursor = crate::terminal::CursorGuard::new();
            Password::with_theme(&crate::ui::theme())
                .with_prompt("Claude setup-token")
                .interact()
                .context("reading Claude setup-token")?
        })
    } else {
        let mut value = String::new();
        io::stdin()
            .lock()
            .read_to_string(&mut value)
            .context("reading Claude setup-token from stdin")?;
        Zeroizing::new(value.trim_end_matches(['\r', '\n']).to_string())
    };
    if token.is_empty() {
        bail!("Claude setup-token must not be empty");
    }

    let service = store_anthropic_token(&token)?;
    data(format!(
        "Anthropic OAuth token stored in keychain:{service}"
    ));
    Ok(())
}

pub(crate) fn store_anthropic_token(token: &str) -> Result<String> {
    if token.is_empty() {
        bail!("Claude setup-token must not be empty");
    }
    let (service, account) = config::keychain_target()?;
    keyring::Entry::new(&service, &account)
        .context("opening OS keychain entry")?
        .set_password(token)
        .context("storing Claude setup-token in OS keychain")?;
    // Opt in to keychain reads so the token we just stored is actually used.
    config::set_keychain_enabled(true)?;
    config::clear_keychain_cache();
    Ok(service)
}

pub fn auth_remove() -> Result<()> {
    let (service, account) = config::keychain_target()?;
    let entry = keyring::Entry::new(&service, &account).context("opening OS keychain entry")?;
    match entry.delete_credential() {
        Ok(()) => {
            config::clear_keychain_cache();
            data(format!(
                "Anthropic OAuth token removed from keychain:{service}"
            ));
        }
        Err(keyring::Error::NoEntry) => data(format!("No credential found in keychain:{service}")),
        Err(error) => return Err(error).context("removing Claude setup-token from OS keychain"),
    }
    // Disarm the probe: leaving it armed after remove reintroduces the dialog
    // for nothing.
    config::set_keychain_enabled(false)?;
    Ok(())
}

#[derive(Clone, Debug)]
struct DashboardCredential {
    source: String,
    kind: Option<String>,
}

#[derive(Clone, Debug)]
struct DashboardRuntime {
    engine: Option<String>,
    error: Option<String>,
}

pub async fn status(runtime_flag: Option<RuntimeArg>, as_json: bool) -> Result<i32> {
    let config = config::load(runtime_flag)?;
    let anthropic_status = config::anthropic_auth_status();
    let anthropic = DashboardCredential {
        source: anthropic_status.source().to_string(),
        kind: anthropic_status
            .kind()
            .map(config::credential_kind_name)
            .map(str::to_string),
    };
    let codex_status = config::codex_auth_status()?;
    let codex = DashboardCredential {
        source: codex_status.source().to_string(),
        kind: codex_status
            .kind()
            .map(config::credential_kind_name)
            .map(str::to_string),
    };
    let provider_keys = config::provider_keystore_status();
    let vpn = vpn_status_for_without_network(config.network.wireguard_conf.as_deref());
    let posture = egress_posture(&config.network);
    let runtime = match capscode_runtime::detect(config.runtime).await {
        Ok(runtime) => DashboardRuntime {
            engine: Some(format!("{:?}", runtime.kind()).to_ascii_lowercase()),
            error: None,
        },
        Err(error) => DashboardRuntime {
            engine: None,
            error: Some(error.to_string()),
        },
    };

    if as_json {
        data(
            (serde_json::to_string_pretty(&status_json_payload(
                &runtime,
                &anthropic,
                &codex,
                &config.defaults,
                &provider_keys,
                &vpn,
                posture,
            ))?)
            .to_string(),
        );
    } else {
        print_status_dashboard(
            StatusDashboardView {
                runtime: &runtime,
                anthropic: &anthropic,
                codex: &codex,
                defaults: &config.defaults,
                provider_keys: &provider_keys,
                vpn: &vpn,
                posture,
            },
            Caps::stderr(),
        );
    }
    let provider_configured = matches!(
        &provider_keys,
        config::ProviderKeystoreStatus::Loaded(ids) if !ids.is_empty()
    );
    let checks = quick_doctor_checks(
        &runtime,
        provider_configured,
        anthropic.kind.is_some() || codex.kind.is_some(),
        matches!(&vpn, VpnStatus::Ready { .. }),
    );
    doctor_status_result(&checks)
}

// DECISION: quick doctor requires runtime selection; provider, credential, and VPN probes remain optional.
fn quick_doctor_checks(
    runtime: &DashboardRuntime,
    provider_configured: bool,
    credential_configured: bool,
    vpn_ready: bool,
) -> Vec<DoctorCheckResult> {
    let runtime_check = match (&runtime.engine, &runtime.error) {
        (Some(_), _) => Ok(()),
        (None, Some(error)) => Err(error.clone()),
        (None, None) => Err("runtime unavailable".to_string()),
    };
    vec![
        DoctorCheckResult {
            kind: DoctorCheckKind::Runtime,
            result: runtime_check,
        },
        DoctorCheckResult {
            kind: DoctorCheckKind::Provider,
            result: optional_check_result(provider_configured, "no provider key is configured"),
        },
        DoctorCheckResult {
            kind: DoctorCheckKind::Credential,
            result: optional_check_result(
                credential_configured,
                "no provider credential is configured",
            ),
        },
        DoctorCheckResult {
            kind: DoctorCheckKind::Vpn,
            result: optional_check_result(vpn_ready, "VPN is not configured"),
        },
    ]
}

/// Checks for a full `doctor` run once the runtime is reachable.
fn full_doctor_checks(
    gateway_error: Option<&str>,
    containment_ok: Option<bool>,
    provider_statuses: &[ToolProviderStatus],
    anthropic_configured: bool,
    vpn_ready: bool,
) -> Vec<DoctorCheckResult> {
    let mut checks = vec![
        DoctorCheckResult {
            kind: DoctorCheckKind::Runtime,
            result: Ok(()),
        },
        DoctorCheckResult {
            kind: DoctorCheckKind::Gateway,
            result: match gateway_error {
                Some(error) => Err(error.to_owned()),
                None => Ok(()),
            },
        },
        // DECISION: a missing containment report fails the required check instead of counting as clean.
        DoctorCheckResult {
            kind: DoctorCheckKind::Containment,
            result: optional_check_result(
                containment_ok.unwrap_or(false),
                "gateway containment checks failed",
            ),
        },
    ];
    checks.extend(provider_statuses.iter().map(|status| {
        DoctorCheckResult {
            kind: DoctorCheckKind::Provider,
            result: optional_check_result(
                status.configured,
                status
                    .error
                    .as_deref()
                    .unwrap_or("provider credential is not configured"),
            ),
        }
    }));
    checks.push(DoctorCheckResult {
        kind: DoctorCheckKind::Credential,
        result: optional_check_result(
            anthropic_configured,
            "Anthropic credential is not configured",
        ),
    });
    checks.push(DoctorCheckResult {
        kind: DoctorCheckKind::Vpn,
        result: optional_check_result(vpn_ready, "VPN is not configured or is invalid"),
    });
    checks
}

fn runtime_failure_doctor_checks(error: &str) -> Vec<DoctorCheckResult> {
    vec![DoctorCheckResult {
        kind: DoctorCheckKind::Runtime,
        result: Err(error.to_owned()),
    }]
}

// DECISION: provider, credential, and VPN probes remain visible diagnostics but do not gate doctor success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DoctorCheckKind {
    Runtime,
    Gateway,
    Containment,
    Provider,
    Credential,
    Vpn,
}

impl DoctorCheckKind {
    fn required(self) -> bool {
        matches!(self, Self::Runtime | Self::Gateway | Self::Containment)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Runtime => "runtime unavailable",
            Self::Gateway => "gateway check failed",
            Self::Containment => "gateway containment checks failed",
            Self::Provider => "provider key is missing",
            Self::Credential => "provider credential is missing",
            Self::Vpn => "VPN is not configured",
        }
    }
}

struct DoctorCheckResult {
    kind: DoctorCheckKind,
    result: DoctorCheckStatus,
}

type DoctorCheckStatus = std::result::Result<(), String>;

fn optional_check_result(configured: bool, message: &str) -> DoctorCheckStatus {
    if configured {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

fn doctor_exit_code(checks: &[DoctorCheckResult]) -> i32 {
    if checks
        .iter()
        .filter(|check| check.kind.required())
        .all(|check| check.result.is_ok())
    {
        0
    } else {
        1
    }
}

fn print_doctor_failure_summary(checks: &[DoctorCheckResult]) {
    let failures: Vec<_> = checks
        .iter()
        .filter(|check| check.kind.required() && check.result.is_err())
        .map(|check| check.kind.label())
        .collect();
    if !failures.is_empty() {
        note(format!(
            "doctor failed required checks: {}",
            failures.join("; ")
        ));
    }
}

fn doctor_status_result(checks: &[DoctorCheckResult]) -> Result<i32> {
    print_doctor_failure_summary(checks);
    Ok(doctor_exit_code(checks))
}

#[derive(Debug)]
struct VpnValidation {
    endpoint: String,
}

#[derive(Debug)]
enum VpnStatus {
    NotConfigured,
    Ready { path: PathBuf, endpoint: String },
    Misconfigured { path: PathBuf, reason: String },
}

fn vpn_status_for(path: Option<&Path>) -> VpnStatus {
    let Some(path) = path else {
        return VpnStatus::NotConfigured;
    };
    match validate_wireguard_file(path) {
        Ok(validation) => VpnStatus::Ready {
            path: path.to_path_buf(),
            endpoint: validation.endpoint,
        },
        Err(error) => VpnStatus::Misconfigured {
            path: path.to_path_buf(),
            reason: format!("{error:#}"),
        },
    }
}

fn vpn_status_for_without_network(path: Option<&Path>) -> VpnStatus {
    let Some(path) = path else {
        return VpnStatus::NotConfigured;
    };
    match validate_wireguard_file_without_network(path) {
        Ok(validation) => VpnStatus::Ready {
            path: path.to_path_buf(),
            endpoint: validation.endpoint,
        },
        Err(error) => VpnStatus::Misconfigured {
            path: path.to_path_buf(),
            reason: format!("{error:#}"),
        },
    }
}

fn validate_wireguard_file(path: &Path) -> Result<VpnValidation> {
    validate_wireguard_file_at(path, true)
}

fn validate_wireguard_file_without_network(path: &Path) -> Result<VpnValidation> {
    validate_wireguard_file_at(path, false)
}

fn validate_wireguard_file_at(path: &Path, resolve_endpoint: bool) -> Result<VpnValidation> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading WireGuard config {}", path.display()))?;
    let parsed = capscode_net::parse_wireguard_conf(&text)?;
    if resolve_endpoint {
        for peer in &parsed.peers {
            let mut addresses = (peer.endpoint_host.as_str(), peer.endpoint_port)
                .to_socket_addrs()
                .with_context(|| {
                    format!(
                        "WireGuard endpoint host '{}' does not resolve",
                        peer.endpoint_host
                    )
                })?;
            if addresses.next().is_none() {
                bail!(
                    "WireGuard endpoint host '{}' does not resolve",
                    peer.endpoint_host
                );
            }
        }
    }
    let first = parsed.peers.first().context("config defines no peer")?;
    Ok(VpnValidation {
        endpoint: format_endpoint(&first.endpoint_host, first.endpoint_port),
    })
}

fn format_endpoint(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn egress_intent_line(network: &capscode_core::NetworkConfig) -> String {
    match (network.wireguard_conf.as_deref(), network.enforce_vpn) {
        (Some(path), true) => {
            let endpoint = match vpn_status_for(Some(path)) {
                VpnStatus::Ready { endpoint, .. } => endpoint,
                _ => "unknown endpoint".to_string(),
            };
            format!("Egress: WireGuard VPN — exit {endpoint}")
        }
        (Some(_), false) => {
            "Egress: DIRECT — VPN imported but not enforced (host IP)".to_string()
        }
        (None, _) => {
            "Egress: DIRECT — via your host IP (containment-only). Add a VPN: capscode vpn import <provider>.conf".to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EgressPosture {
    mode: &'static str,
    detail: &'static str,
}

impl EgressPosture {
    fn line(self) -> String {
        format!("Egress posture: {} — {}", self.mode, self.detail)
    }
}

fn egress_posture(network: &capscode_core::NetworkConfig) -> EgressPosture {
    if network.wireguard_conf.is_some() && network.enforce_vpn {
        EgressPosture {
            mode: "tunnel",
            detail: "all egress forced through WireGuard; DNS pinned to the tunnel resolver; host/LAN/metadata blocked. No destination filtering beyond this (by design).",
        }
    } else {
        EgressPosture {
            mode: "containment-only",
            detail: "public egress permitted by design (no data-exfiltration filtering); host/LAN/metadata blocked. Import a VPN for origin privacy: capscode vpn import <provider>.conf",
        }
    }
}

pub async fn ls(runtime_flag: Option<RuntimeArg>, as_json: bool) -> Result<()> {
    let runtime = selected_runtime(runtime_flag).await?;
    let objects = runtime.list_owned().await?;
    let boxes = crate::boxes::ls_rows(Arc::clone(&runtime)).await?;
    let mut box_sessions = HashSet::new();
    for name in crate::boxes::index::names() {
        match crate::boxes::resolve(&name)? {
            crate::boxes::Resolved::One(target) => {
                box_sessions.insert(SessionId::from(target.session));
            }
            crate::boxes::Resolved::Ambiguous(matches) => bail!(
                "box index entry '{name}' is ambiguous: {}",
                matches
                    .iter()
                    .map(|item| item.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            crate::boxes::Resolved::None => {}
        }
    }
    box_sessions.extend(boxes.iter().filter_map(|row| {
        row.session
            .as_ref()
            .map(|session| SessionId::from(session.clone()))
    }));
    let mut sessions: Vec<(OwnedObject, Option<PathBuf>)> = Vec::new();
    for object in objects
        .into_iter()
        .filter(|object| session_object_is_listable(object, &box_sessions))
    {
        let session = object.session.as_ref().expect("filtered above");
        let project = crate::session::state::read_session_host_repo(session).ok();
        if let Some(existing) = sessions
            .iter_mut()
            .find(|(known, _)| known.session.as_ref() == Some(session))
        {
            if object.kind == OwnedObjectKind::Container {
                *existing = (object, project);
            }
        } else {
            sessions.push((object, project));
        }
    }
    if as_json {
        let mut values: Vec<_> = sessions
            .iter()
            .map(|(object, project)| {
                json!({
                    "kind": "session",
                    "session": object.session.as_ref().map(ToString::to_string),
                    "state": object.state.as_deref().unwrap_or("workspace preserved"),
                    "project": project,
                    "created_at": object.created_at,
                })
            })
            .collect();
        values.extend(boxes.iter().map(box_row_json));
        data((serde_json::to_string_pretty(&values)?).to_string());
    } else if sessions.is_empty() && boxes.is_empty() {
        data("No CapsCode-owned runtime objects.");
    } else {
        let rows = sessions
            .into_iter()
            .map(|(object, project)| {
                vec![
                    "session".to_string(),
                    object
                        .session
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "-".into()),
                    object
                        .state
                        .unwrap_or_else(|| "workspace preserved".to_string()),
                    project
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    object
                        .created_at
                        .as_deref()
                        .map(crate::session::state::format_created_at)
                        .unwrap_or_else(|| "-".to_string()),
                ]
            })
            .collect::<Vec<_>>();
        let mut rows = rows;
        let yanked_boxes: Vec<String> = boxes
            .iter()
            .filter(|row| row.toolchain_yanked)
            .map(|row| row.name.clone())
            .collect();
        rows.extend(boxes.into_iter().map(|row| {
            let toolchain_label = toolchain_row_label(&row);
            vec![
                if row.state == "legacy" {
                    "legacy"
                } else {
                    "box"
                }
                .to_string(),
                row.name,
                row.state,
                row.project.display().to_string(),
                row.mode,
                row.providers.join(","),
                toolchain_label,
                row.last_entered,
            ]
        }));
        data(table(
            &[
                "KIND",
                "NAME / SESSION",
                "STATE",
                "PROJECT",
                "MODE",
                "PROVIDERS",
                "TOOLCHAIN",
                "LAST ENTERED / CREATED",
            ],
            &rows,
            Caps::PLAIN,
        ));
        note("Use `capscode <box>` to enter a box.");
        for name in yanked_boxes {
            note(format!(
                "Box {name} uses a withdrawn toolchain; switch with `capscode resume {name} --toolchain current`."
            ));
        }
    }
    Ok(())
}

fn box_row_json(row: &crate::boxes::BoxRow) -> serde_json::Value {
    json!({
        "kind": if row.state == "legacy" { "legacy" } else { "box" },
        "name": row.name, "session": row.session, "state": row.state,
        "project": row.project, "mode": row.mode,
        "providers": row.providers, "last_entered": row.last_entered,
        "toolchain": row.toolchain,
        "toolchain_current": row.toolchain_current,
        "toolchain_pinned": row.toolchain_pinned,
        "toolchain_tested": row.toolchain_tested,
        "toolchain_yanked": row.toolchain_yanked,
        "cache_volume": row.cache_volume,
    })
}

fn toolchain_row_label(row: &crate::boxes::BoxRow) -> String {
    if row.toolchain.is_empty() {
        return "-".to_owned();
    }
    let digest = row.toolchain.get(..7).unwrap_or(&row.toolchain);
    if row.toolchain_yanked {
        format!("{digest} (yanked)")
    } else if row.toolchain_pinned {
        format!("{digest} (pinned)")
    } else if !row.toolchain_tested {
        format!("{digest} (untested)")
    } else if !row.toolchain_current {
        format!("{digest} (older)")
    } else {
        digest.to_owned()
    }
}

fn session_object_is_listable(object: &OwnedObject, box_sessions: &HashSet<SessionId>) -> bool {
    object.session.as_ref().is_some_and(|session| {
        !box_sessions.contains(session)
            && object.labels.get(LABEL_ROLE).map(String::as_str) != Some(ROLE_CACHE)
            && (object.kind == OwnedObjectKind::Volume
                || (object.kind == OwnedObjectKind::Container
                    && object.role.as_deref() == Some(ROLE_CAPSULE)))
    })
}

/// Remove a session's runtime objects, copying its work back first where that is
/// possible.
///
/// `discard_workspace` is the escape hatch for the case copy-back cannot cover.
/// A session whose workspace is not a git repository gets a *file sync* receipt, and
/// sync/rm captures a fresh workspace snapshot before removing it. If that
/// capture or handoff cannot complete, rm preserves the volume rather than
/// destroying uncaptured work. `--discard-workspace` says "I accept the loss";
/// it is deliberately separate from `--force`, which only controls whether
/// containers are stopped before removal.
pub async fn rm_with_confirmation(
    runtime: Arc<dyn RuntimeAdapter>,
    session: Option<String>,
    all: bool,
    force: bool,
    discard_workspace: bool,
    yes: bool,
) -> Result<()> {
    rm_with_box_identity(runtime, session, all, force, discard_workspace, yes, None).await
}

pub(crate) async fn rm_box_with_confirmation(
    runtime: Arc<dyn RuntimeAdapter>,
    target: &crate::boxes::BoxRef,
    force: bool,
    discard_workspace: bool,
) -> Result<()> {
    rm_with_box_identity(
        runtime,
        Some(target.session.clone()),
        false,
        force,
        discard_workspace,
        false,
        Some(target),
    )
    .await
}

async fn rm_with_box_identity(
    runtime: Arc<dyn RuntimeAdapter>,
    session: Option<String>,
    all: bool,
    force: bool,
    discard_workspace: bool,
    yes: bool,
    expected_box: Option<&crate::boxes::BoxRef>,
) -> Result<()> {
    if !all && session.is_none() {
        bail!("provide a session id or use --all");
    }
    if let Some(session) = session.as_deref() {
        validate_session_id(session)?;
    }
    let objects = runtime.list_owned().await?;
    let selected = select_rm_objects(&objects, session.as_deref(), all, expected_box);
    if selected.is_empty() {
        if !all && discard_workspace {
            let session_id = SessionId::from(session.expect("validated above"));
            note(format!(
                "Discarding session {session_id}; its capsule and workspace volume are already gone."
            ));
            report_session_branch_for_removal(&session_id)?;
            remove_session_state_after_confirmation(&session_id)?;
            return Ok(());
        }
        data("No matching CapsCode-owned runtime objects.");
        return Ok(());
    }

    let session_ids = selected
        .iter()
        .filter_map(|object| object.session.clone())
        .fold(Vec::<SessionId>::new(), |mut sessions, session| {
            if !sessions.iter().any(|existing| existing == &session) {
                sessions.push(session);
            }
            sessions
        });
    for session in &session_ids {
        validate_session_id(session.as_str())?;
    }
    if all {
        note("Warning: `rm --all` does no copy-back; unsynced workspace changes may be lost.");
        if !yes {
            if !io::stdin().is_terminal() {
                bail!("`capscode rm --all` requires --yes when stdin is not interactive");
            }
            let confirmed = Confirm::with_theme(&crate::ui::theme())
                .with_prompt("Remove every CapsCode session without copying work back?")
                .default(false)
                .interact()
                .context("reading rm --all confirmation")?;
            if !confirmed {
                note("Removal cancelled.");
                return Ok(());
            }
        }
    }

    let mut clean_state_after_teardown = if all { session_ids.clone() } else { Vec::new() };
    if !all {
        let requested = session.as_deref().expect("validated above");
        let session_id = selected
            .iter()
            .find_map(|object| object.session.clone())
            .unwrap_or_else(|| SessionId::from(requested.to_string()));
        let capture_runtime = runtime.as_ref();
        let capture_session = session_id.clone();
        let workspace_mode = crate::session::state::read_session_workspace_mode(&session_id)?
            .unwrap_or(capscode_core::WorkspaceMode::Copy);
        let clean_state = rm_session_core(
            runtime.as_ref(),
            &selected,
            &session_id,
            force,
            discard_workspace,
            move |stopped| async move {
                capture_rm_session(
                    capture_runtime,
                    &stopped,
                    &capture_session,
                    workspace_mode,
                    discard_workspace,
                )
                .await
            },
            || report_session_branch_for_removal(&session_id),
        )
        .await?;
        if clean_state {
            clean_state_after_teardown.push(session_id);
        }
    } else {
        for session_id in &session_ids {
            report_session_branch_for_removal(session_id)?;
        }

        let failures = teardown_owned_objects(runtime.as_ref(), &selected, true).await;
        if failures > 0 {
            bail!("{failures} object(s) could not be removed (see warnings above)");
        }
    }
    for session_id in clean_state_after_teardown {
        remove_session_state_after_confirmation(&session_id)?;
    }
    Ok(())
}

fn select_rm_objects(
    objects: &[OwnedObject],
    session: Option<&str>,
    all: bool,
    expected_box: Option<&crate::boxes::BoxRef>,
) -> Vec<OwnedObject> {
    objects
        .iter()
        .filter(|object| {
            if !all
                && !object
                    .session
                    .as_ref()
                    .is_some_and(|id| Some(id.as_str()) == session)
            {
                return false;
            }
            if object.kind == OwnedObjectKind::Volume
                && (object.role.as_deref() == Some(ROLE_CACHE)
                    || object.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CACHE))
            {
                return all
                    || expected_box.is_some_and(|target| {
                        crate::boxes::cache_volume_matches_box(object, target)
                    });
            }
            true
        })
        .cloned()
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RmSessionWork {
    Captured,
    Discarded,
    Uncaptured,
}

// DECISION: keep stop, capture, and teardown in one injectable core so rm cannot bypass the
// ordering that protects a running workspace from being copied or removed mid-write.
async fn rm_session_core<Capture, CaptureFuture, BeforeTeardown>(
    runtime: &dyn RuntimeAdapter,
    objects: &[OwnedObject],
    session: &SessionId,
    force_remove: bool,
    discard_workspace: bool,
    capture: Capture,
    before_teardown: BeforeTeardown,
) -> Result<bool>
where
    Capture: FnOnce(Vec<OwnedObject>) -> CaptureFuture,
    CaptureFuture: Future<Output = Result<RmSessionWork>>,
    BeforeTeardown: FnOnce() -> Result<()>,
{
    let mut selected = stop_for_rm_capture(runtime, objects, session, discard_workspace).await?;
    let work = capture(selected.clone()).await?;
    if work == RmSessionWork::Uncaptured {
        preserve_uncaptured_workspace(&mut selected);
    }
    before_teardown()?;
    let failures = teardown_owned_objects(runtime, &selected, force_remove).await;
    if failures > 0 {
        bail!("{failures} object(s) could not be removed (see warnings above)");
    }
    Ok(work != RmSessionWork::Uncaptured)
}

fn preserve_uncaptured_workspace(selected: &mut Vec<OwnedObject>) {
    selected.retain(|object| {
        object.kind != OwnedObjectKind::Volume
            || (object.role.as_deref() == Some(ROLE_CACHE)
                || object.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CACHE))
    });
}

async fn capture_rm_session(
    runtime: &dyn RuntimeAdapter,
    selected: &[OwnedObject],
    session: &SessionId,
    workspace_mode: capscode_core::WorkspaceMode,
    discard_workspace: bool,
) -> Result<RmSessionWork> {
    if discard_workspace {
        note(format!(
            "Discarding session {session} without copy-back, as requested."
        ));
        for volume in selected.iter().filter(|object| {
            object.kind == OwnedObjectKind::Volume
                && object.role.as_deref() != Some(ROLE_CACHE)
                && object.labels.get(LABEL_ROLE).map(String::as_str) != Some(ROLE_CACHE)
        }) {
            note(format!(
                "Discarding workspace volume {} without copy-back, as requested.",
                volume.id
            ));
        }
        return Ok(RmSessionWork::Discarded);
    }
    if workspace_mode == capscode_core::WorkspaceMode::Mount {
        note(format!(
            "Session {session} used a bind-mounted workspace; its files are already on the host."
        ));
        return Ok(RmSessionWork::Captured);
    }

    let resume_hint = rm_resume_hint(selected);
    let receipt = read_git_sync_receipt(session)
        .with_context(|| stopped_context("reading the sync receipt", resume_hint.as_deref()))?;
    let work = match receipt {
        GitSyncReceipt::Git { baseline } => {
            let capsule = selected.iter().find(|object| {
                object.kind == OwnedObjectKind::Container
                    && object.role.as_deref() == Some(ROLE_CAPSULE)
            });
            if let Some(capsule) = capsule {
                let host_repo = crate::session::state::read_session_host_repo(session)
                    .with_context(|| {
                        stopped_context(
                            &format!("reading host repository for session {session}"),
                            resume_hint.as_deref(),
                        )
                    })?;
                warn_if_cwd_differs(&host_repo);
                let (report, wip_skipped, _) = copy_back_git_session(
                    runtime,
                    capsule,
                    session.clone(),
                    host_repo,
                    baseline,
                    false,
                )
                .await
                .with_context(|| {
                    stopped_context(
                        &format!("copy-back failed for session {session}; workspace volume was kept. Retry, or discard it with `capscode rm {session} --discard-workspace`."),
                        resume_hint.as_deref(),
                    )
                })?;
                data(format!(
                    "Copied back session {session} before removal ({} {}, {} {}).",
                    report.commits,
                    if report.commits == 1 {
                        "commit"
                    } else {
                        "commits"
                    },
                    report.files_changed,
                    if report.files_changed == 1 {
                        "file"
                    } else {
                        "files"
                    }
                ));
                if wip_skipped {
                    note(format!(
                        "Kept the workspace volume for session {session}; finish the Git operation or remove it later with `capscode rm {session} --discard-workspace`."
                    ));
                    RmSessionWork::Uncaptured
                } else {
                    RmSessionWork::Captured
                }
            } else {
                note(format!(
                    "Skipped copy-back for session {session}: its capsule is gone, so there is nothing left to copy back."
                ));
                RmSessionWork::Uncaptured
            }
        }
        GitSyncReceipt::Shadow {
            git_dir,
            baseline,
            state_dir,
        } => {
            let capsule = selected.iter().find(|object| {
                object.kind == OwnedObjectKind::Container
                    && object.role.as_deref() == Some(ROLE_CAPSULE)
            });
            if let Some(capsule) = capsule {
                let host_repo = crate::session::state::read_session_host_repo(session)
                    .with_context(|| {
                        stopped_context(
                            &format!("reading host repository for session {session}"),
                            resume_hint.as_deref(),
                        )
                    })?;
                warn_if_cwd_differs(&host_repo);
                let (outcome, unsupported, _) = shadow_copy_back(
                    runtime,
                    capsule,
                    session,
                    &host_repo,
                    &git_dir,
                    baseline,
                    &state_dir,
                    false,
                    false,
                )
                .await
                .with_context(|| {
                    stopped_context(
                        &format!("copy-back failed for session {session}; workspace volume was kept. Retry, or discard it with `capscode rm {session} --discard-workspace`."),
                        resume_hint.as_deref(),
                    )
                })?;
                match outcome {
                    ShadowOutcome::NoChanges => {
                        data(format!("Session {session} had no changes to hand off."));
                    }
                    ShadowOutcome::Applied => {
                        data(format!("Copied back session {session} before removal."));
                    }
                    ShadowOutcome::Staged if unsupported => data(format!(
                        "Session {session} has work that was not captured. Its file sync export was kept."
                    )),
                    ShadowOutcome::Staged => data(format!(
                        "Captured session {session} before removal; its changes are staged on the host, not applied."
                    )),
                }
                if unsupported {
                    note(format!(
                        "Kept the workspace volume and file sync export for session {session}; it contains unsupported files that were not captured. Review the export or remove it later with `capscode rm {session} --discard-workspace`."
                    ));
                    RmSessionWork::Uncaptured
                } else {
                    RmSessionWork::Captured
                }
            } else {
                note(format!(
                    "Skipped copy-back for session {session}: its capsule is gone, so there is nothing left to copy back."
                ));
                RmSessionWork::Uncaptured
            }
        }
        GitSyncReceipt::Missing => {
            note(format!(
                "Skipped copy-back for session {session}: no saved project location was found."
            ));
            RmSessionWork::Uncaptured
        }
    };
    if work == RmSessionWork::Uncaptured {
        for volume in selected.iter().filter(|object| {
            object.kind == OwnedObjectKind::Volume
                && object.role.as_deref() != Some(ROLE_CACHE)
                && object.labels.get(LABEL_ROLE).map(String::as_str) != Some(ROLE_CACHE)
        }) {
            data(format!(
                "Preserved volume {} (its work was not copied back). Reclaim it with `capscode rm {session} --discard-workspace`.",
                volume.id
            ));
        }
    }
    Ok(work)
}

fn rm_resume_hint(objects: &[OwnedObject]) -> Option<String> {
    objects
        .iter()
        .find_map(|object| object.labels.get(LABEL_BOX))
        .map(|name| format!("The box is stopped; `capscode resume {name}` restarts it."))
}

fn stopped_context(message: &str, resume_hint: Option<&str>) -> String {
    match resume_hint {
        Some(hint) => format!("{message}; {hint}"),
        None => message.to_owned(),
    }
}

fn report_session_branch(host_repo: &Path, session: &SessionId) -> Result<()> {
    if let Some(branch) = crate::session::branch::publish_session_branch(host_repo, session)? {
        data(crate::session::branch::branch_hint(&branch));
    }
    Ok(())
}

fn report_session_branch_for_removal(session: &SessionId) -> Result<()> {
    if crate::session::state::read_session_workspace_mode(session)?
        == Some(capscode_core::WorkspaceMode::Mount)
    {
        return Ok(());
    }
    let Some(host_repo) = crate::session::state::read_session_host_repo_if_present(session)? else {
        return Ok(());
    };
    if matches!(
        read_git_sync_receipt(session)?,
        GitSyncReceipt::Shadow { .. }
    ) {
        return Ok(());
    }
    report_session_branch(&host_repo, session)
}

fn remove_session_state_after_confirmation(session: &SessionId) -> Result<()> {
    validate_session_id(session.as_str())?;
    let state_dir = crate::session::state::session_state_dir(session);
    let canonical_state_dir = match canonical_session_state_dir(session, &state_dir)? {
        Some(path) => path,
        None => return Ok(()),
    };
    if !canonical_state_dir.exists() {
        return Ok(());
    }
    let protected = session_state_handoff_data(&canonical_state_dir)?;
    if !protected.is_empty() {
        data(format!(
            "Session {session} has staged or previous handoff data:\n{}",
            protected
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        if !io::stdin().is_terminal() {
            note(format!(
                "Keeping session state at {}; review the listed handoff data before deleting it.",
                canonical_state_dir.display()
            ));
            return Ok(());
        }
        let confirmed = Confirm::with_theme(&crate::ui::theme())
            .with_prompt("Delete this staged and previous handoff data?")
            .default(false)
            .interact()
            .context("reading session state removal confirmation")?;
        if !confirmed {
            note(format!(
                "Keeping session state at {}.",
                canonical_state_dir.display()
            ));
            return Ok(());
        }
    }
    fs::remove_dir_all(&canonical_state_dir)
        .with_context(|| format!("removing session state {}", canonical_state_dir.display()))
}

fn canonical_session_state_dir(session: &SessionId, state_dir: &Path) -> Result<Option<PathBuf>> {
    let sessions_dir = state_dir
        .parent()
        .context("session state directory has no sessions parent")?;
    let canonical_sessions = match sessions_dir.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("canonicalizing sessions directory"),
    };
    let canonical_target = match state_dir.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("canonicalizing session state directory"),
    };
    if canonical_target == canonical_sessions
        || canonical_target.parent() != Some(canonical_sessions.as_path())
        || canonical_target.file_name().and_then(|name| name.to_str()) != Some(session.as_str())
    {
        bail!("refusing to remove session state outside its private sessions directory");
    }
    Ok(Some(canonical_target))
}

pub(crate) fn validate_session_id(session: &str) -> Result<()> {
    let bytes = session.as_bytes();
    let valid = bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && !session.bytes().any(|byte| byte.is_ascii_uppercase());
    if !valid {
        bail!("invalid session id; expected a CapsCode session UUID");
    }
    Ok(())
}

fn session_state_handoff_data(state_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut protected = Vec::new();
    for entry in fs::read_dir(state_dir)
        .with_context(|| format!("listing session state {}", state_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file_type = entry.file_type()?;
        if (name == "staging" || name == "backup" || name.starts_with("previous-handoff."))
            && (file_type.is_file()
                || file_type.is_symlink()
                || (file_type.is_dir()
                    && fs::read_dir(entry.path())?.next().transpose()?.is_some()))
        {
            let path = entry.path();
            let before = protected.len();
            collect_handoff_paths(state_dir, &path, &mut protected)?;
            // Preserve visibility of non-empty directory trees that contain
            // only empty subdirectories.
            if protected.len() == before {
                protected.push(escaped_state_path(state_dir, &path));
            }
        }
    }
    protected.sort();
    Ok(protected)
}

fn collect_handoff_paths(
    state_dir: &Path,
    path: &Path,
    protected: &mut Vec<PathBuf>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting staged handoff path {}", path.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let mut entries = fs::read_dir(path)
            .with_context(|| format!("listing staged handoff path {}", path.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            collect_handoff_paths(state_dir, &entry.path(), protected)?;
        }
    } else {
        protected.push(escaped_state_path(state_dir, path));
    }
    Ok(())
}

fn escaped_state_path(state_dir: &Path, path: &Path) -> PathBuf {
    let relative = path.strip_prefix(state_dir).unwrap_or(path);
    PathBuf::from(capscode_git::escape_shadow_path(relative))
}

async fn teardown_owned_objects(
    runtime: &dyn RuntimeAdapter,
    selected: &[OwnedObject],
    force_remove: bool,
) -> usize {
    // Best-effort teardown: attempt every object and keep going on failure so a
    // single stuck object never leaves the rest behind. Dependency order matters.
    let mut failures = 0usize;
    for object in selected
        .iter()
        .filter(|object| object.kind == OwnedObjectKind::Container)
    {
        let id = capscode_core::ContainerId(object.id.clone());
        if !force_remove {
            if let Err(e) = runtime.stop(&id, crate::boxes::SESSION_STOP_GRACE).await {
                note(format!(
                    "warning: stop container {}: {e}",
                    short_id(&object.id)
                ));
            }
        }
        match runtime.remove(&id, force_remove).await {
            Ok(()) => data(format!("Removed container {}", short_id(&object.id))),
            Err(e) => {
                failures += 1;
                note(format!(
                    "warning: remove container {}: {e}",
                    short_id(&object.id)
                ));
            }
        }
    }
    for object in selected
        .iter()
        .filter(|object| object.kind == OwnedObjectKind::Volume)
    {
        match runtime
            .remove_volume(&capscode_core::VolumeId(object.id.clone()))
            .await
        {
            Ok(()) => data(format!("Removed volume {}", object.id)),
            Err(e) => {
                failures += 1;
                note(format!("warning: remove volume {}: {e}", object.id));
            }
        }
    }
    for object in selected
        .iter()
        .filter(|object| object.kind == OwnedObjectKind::Network)
    {
        match runtime.remove_network(&object.id).await {
            Ok(()) => data(format!("Removed network {}", object.id)),
            Err(e) => {
                failures += 1;
                note(format!("warning: remove network {}: {e}", object.id));
            }
        }
    }
    failures
}

async fn stop_session_containers(
    runtime: &dyn RuntimeAdapter,
    objects: &[OwnedObject],
    grace: Duration,
) -> Result<()> {
    let mut errors = Vec::new();
    for role in [ROLE_CAPSULE, ROLE_GATEWAY] {
        for object in objects.iter().filter(|object| {
            object.kind == OwnedObjectKind::Container && object.role.as_deref() == Some(role)
        }) {
            if let Err(error) = runtime
                .stop(&capscode_core::ContainerId(object.id.clone()), grace)
                .await
            {
                errors.push(format!(
                    "session {} container {}: {error}",
                    object.session.as_ref().map_or("unknown", SessionId::as_str),
                    short_id(&object.id)
                ));
            }
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}

async fn stop_for_rm_capture(
    runtime: &dyn RuntimeAdapter,
    objects: &[OwnedObject],
    session: &SessionId,
    discard_workspace: bool,
) -> Result<Vec<OwnedObject>> {
    if !has_running_session_container(objects) {
        return Ok(objects.to_vec());
    }
    let target = rm_target_label(objects, session);
    // DECISION: use one truthful stop message for capture, bind mount, and discard flows.
    note(format!(
        "Stopping {target} before workspace handling and removal."
    ));
    if let Err(error) =
        stop_session_containers(runtime, objects, crate::boxes::SESSION_STOP_GRACE).await
    {
        // DECISION: with --discard-workspace nothing is captured, so a container that will not
        // stop must not block removal; teardown removes it as before.
        if discard_workspace {
            note(format!(
                "warning: could not stop session {session} gracefully; removing it anyway because --discard-workspace was given. {error:#}"
            ));
            return Ok(objects.to_vec());
        }
        bail!(
            "could not stop session {session}; nothing was removed. Retry, or `capscode rm {session} --discard-workspace`. {error:#}"
        );
    }
    let resume_hint = rm_resume_hint(objects);
    let refreshed = runtime.list_owned().await.with_context(|| {
        stopped_context(
            &format!("refreshing session {session} after stop; nothing was removed"),
            resume_hint.as_deref(),
        )
    })?;
    Ok(refreshed
        .into_iter()
        .filter(|object| object.session.as_ref().is_some_and(|id| id == session))
        .collect())
}

fn rm_target_label(objects: &[OwnedObject], session: &SessionId) -> String {
    // DECISION: classic sessions have no box resume command, so their stop message uses the
    // session id while box messages use the user-facing box name.
    objects
        .iter()
        .find_map(|object| object.labels.get(LABEL_BOX))
        .map(|name| format!("box {name}"))
        .unwrap_or_else(|| format!("session {session}"))
}

async fn stop_and_recheck_uninstall(
    runtime: &dyn RuntimeAdapter,
    index_path: &Path,
) -> Result<(Vec<OwnedObject>, Vec<crate::boxes::BoxWorkspace>)> {
    // DECISION: refresh the inventory here instead of trusting the plan's first listing, which
    // may have failed or missed a box that is still running.
    let objects = runtime
        .list_owned()
        .await
        .context("listing CapsCode-owned objects before uninstall stop; nothing was removed")?;
    stop_session_containers(runtime, &objects, crate::boxes::SESSION_STOP_GRACE)
        .await
        .context("could not stop every box container; boxes were not removed")?;
    let stopped_objects = runtime
        .list_owned()
        .await
        .context("listing stopped boxes before uninstall recheck; nothing was removed")?;
    let stopped_boxes = crate::boxes::workspaces_at_risk_from(
        &stopped_objects,
        crate::boxes::index::read_from(index_path),
        Some(runtime),
    )
    .await?;
    Ok((stopped_objects, stopped_boxes))
}

async fn uninstall_runtime_core(
    runtime: &dyn RuntimeAdapter,
    index_path: &Path,
    discard_workspace: bool,
) -> Result<usize> {
    let (stopped_objects, stopped_boxes) = stop_and_recheck_uninstall(runtime, index_path).await?;
    let stopped_count = box_container_count(&stopped_objects);
    if let Some(message) = uninstall_recheck_error(stopped_count, &stopped_boxes, discard_workspace)
    {
        bail!("{message}");
    }
    Ok(teardown_owned_objects(runtime, &stopped_objects, true).await)
}

fn has_running_session_container(objects: &[OwnedObject]) -> bool {
    objects.iter().any(|object| {
        object.kind == OwnedObjectKind::Container
            && matches!(object.role.as_deref(), Some(ROLE_CAPSULE | ROLE_GATEWAY))
            && object.state.as_deref() == Some("running")
    })
}

fn risky_box_names(boxes: &[crate::boxes::BoxWorkspace]) -> String {
    boxes
        .iter()
        .filter_map(|workspace| {
            workspace
                .risk
                .as_ref()
                .map(|reason| format!("{} ({reason})", workspace.name))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn box_container_count(objects: &[OwnedObject]) -> usize {
    objects
        .iter()
        .filter(|object| {
            object.kind == OwnedObjectKind::Container && object.labels.contains_key(LABEL_BOX)
        })
        .map(|object| {
            object
                .session
                .as_ref()
                .map(|session| session.as_str().to_owned())
                .or_else(|| object.labels.get(capscode_core::LABEL_SESSION).cloned())
                .unwrap_or_else(|| object.labels.get(LABEL_BOX).cloned().unwrap_or_default())
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

fn uninstall_recheck_error(
    stopped_count: usize,
    boxes: &[crate::boxes::BoxWorkspace],
    discard_workspace: bool,
) -> Option<String> {
    let at_risk = boxes.iter().any(|workspace| workspace.risk.is_some());
    (at_risk && !discard_workspace).then(|| {
        format!(
            "uninstall stopped {stopped_count} box(es) but found work in {} that is not on your machine; run `capscode sync <box>` (works on a stopped box), then rerun uninstall, or rerun with --discard-workspace. The boxes were stopped, not removed; `capscode resume <box>` restarts one",
            risky_box_names(boxes)
        )
    })
}

/// Without a runtime listing, box volumes stay on the daemon but uninstall would still delete the
/// box metadata in `~/.capscode` that `sync` needs to find their projects.
fn uninstall_unlisted_boxes_guard(
    inventory_complete: bool,
    discard_workspace: bool,
    indexed_boxes: Result<usize>,
) -> Result<()> {
    if inventory_complete || discard_workspace {
        return Ok(());
    }
    match indexed_boxes {
        Ok(0) => Ok(()),
        Ok(count) => bail!(
            "cannot list runtime objects, so {count} box(es) in boxes.json cannot be checked for \
             unsynced work; start the container runtime and rerun uninstall, or rerun with \
             --discard-workspace"
        ),
        Err(error) => Err(error).context(
            "cannot list runtime objects or read boxes.json, so boxes cannot be checked for \
             unsynced work; start the container runtime and rerun uninstall, or rerun with \
             --discard-workspace",
        ),
    }
}

pub async fn uninstall(
    runtime_flag: Option<RuntimeArg>,
    remove_images: bool,
    yes: bool,
    discard_workspace: bool,
) -> Result<()> {
    let home = Config::config_home();
    let mut failures = 0usize;

    let mut completion_files = Vec::new();
    for path in crate::shell_completion::startup_files() {
        match crate::shell_completion::has_startup_block(&path) {
            Ok(true) => completion_files.push(path),
            Ok(false) => {}
            Err(error) => {
                failures += 1;
                note(format!(
                    "warning: inspect completion block in {}: {error:#}",
                    path.display()
                ));
            }
        }
    }

    let runtime_pref = uninstall_runtime_pref(runtime_flag);
    let mut runtime = None;
    let mut inventory_complete = false;
    let mut objects = Vec::new();
    let mut images: Vec<LocalImageInfo> = Vec::new();
    match selected_runtime_pref(runtime_pref).await {
        Ok((adapter, endpoint_error)) => {
            if let Some(error) = endpoint_error {
                // DECISION: uninstall uses the runtime API directly, so endpoint lookup is informational here.
                note(format!(
                    "warning: read selected runtime endpoint: {error:#}"
                ));
            }
            match adapter.list_owned().await {
                Ok(found) => {
                    objects = found;
                    inventory_complete = true;
                }
                Err(error) => {
                    failures += 1;
                    note(format!(
                        "warning: list CapsCode-owned runtime objects: {error}"
                    ));
                }
            }
            if remove_images {
                match adapter.list_local_image_details().await {
                    Ok(found) => {
                        images = found
                            .into_iter()
                            .filter(|image| image.reference.starts_with("capscode/"))
                            .collect();
                    }
                    Err(error) => {
                        failures += 1;
                        note(format!("warning: list local image labels: {error}"));
                    }
                }
            }
            runtime = Some(adapter);
        }
        Err(error) => {
            failures += 1;
            note(format!("warning: select container runtime: {error}"));
        }
    }

    uninstall_unlisted_boxes_guard(
        inventory_complete,
        discard_workspace,
        crate::boxes::index::read().map(|entries| entries.len()),
    )?;
    let boxes = crate::boxes::workspaces_at_risk(&objects, runtime.as_deref()).await?;
    print_uninstall_plan(
        &objects,
        &boxes,
        &home,
        &completion_files,
        remove_images,
        &images,
    );
    // DECISION: the override is --discard-workspace, matching `rm`; --yes alone never deletes
    // unsynced box work.
    let at_risk = boxes
        .iter()
        .filter(|workspace| workspace.risk.is_some())
        .count();
    if at_risk > 0 && !discard_workspace {
        bail!(
            "uninstall would delete work in {at_risk} box(es) that is not on your machine: {}; \
             sync each box before uninstall, or rerun with --discard-workspace to delete it",
            risky_box_names(&boxes)
        );
    }
    if !yes {
        if !io::stdin().is_terminal() {
            data("Uninstall aborted: stdin is not interactive; rerun with --yes to confirm.");
            return Ok(());
        }
        let confirmed = {
            let _cursor = crate::terminal::CursorGuard::new();
            Confirm::with_theme(&crate::ui::theme())
                .with_prompt("Proceed with uninstall?")
                .default(false)
                .interact()
                .context("reading uninstall confirmation")?
        };
        if !confirmed {
            data("Uninstall cancelled.");
            return Ok(());
        }
    }

    if let Some(runtime) = runtime.as_deref() {
        let index_path = crate::boxes::index::path();
        failures += uninstall_runtime_core(runtime, &index_path, discard_workspace).await?;
    }

    for path in &completion_files {
        match crate::shell_completion::remove_startup_block(path) {
            Ok(true) => data(format!(
                "Removed shell completion block from {}",
                path.display()
            )),
            Ok(false) => data(format!("No shell completion block in {}", path.display())),
            Err(error) => {
                failures += 1;
                note(format!(
                    "warning: remove completion block from {}: {error:#}",
                    path.display()
                ));
            }
        }
    }

    match remove_config_home(&home) {
        Ok(ConfigHomeRemoval::Removed) => data(format!("Removed config home {}", home.display())),
        Ok(ConfigHomeRemoval::Missing) => data(format!("Config home not found: {}", home.display())),
        Ok(ConfigHomeRemoval::NotCapscodeHome) => note(format!(
            "warning: skipped config home {} because it does not look like a CapsCode home (no config.toml, vpn/, or providers.yaml); remove it manually if intended",
            home.display()
        )),
        Ok(ConfigHomeRemoval::Unsafe) => note(format!(
            "warning: refused to remove config home {} because it resolves to a protected path (root, cwd, $HOME, or a symlink)",
            home.display()
        )),
        Err(error) => {
            failures += 1;
            note(format!("warning: remove config home {}: {error:#}", home.display()));
        }
    }

    if remove_images {
        if let Some(runtime) = runtime.as_deref() {
            for image in images
                .iter()
                .filter(|image| image_created_by_capscode(image))
            {
                match runtime.remove_image(&image.reference).await {
                    Ok(()) => data(format!("Removed image {}", image.reference)),
                    Err(error) => {
                        failures += 1;
                        note(format!(
                            "warning: remove image {}: {error}",
                            image.reference
                        ));
                    }
                }
            }
        }
    }

    print_uninstall_preservation_notes();
    if failures > 0 {
        bail!("{failures} uninstall step(s) could not be completed (see warnings above)");
    }
    Ok(())
}

fn uninstall_runtime_pref(runtime_flag: Option<RuntimeArg>) -> RuntimePref {
    if let Some(runtime) = runtime_flag {
        return runtime.into();
    }
    if !Config::config_file_path().is_file() {
        return Config::default().runtime;
    }
    match config::load(None) {
        Ok(config) => config.runtime,
        Err(error) => {
            note(format!(
                "warning: read runtime from configuration: {error:#}; using automatic detection"
            ));
            RuntimePref::Auto
        }
    }
}

fn print_uninstall_plan(
    objects: &[OwnedObject],
    boxes: &[crate::boxes::BoxWorkspace],
    home: &Path,
    completion_files: &[PathBuf],
    remove_images: bool,
    images: &[LocalImageInfo],
) {
    let count = |kind| objects.iter().filter(|object| object.kind == kind).count();
    data("Uninstall plan:");
    data(format!(
        "  Runtime objects: {} container(s), {} volume(s), {} network(s)",
        count(OwnedObjectKind::Container),
        count(OwnedObjectKind::Volume),
        count(OwnedObjectKind::Network)
    ));
    if !boxes.is_empty() {
        data("  Box workspaces deleted:");
        for workspace in boxes {
            match &workspace.risk {
                Some(risk) => data(format!("    {} ({risk})", workspace.name)),
                None => data(format!("    {}", workspace.name)),
            }
        }
    }
    data(format!("  Config home: {}", home.display()));
    if completion_files.is_empty() {
        data("  Shell completion blocks: none");
    } else {
        data("  Shell completion blocks:");
        for path in completion_files {
            data(format!("    {}", path.display()));
        }
    }
    if remove_images {
        let owned = images
            .iter()
            .filter(|image| image_created_by_capscode(image))
            .count();
        let left_alone = images.len() - owned;
        data(format!("  Local capscode/* images: {owned} to remove, {left_alone} not created by CapsCode and left alone"));
        for image in images {
            if image_created_by_capscode(image) {
                data(format!("    {}", image.reference));
            } else {
                data(format!(
                    "    {} (not created by CapsCode, left alone)",
                    image.reference
                ));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigHomeRemoval {
    Removed,
    Missing,
    NotCapscodeHome,
    Unsafe,
}

fn looks_like_capscode_home(path: &Path) -> bool {
    path.join("config.toml").is_file()
        || path.join("vpn").is_dir()
        || path.join("providers.yaml").is_file()
}

fn remove_config_home(path: &Path) -> Result<ConfigHomeRemoval> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ConfigHomeRemoval::Missing);
        }
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(ConfigHomeRemoval::Unsafe);
    }
    // Always require a capscode marker: never rm -rf a directory that does not look like a
    // capscode home, not even the default ~/.capscode path.
    if !looks_like_capscode_home(path) {
        return Ok(ConfigHomeRemoval::NotCapscodeHome);
    }
    // Compare against canonicalized cwd/$HOME and remove via the canonical path so a symlink
    // component cannot redirect the delete; re-check the marker on the canonical path right
    // before removal to shrink the TOCTOU window.
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    let current = std::env::current_dir()
        .ok()
        .and_then(|dir| dir.canonicalize().ok());
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| home.canonicalize().ok());
    if canonical.parent().is_none()
        || current.as_deref() == Some(canonical.as_path())
        || user_home.as_deref() == Some(canonical.as_path())
        || !looks_like_capscode_home(&canonical)
    {
        return Ok(ConfigHomeRemoval::Unsafe);
    }
    fs::remove_dir_all(&canonical).with_context(|| format!("removing {}", canonical.display()))?;
    Ok(ConfigHomeRemoval::Removed)
}

fn print_uninstall_preservation_notes() {
    data("Note: keychain entry 'claude-setup-token' is user-owned and was not removed.");
    #[cfg(target_os = "macos")]
    data("To remove it manually: security delete-generic-password -s claude-setup-token");
    #[cfg(target_os = "linux")]
    data("To remove it manually: secret-tool clear service claude-setup-token");
    #[cfg(target_os = "windows")]
    data("To remove it manually: cmdkey /delete:claude-setup-token");
    data("The capscode binary was not removed; use 'cargo uninstall capscode' or your package manager (for Homebrew: 'brew uninstall capscode').");
}

pub async fn sync(
    runtime: Arc<dyn RuntimeAdapter>,
    requested_session: Option<String>,
    verbose: bool,
    box_name: Option<&str>,
) -> Result<Option<crate::boxes::SyncIdentity>> {
    if let Some(session) = requested_session.as_deref() {
        validate_session_id(session)?;
    }
    let capsules: Vec<_> = runtime
        .list_owned()
        .await?
        .into_iter()
        .filter(|object| {
            object.kind == OwnedObjectKind::Container
                && object.role.as_deref() == Some(ROLE_CAPSULE)
                && object.session.is_some()
        })
        .collect();
    let capsule = resolve_sync_capsule(&capsules, requested_session.as_deref())?;
    let session = capsule.session.clone().expect("filtered above");
    let workspace_mode = crate::session::state::read_session_workspace_mode(&session)?
        .unwrap_or(capscode_core::WorkspaceMode::Copy);
    if workspace_mode == capscode_core::WorkspaceMode::Mount {
        data(format!(
            "sync is a no-op for session {session}: its workspace is bind-mounted and the agent already writes directly to the host directory."
        ));
        return Ok(None);
    }
    let host_repo = crate::session::state::read_session_host_repo(&session)
        .with_context(|| format!("reading host repository for session {session}"))?;
    warn_if_cwd_differs(&host_repo);
    let baseline = match read_git_sync_receipt(&session)? {
        GitSyncReceipt::Git { baseline } => baseline,
        GitSyncReceipt::Shadow {
            git_dir,
            baseline,
            state_dir,
        } => {
            // A non-git workspace is read as a fresh file snapshot. If it is
            // running, this v0.1 path stops it, so ask before making the stop
            // terminal. A stopped container is copied directly from its volume.
            if capsule_is_running(capsule) && !confirm_sync_that_stops_box(&session, box_name)? {
                note("Sync cancelled; the box is still running.");
                return Ok(None);
            }
            let (outcome, unsupported, identity) = shadow_copy_back(
                runtime.as_ref(),
                capsule,
                &session,
                &host_repo,
                &git_dir,
                baseline,
                &state_dir,
                verbose,
                box_name.is_some(),
            )
            .await?;
            let recorded = !unsupported && !matches!(outcome, ShadowOutcome::Staged);
            data(match outcome {
                ShadowOutcome::NoChanges => {
                    format!(
                        "Session {session} had no changes to hand off; {}",
                        stopped_sync_hint(box_name)
                    )
                }
                ShadowOutcome::Applied => {
                    format!(
                        "Handed off session {session} to {}; {}",
                        host_repo.display(),
                        stopped_sync_hint(box_name)
                    )
                }
                ShadowOutcome::Staged => format!(
                    "Session {session} was captured but not applied; {}",
                    stopped_sync_hint(box_name)
                ),
            });
            let identity = if recorded { identity } else { None };
            return Ok(identity);
        }
        GitSyncReceipt::Missing => {
            bail!(
                "session {session} has no saved project location; refusing to guess the host repository"
            )
        }
    };
    let was_running = capsule_is_running(capsule);
    let (report, wip_skipped, exported_oid) = copy_back_git_session(
        runtime.as_ref(),
        capsule,
        session.clone(),
        host_repo.clone(),
        baseline,
        verbose,
    )
    .await?;
    report_session_branch(&host_repo, &session)?;
    let box_state = if was_running {
        "the box remains running"
    } else if box_name.is_some() {
        "the box remains stopped"
    } else {
        "the session remains stopped"
    };
    let commit_word = if report.commits == 1 {
        "commit"
    } else {
        "commits"
    };
    let file_word = if report.files_changed == 1 {
        "file"
    } else {
        "files"
    };
    data(format!(
        "Synced {} {commit_word} (+{} -{}, {} {file_word}); {box_state}.",
        report.commits, report.insertions, report.deletions, report.files_changed
    ));
    if !was_running {
        if let Some(name) = box_name {
            data(format!(
                "Enter it again with `capscode {name}` or `capscode resume {name}`."
            ));
        }
    }
    Ok(
        (!wip_skipped && box_name.is_some()).then(|| crate::boxes::SyncIdentity {
            oid: exported_oid.to_string(),
            kind: "git".to_owned(),
        }),
    )
}

fn stopped_sync_hint(box_name: Option<&str>) -> String {
    if let Some(name) = box_name {
        format!("the box remains stopped; enter it again with `capscode {name}` or `capscode resume {name}`")
    } else {
        "the session remains stopped".to_string()
    }
}

/// Capture a non-git workspace and offer it to the host.
///
/// Shared by `sync` and `rm` so the two cannot diverge on what "copy back"
/// means for a non-git box. The capture is always fresh: handing off a snapshot
/// an earlier call took would report stale work as current.
#[allow(clippy::too_many_arguments)]
async fn shadow_copy_back(
    runtime: &dyn RuntimeAdapter,
    capsule: &OwnedObject,
    session: &SessionId,
    host_repo: &Path,
    git_dir: &Path,
    baseline: git2::Oid,
    state_dir: &Path,
    verbose: bool,
    record_identity: bool,
) -> Result<(ShadowOutcome, bool, Option<crate::boxes::SyncIdentity>)> {
    let container = capscode_core::ContainerId(capsule.id.clone());
    if capsule_is_running(capsule) {
        note(format!(
            "Stopping session {session} so its workspace can be captured cleanly."
        ));
        crate::session::shadow::quiesce_and_capture(runtime, &container, state_dir, verbose)
            .await?;
    } else {
        note(format!(
            "Reading the stopped workspace for session {session}."
        ));
        capscode_git::recapture_shadow_export(&container, state_dir, verbose)
            .await
            .context("exporting the stopped non-git workspace")?;
    }
    // An earlier round's staging and manifest would otherwise block this one.
    // They are kept, not deleted: staging holds whatever that round did not
    // apply, and backup holds the host files it did overwrite.
    if let Some(retired) = capscode_git::retire_previous_handoff(state_dir)
        .context("making room for a fresh handoff")?
    {
        note(format!(
            "Kept the previous handoff at {}.",
            retired.display()
        ));
    }
    let handoff =
        capscode_git::shadow_egress(&container, host_repo, git_dir, baseline, state_dir, verbose)
            .await
            .context("comparing the captured workspace against its file sync baseline")?;
    let has_unsupported_work = !handoff.unsupported.is_empty();
    let (outcome, identity) = apply_shadow_handoff_then_read_identity(
        record_identity,
        || {
            crate::session::shadow::present_and_apply(
                &handoff,
                host_repo,
                io::stdin().is_terminal(),
            )
        },
        || crate::boxes::workspace_identity(session.as_str(), runtime),
    )
    .await?;
    Ok((outcome, has_unsupported_work, identity))
}

async fn apply_shadow_handoff_then_read_identity<Apply, ReadIdentity, IdentityFuture>(
    record_identity: bool,
    apply: Apply,
    read_identity: ReadIdentity,
) -> Result<(ShadowOutcome, Option<crate::boxes::SyncIdentity>)>
where
    Apply: FnOnce() -> Result<ShadowOutcome>,
    ReadIdentity: FnOnce() -> IdentityFuture,
    IdentityFuture: Future<Output = Result<crate::boxes::SyncIdentity>>,
{
    // DECISION: apply the user's accepted handoff before reading identity, and leave the box
    // unsynced with a warning if that read fails after the host changes were applied.
    let outcome = apply()?;
    let identity = if record_identity {
        match read_identity().await {
            Ok(identity) => Some(identity),
            Err(error) => {
                note(format!(
                    "warning: the handoff was applied, but the box sync receipt could not be updated: {error:#}; the box remains unsynced"
                ));
                None
            }
        }
    } else {
        None
    };
    Ok((outcome, identity))
}

fn resolve_sync_capsule<'a>(
    capsules: &'a [OwnedObject],
    requested_session: Option<&str>,
) -> Result<&'a OwnedObject> {
    if let Some(requested) = requested_session {
        validate_session_id(requested)?;
    }
    for session in capsules
        .iter()
        .filter_map(|capsule| capsule.session.as_ref())
    {
        validate_session_id(session.as_str())?;
    }
    let mut box_sessions = capsules
        .iter()
        .filter_map(|capsule| capsule.session.as_ref().map(|session| session.as_str()))
        .collect::<Vec<_>>();
    box_sessions.sort_unstable();
    box_sessions.dedup();
    let boxes = if box_sessions.is_empty() {
        "none".to_string()
    } else {
        box_sessions.join(", ")
    };

    if let Some(requested) = requested_session {
        return capsules
            .iter()
            .find(|capsule| {
                capsule
                    .session
                    .as_ref()
                    .is_some_and(|session| session.as_str() == requested)
            })
            .with_context(|| {
                format!(
                    "no capsule was found for session {requested}; boxes with capsules: {boxes}"
                )
            });
    }

    match capsules {
        [] => bail!("no CapsCode capsule found; boxes with capsules: {boxes}"),
        [capsule] => Ok(capsule),
        _ => {
            bail!("multiple boxes have capsules; specify a session (boxes: {boxes})")
        }
    }
}

fn capsule_is_running(capsule: &OwnedObject) -> bool {
    capsule.state.as_deref() == Some("running")
}

fn confirm_sync_that_stops_box(session: &SessionId, box_name: Option<&str>) -> Result<bool> {
    if !io::stdin().is_terminal() {
        if let Some(name) = box_name {
            bail!("This sync will stop box {name}; run `capscode sync {name}` from an interactive terminal to confirm. You can enter it again with `capscode {name}`.");
        }
        bail!("This sync will stop session {session}; run `capscode sync {session}` from an interactive terminal to confirm.");
    }
    Confirm::with_theme(&crate::ui::theme())
        .with_prompt(if box_name.is_some() {
            "Sync will stop this box. You can enter it again afterward. Capture its work now?"
        } else {
            "Sync will stop this session. Capture its work now?"
        })
        .default(false)
        .interact()
        .context("reading sync stop confirmation")
}

fn warn_if_cwd_differs(host_repo: &Path) {
    match std::env::current_dir() {
        Ok(cwd) if cwd != host_repo => note(format!(
            "warning: current directory {} differs from session host repository {}; syncing to the persisted repository",
            cwd.display(),
            host_repo.display()
        )),
        Ok(_) => {}
        Err(error) => note(format!(
            "warning: could not read the current directory ({error}); syncing to persisted host repository {}",
            host_repo.display()
        )),
    }
}

async fn copy_back_git_session(
    runtime: &dyn RuntimeAdapter,
    capsule: &OwnedObject,
    session: SessionId,
    host_repo: PathBuf,
    baseline: Option<git2::Oid>,
    verbose: bool,
) -> Result<(EgressReport, bool, git2::Oid)> {
    let container = capscode_core::ContainerId(capsule.id.clone());
    if !capsule_is_running(capsule) {
        return copy_back_stopped_git_session(
            runtime, capsule, session, host_repo, baseline, verbose,
        )
        .await;
    }

    let command = format!(
        "set -e; {}; git -C /workspace add -A; if ! git -C /workspace diff --cached --quiet; then git -C /workspace -c user.name=capsule -c user.email=capsule@localhost -c commit.gpgsign=false -c core.hooksPath=/dev/null commit --no-verify -m 'capsule: sync WIP'; fi",
        crate::session::git_operation_guard_shell()
    );
    let commit = runtime
        .exec(
            &container,
            ExecSpec {
                cmd: vec!["sh".into(), "-c".into(), command],
                user: Some("agent".into()),
                ..Default::default()
            },
        )
        .await?;
    let wip_skipped = String::from_utf8_lossy(&commit.stdout).contains("skipping the WIP commit");
    if wip_skipped {
        note(String::from_utf8_lossy(&commit.stdout).trim().to_string());
    }
    if commit.exit_code != 0 {
        bail!(
            "sync WIP commit failed (exit {}): {}",
            commit.exit_code,
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    let report = DefaultGitEngine::new()
        .egress(
            &EgressPlan {
                host_repo: host_repo.clone(),
                session: session.clone(),
                capsule: container,
                workspace_path: PathBuf::from("/workspace"),
                baseline,
                verbose,
            },
            runtime,
        )
        .await?;
    let exported_oid = fetched_git_head(&host_repo, &session)?;
    Ok((report, wip_skipped, exported_oid))
}

async fn copy_back_stopped_git_session(
    runtime: &dyn RuntimeAdapter,
    capsule: &OwnedObject,
    session: SessionId,
    host_repo: PathBuf,
    baseline: Option<git2::Oid>,
    verbose: bool,
) -> Result<(EgressReport, bool, git2::Oid)> {
    note(format!(
        "Reading the stopped Git workspace for session {session}."
    ));
    let temporary = tempfile::tempdir().context("creating a temporary stopped-workspace copy")?;
    let workspace = temporary.path().join("workspace");
    fs::create_dir(&workspace).context("creating the stopped-workspace copy directory")?;
    let cli = match runtime.kind() {
        RuntimeKind::AppleContainer => "container",
        RuntimeKind::Docker | RuntimeKind::OrbStack => "docker",
    };
    let source = format!("{}:/workspace/.", capsule.id);
    let output = TokioCommand::new(cli)
        .args(["cp", &source, workspace.to_string_lossy().as_ref()])
        .output()
        .await
        .with_context(|| format!("copying stopped workspace for session {session}"))?;
    if !output.status.success() {
        bail!(
            "could not read stopped workspace for session {session}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    capscode_git::validate_exported_git_repository(&workspace)
        .context("validating the copied stopped Git workspace")?;
    remove_stale_exported_index_lock(&workspace)?;
    let repository = Repository::open(&workspace)
        .with_context(|| format!("opening stopped Git workspace for session {session}"))?;
    let wip_skipped = git_operation_in_progress(&repository).is_some();
    commit_worktree_snapshot(&repository, "capsule: sync WIP")?;
    let report = fetch_stopped_git_snapshot(&host_repo, &repository, &session, baseline, verbose)?;
    let exported_oid = fetched_git_head(&host_repo, &session)?;
    Ok((report, wip_skipped, exported_oid))
}

/// Runs only after `validate_exported_git_repository`, so `.git` cannot redirect the removal into
/// a host repository; the directory check repeats that guarantee locally.
fn remove_stale_exported_index_lock(workspace: &Path) -> Result<()> {
    let git_dir = workspace.join(".git");
    let metadata = fs::symlink_metadata(&git_dir)
        .with_context(|| format!("inspecting {}", git_dir.display()))?;
    if !metadata.is_dir() {
        bail!(
            "capsule export {} is not a directory; refusing to touch its index lock",
            git_dir.display()
        );
    }
    let lock = git_dir.join("index.lock");
    match fs::remove_file(&lock) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "removing stale index lock from temporary workspace copy {}",
                lock.display()
            )
        }),
    }
}

fn fetched_git_head(host_repo: &Path, session: &SessionId) -> Result<git2::Oid> {
    let repository = Repository::open(host_repo)
        .with_context(|| format!("opening host repository {}", host_repo.display()))?;
    let name = format!("refs/capscode/{}/head", session.as_str());
    let reference = repository
        .find_reference(&name)
        .with_context(|| format!("finding fetched capsule ref {name}"))?;
    let oid = reference
        .target()
        .with_context(|| format!("fetched capsule ref {name} is symbolic"))?;
    Ok(oid)
}

/// Stage the copied worktree with libgit2 and write a capsule-local checkpoint.
/// This never invokes repository hooks or a configured signing program.
fn commit_worktree_snapshot(repository: &Repository, message: &str) -> Result<Option<git2::Oid>> {
    if let Some(marker) = git_operation_in_progress(repository) {
        note(format!(
            "Git operation in progress ({marker}); skipping the WIP commit and syncing committed work."
        ));
        return Ok(None);
    }
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let mut index = repository.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.update_all(["*"], None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    if parent
        .as_ref()
        .is_some_and(|commit| commit.tree_id() == tree_id)
    {
        return Ok(None);
    }
    let tree = repository.find_tree(tree_id)?;
    let signature = git2::Signature::now("capsule", "capsule@localhost")?;
    let parents = parent.iter().collect::<Vec<_>>();
    let commit = repository.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?;
    Ok(Some(commit))
}

fn git_operation_in_progress(repository: &Repository) -> Option<&'static str> {
    crate::session::GIT_OPERATION_MARKERS
        .iter()
        .copied()
        .find(|marker| repository.path().join(marker).exists())
}

fn fetch_stopped_git_snapshot(
    host_repo: &Path,
    source: &Repository,
    session: &SessionId,
    baseline: Option<git2::Oid>,
    _verbose: bool,
) -> Result<EgressReport> {
    let source_path = source
        .workdir()
        .context("stopped Git workspace has no worktree")?;
    let source_path = source_path.to_string_lossy();
    let prefix = format!("refs/capscode/{}", session.as_str());
    let refspecs = [
        format!("+HEAD:{prefix}/head"),
        format!("+refs/heads/*:{prefix}/heads/*"),
        format!("+refs/capscode/baseline:{prefix}/baseline"),
    ];
    let refspecs = refspecs.iter().map(String::as_str).collect::<Vec<_>>();
    let host = Repository::open(host_repo)
        .with_context(|| format!("opening host Git repository {}", host_repo.display()))?;
    host.remote_anonymous(&source_path)?
        .fetch(&refspecs, None, None)
        .context("fetching stopped capsule commits")?;
    verify_stopped_git_baseline(&host, session, baseline)?;
    stopped_git_egress_report(&host, session, baseline)
}

fn verify_stopped_git_baseline(
    repository: &Repository,
    session: &SessionId,
    expected: Option<git2::Oid>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let name = format!("refs/capscode/{}/baseline", session.as_str());
    let actual = repository
        .find_reference(&name)
        .with_context(|| format!("capsule baseline ref {name} was not fetched"))?
        .target()
        .context("fetched capsule baseline ref is symbolic")?;
    if actual != expected {
        bail!("fetched capsule baseline does not match the saved ingress snapshot");
    }
    repository
        .find_commit(expected)
        .context("fetched capsule baseline commit is missing")?;
    Ok(())
}

fn stopped_git_egress_report(
    repository: &Repository,
    session: &SessionId,
    baseline: Option<git2::Oid>,
) -> Result<EgressReport> {
    let head_name = format!("refs/capscode/{}/head", session.as_str());
    let head = repository
        .find_reference(&head_name)
        .with_context(|| format!("missing {head_name} after egress"))?
        .target()
        .context("capsule HEAD ref is symbolic")?;
    let base = baseline.unwrap_or_else(|| {
        repository
            .find_commit(head)
            .ok()
            .and_then(|commit| commit.parents().next().map(|parent| parent.id()))
            .unwrap_or(head)
    });
    let commits = if base == head {
        0
    } else {
        let mut walk = repository.revwalk()?;
        walk.push(head)?;
        walk.hide(base)?;
        let mut count = 0;
        for commit in walk {
            commit?;
            count += 1;
        }
        count
    };
    let (files_changed, insertions, deletions) = if base == head {
        (0, 0, 0)
    } else {
        let base_tree = repository.find_commit(base)?.tree()?;
        let head_tree = repository.find_commit(head)?.tree()?;
        let stats = repository
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?
            .stats()?;
        (
            stats.files_changed() as u32,
            stats.insertions() as u32,
            stats.deletions() as u32,
        )
    };
    Ok(EgressReport {
        base_sha: base,
        commits,
        files_changed,
        insertions,
        deletions,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitSyncReceipt {
    Git {
        baseline: Option<git2::Oid>,
    },
    /// A non-git workspace, recovered by comparing the capsule against the
    /// file sync baseline. These three values are what that comparison needs, and
    /// the persisted project location lets `sync` and `rm` target the right project,
    /// refuse rather than perform the handoff themselves.
    Shadow {
        git_dir: PathBuf,
        baseline: git2::Oid,
        state_dir: PathBuf,
    },
    Missing,
}

impl GitSyncReceipt {
    /// Whether this session's work can be copied back at all.
    ///
    /// Both kinds can, by different routes: a git workspace through its commits,
    /// a file sync workspace through a fresh capture. Only a missing receipt leaves
    /// nothing to copy. Drives the console's confirmation wording — promising
    /// copy-back for a session that cannot have any is how a user loses work by
    /// saying yes.
    #[cfg(test)]
    pub(crate) fn can_copy_back(&self) -> bool {
        !matches!(self, GitSyncReceipt::Missing)
    }
}

pub(crate) fn read_git_sync_receipt(session: &SessionId) -> Result<GitSyncReceipt> {
    let receipt_path = crate::session::state::session_state_dir(session).join("receipt.json");
    let receipt = match fs::read(&receipt_path) {
        Ok(serialized) => Some(
            serde_json::from_slice::<IngressReceipt>(&serialized)
                .with_context(|| format!("parsing session receipt {}", receipt_path.display()))?,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading session receipt {}", receipt_path.display()))
        }
    };
    classify_sync_receipt(receipt)
}

/// The receipt -> sync-capability decision, split out from the file read so it is
/// testable without a session state dir on disk.
fn classify_sync_receipt(receipt: Option<IngressReceipt>) -> Result<GitSyncReceipt> {
    match receipt {
        Some(IngressReceipt::GitSnapshot { baseline }) => Ok(GitSyncReceipt::Git {
            baseline: baseline
                .as_deref()
                .map(str::parse::<git2::Oid>)
                .transpose()
                .context("invalid GitSnapshot baseline OID")?,
        }),
        Some(IngressReceipt::Shadow {
            git_dir,
            baseline,
            state_dir,
        }) => Ok(GitSyncReceipt::Shadow {
            git_dir,
            baseline: baseline
                .parse::<git2::Oid>()
                .context("invalid saved file sync baseline")?,
            state_dir,
        }),
        None => Ok(GitSyncReceipt::Missing),
    }
}

pub async fn doctor(runtime_flag: Option<RuntimeArg>, as_json: bool) -> Result<i32> {
    let (config, anthropic_auth) = config::load_with_auth_status(runtime_flag)?;
    let provider_statuses = tool_provider_statuses(&config.defaults);
    let vpn_status = vpn_status_for(config.network.wireguard_conf.as_deref());
    let posture = egress_posture(&config.network);
    let caps = Caps::stderr();
    let runtime = match capscode_runtime::detect(config.runtime.clone()).await {
        Ok(runtime) => runtime,
        Err(error) => {
            return report_doctor_runtime_failure(
                &error.to_string(),
                &anthropic_auth,
                &provider_statuses,
                &vpn_status,
                posture,
                as_json,
                caps,
            );
        }
    };
    // DECISION: treat a failed preflight as an unavailable runtime because required doctor checks cannot run.
    let info = match runtime.preflight().await {
        Ok(info) => info,
        Err(error) => {
            return report_doctor_runtime_failure(
                &error.to_string(),
                &anthropic_auth,
                &provider_statuses,
                &vpn_status,
                posture,
                as_json,
                caps,
            );
        }
    };
    let gateway_image = config::gateway_image(&config)?;
    let expects_tunnel = config.network.wireguard_conf.is_some() && config.network.enforce_vpn;

    let (gateway_error, report) = match runtime.image_present(&gateway_image).await {
        Ok(false) => (
            Some(format!("required image {gateway_image} is not present")),
            None,
        ),
        Ok(true) => doctor_gateway(runtime.as_ref(), &config, gateway_image).await,
        Err(error) => (
            Some(format!(
                "could not check required image {gateway_image}: {error}"
            )),
            None,
        ),
    };
    let containment_ok = report
        .as_ref()
        .map(|report| capscode_net::verify_passed(report, expects_tunnel));
    let checks = full_doctor_checks(
        gateway_error.as_deref(),
        containment_ok,
        &provider_statuses,
        anthropic_auth.kind().is_some(),
        matches!(&vpn_status, VpnStatus::Ready { .. }),
    );
    let (orphaned_cache_volumes, cache_inventory_error) = match runtime.list_owned().await {
        Ok(objects) => (crate::boxes::orphaned_cache_volumes(&objects), None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };

    if as_json {
        data((serde_json::to_string_pretty(&json!({
                "runtime": {
                    "ok": true,
                    "kind": runtime_kind_label(info.kind),
                    "version": info.version,
                    "socket": info.socket_path,
                },
                "gateway": doctor_gateway_json(
                    gateway_error.is_none(),
                    gateway_error.as_deref(),
                    containment_ok,
                ),
                "anthropic_credential": anthropic_auth_json(&anthropic_auth),
                "provider_keystore": config::provider_keystore_status_line(),
                "providers": tool_provider_status_json(&provider_statuses),
                "vpn": vpn_status_json(&vpn_status),
                "egress_posture": posture.mode,
                "egress_posture_detail": posture.detail,
                "vpn_egress_protected": report.as_ref().map(|value| value.egress_via_wireguard_only),
                "dns_leak_protected": report.as_ref().map(|value| value.resolver_in_tunnel),
                "kill_switch": containment_ok,
                "rfc1918_blocked": report.as_ref().map(|value| value.rfc1918_blocked),
                "host_gateway_blocked": report.as_ref().map(|value| value.host_gateway_blocked),
                // Frozen bool contract: true unless the IPv6 probe was Reachable.
                "ipv6_contained": report.as_ref().map(|value| {
                    !matches!(value.ipv6_contained, capscode_core::ProbeOutcome::Reachable)
                }),
                "host_gateway": report.as_ref().map(|value| {
                    value
                        .host_gateway
                        .iter()
                        .map(|(name, outcome)| {
                            json!({
                                "name": name,
                                "outcome": probe_outcome_json(outcome),
                            })
                        })
                        .collect::<Vec<_>>()
                }),
                "public_probe": report.as_ref().and_then(|value| {
                    value.public_probe.as_ref().map(probe_outcome_json)
                }),
                "egress_dev": report.as_ref().map(|value| &value.egress_dev),
                "egress_ip": report.as_ref().and_then(|value| value.egress_ip.clone()),
                "egress_loc": report.as_ref().and_then(|value| value.egress_loc.clone()),
                "details": report.as_ref().map(|value| &value.details),
                "orphaned_cache_volumes": orphaned_cache_volumes,
                "cache_inventory_error": cache_inventory_error,
            }))?).to_string());
    } else {
        let keys = config::provider_keystore_status();
        let gateway = match &gateway_error {
            Some(error) => DoctorGatewayView::Fail {
                error: error.clone(),
            },
            None => DoctorGatewayView::Pass,
        };
        print_doctor_report(
            DoctorReportView {
                runtime: DoctorRuntimeView::Pass {
                    kind: runtime_kind_label(info.kind).to_owned(),
                    version: info.version.clone(),
                    socket: info
                        .socket_path
                        .clone()
                        .unwrap_or_else(|| "default socket".to_string()),
                },
                gateway,
                anthropic: &anthropic_auth,
                providers: &provider_statuses,
                keys: &keys,
                vpn: &vpn_status,
                posture,
                report: report.as_ref(),
                expects_tunnel,
            },
            caps,
        );
        if let Some(error) = cache_inventory_error {
            note(format!("warning: list cache volumes: {error}"));
        }
        for volume in orphaned_cache_volumes {
            note(orphaned_cache_volume_line(&volume));
        }
    }
    doctor_status_result(&checks)
}

fn orphaned_cache_volume_line(name: &str) -> String {
    format!("orphaned cache volume {name}; remove with `docker volume rm {name}`")
}

fn doctor_gateway_json(
    gateway_ran: bool,
    gateway_error: Option<&str>,
    containment_ok: Option<bool>,
) -> serde_json::Value {
    json!({
        "ok": gateway_ran,
        "error": gateway_error,
        "containment_ok": containment_ok,
    })
}

fn report_doctor_runtime_failure(
    error: &str,
    anthropic_auth: &config::AnthropicAuthStatus,
    provider_statuses: &[ToolProviderStatus],
    vpn_status: &VpnStatus,
    posture: EgressPosture,
    as_json: bool,
    caps: Caps,
) -> Result<i32> {
    let checks = runtime_failure_doctor_checks(error);
    if as_json {
        data(
            (serde_json::to_string_pretty(&json!({
                "runtime": { "ok": false, "error": error },
                "gateway": doctor_gateway_json(false, Some("runtime unavailable"), None),
                "anthropic_credential": anthropic_auth_json(anthropic_auth),
                "provider_keystore": config::provider_keystore_status_line(),
                "providers": tool_provider_status_json(provider_statuses),
                "vpn": vpn_status_json(vpn_status),
                "egress_posture": posture.mode,
                "egress_posture_detail": posture.detail,
                "dns_leak_protected": null,
                "kill_switch": null
            }))?)
            .to_string(),
        );
    } else {
        let keys = config::provider_keystore_status();
        print_doctor_report(
            DoctorReportView {
                runtime: DoctorRuntimeView::Fail {
                    error: error.to_owned(),
                },
                gateway: DoctorGatewayView::Skipped,
                anthropic: anthropic_auth,
                providers: provider_statuses,
                keys: &keys,
                vpn: vpn_status,
                posture,
                report: None,
                expects_tunnel: false,
            },
            caps,
        );
    }
    doctor_status_result(&checks)
}

#[derive(Debug)]
struct ToolProviderStatus {
    tool: &'static str,
    provider: Option<String>,
    configured: bool,
    error: Option<String>,
}

fn tool_provider_statuses(defaults: &ProviderDefaults) -> Vec<ToolProviderStatus> {
    Tool::ALL
        .into_iter()
        .map(
            |tool| match config::resolve_provider(None, tool, defaults) {
                Ok(provider) => {
                    let configured = provider.credential_available();
                    ToolProviderStatus {
                        tool: tool.name(),
                        provider: Some(provider.spec.id),
                        configured,
                        error: None,
                    }
                }
                Err(error) => ToolProviderStatus {
                    tool: tool.name(),
                    provider: None,
                    configured: false,
                    error: Some(error.to_string()),
                },
            },
        )
        .collect()
}

/// Human-facing doctor layout: Runtime / Providers & Credentials / Network / Containment.
enum DoctorRuntimeView {
    Pass {
        kind: String,
        version: String,
        socket: String,
    },
    Fail {
        error: String,
    },
}

enum DoctorGatewayView {
    Pass,
    Fail { error: String },
    Skipped,
}

struct DoctorReportView<'a> {
    runtime: DoctorRuntimeView,
    gateway: DoctorGatewayView,
    anthropic: &'a config::AnthropicAuthStatus,
    providers: &'a [ToolProviderStatus],
    keys: &'a config::ProviderKeystoreStatus,
    vpn: &'a VpnStatus,
    posture: EgressPosture,
    report: Option<&'a capscode_core::NetVerifyReport>,
    expects_tunnel: bool,
}

fn print_doctor_report(view: DoctorReportView<'_>, caps: Caps) {
    for line in format_doctor_lines(&view, caps) {
        note(line);
    }
}

fn status_kv(label: &str, value: &str, status: Status, caps: Caps) -> String {
    let line = format!("{} {}", glyph(status, caps), kv(label, value, None, caps));
    styled(status, &crate::redact::redact(&line), caps)
}

/// Pure formatter for doctor human output (testable with `Caps::PLAIN`).
fn format_doctor_lines(view: &DoctorReportView<'_>, caps: Caps) -> Vec<String> {
    let mut lines = Vec::new();

    lines.push(styled(Status::Dim, &rule(Some("Runtime"), caps), caps));
    match &view.runtime {
        DoctorRuntimeView::Pass {
            kind,
            version,
            socket,
        } => {
            lines.push(status_kv(
                "runtime",
                &format!("PASS ({kind} {version}, {socket})"),
                Status::Ok,
                caps,
            ));
        }
        DoctorRuntimeView::Fail { error } => {
            lines.push(status_kv(
                "runtime",
                &format!("FAIL ({error})"),
                Status::Fail,
                caps,
            ));
        }
    }
    match &view.gateway {
        DoctorGatewayView::Pass => {
            lines.push(status_kv(
                "gateway",
                "PASS (throwaway gateway started and probed)",
                Status::Ok,
                caps,
            ));
        }
        DoctorGatewayView::Fail { error } => {
            lines.push(status_kv(
                "gateway",
                &format!("FAIL ({error})"),
                Status::Fail,
                caps,
            ));
            if error.contains("is not present") {
                lines.push(format!(
                    "  {}",
                    styled(
                        Status::Dim,
                        &format!(
                            "Build missing images: {}",
                            crate::build_context::image_build_hint()
                        ),
                        caps
                    )
                ));
            }
        }
        DoctorGatewayView::Skipped => {
            lines.push(status_kv("gateway", "SKIPPED", Status::Dim, caps));
        }
    }

    lines.push(styled(
        Status::Dim,
        &rule(Some("Providers & Credentials"), caps),
        caps,
    ));
    let (anth_status, anth_value, anth_fix) = format_anthropic_doctor(view.anthropic);
    lines.push(status_kv("anthropic", &anth_value, anth_status, caps));
    if let Some(fix) = anth_fix {
        lines.push(format!("  {}", styled(Status::Dim, fix, caps)));
    }
    let (keys_status, keys_value, keys_fix) = format_provider_keys_doctor(view.keys);
    lines.push(status_kv("keys", &keys_value, keys_status, caps));
    if let Some(fix) = keys_fix {
        lines.push(format!("  {}", styled(Status::Dim, fix, caps)));
    }
    for line in format_tool_provider_kv_lines(view.providers, caps) {
        lines.push(line);
    }

    lines.push(styled(Status::Dim, &rule(Some("Network"), caps), caps));
    let (vpn_status, vpn_value, vpn_fix) = format_vpn_doctor(view.vpn);
    lines.push(status_kv("vpn", &vpn_value, vpn_status, caps));
    if let Some(fix) = vpn_fix {
        lines.push(format!("  {}", styled(Status::Dim, fix, caps)));
    }
    let posture_line = view.posture.line();
    let posture_value = posture_line
        .strip_prefix("Egress posture: ")
        .unwrap_or(&posture_line);
    lines.push(status_kv("posture", posture_value, Status::Dim, caps));
    if let Some(report) = view.report {
        lines.push(status_kv(
            "egress",
            &format_egress_value(report),
            Status::Dim,
            caps,
        ));
    }

    lines.push(styled(Status::Dim, &rule(Some("Containment"), caps), caps));
    match view.report {
        Some(report) => {
            if view.expects_tunnel {
                let (st, label) = check_status(report.resolver_in_tunnel);
                lines.push(status_kv("dns-leak", label, st, caps));
            } else {
                lines.push(status_kv(
                    "dns-leak",
                    "N/A (no tunnel resolver configured)",
                    Status::Dim,
                    caps,
                ));
            }
            let (st, label) =
                check_status(capscode_net::verify_passed(report, view.expects_tunnel));
            lines.push(status_kv("kill-switch", label, st, caps));
            let (st, label) = check_status(report.rfc1918_blocked);
            lines.push(status_kv("rfc1918", label, st, caps));
            let (st, label) = check_status(report.host_gateway_blocked);
            lines.push(status_kv("host-gw", label, st, caps));
            for detail in &report.details {
                lines.push(format!("  {}", styled(Status::Dim, detail, caps)));
            }
        }
        None => {
            lines.push(status_kv("dns-leak", "UNKNOWN", Status::Dim, caps));
            lines.push(status_kv("kill-switch", "UNKNOWN", Status::Dim, caps));
        }
    }

    lines
}

fn check_status(ok: bool) -> (Status, &'static str) {
    if ok {
        (Status::Ok, "PASS")
    } else {
        (Status::Fail, "FAIL")
    }
}

fn format_anthropic_doctor(
    auth: &config::AnthropicAuthStatus,
) -> (Status, String, Option<&'static str>) {
    match auth.kind() {
        Some(kind) => (
            Status::Ok,
            format!("{} ({})", auth.source(), config::credential_kind_name(kind)),
            None,
        ),
        None => {
            let line = auth.doctor_line();
            let value = line
                .strip_prefix("Anthropic credential: ")
                .unwrap_or(&line)
                .to_string();
            let status = if value.contains("keychain lookup disabled") {
                Status::Warn
            } else {
                Status::Fail
            };
            (status, value, Some("capscode key set anthropic"))
        }
    }
}

fn format_provider_keys_doctor(
    status: &config::ProviderKeystoreStatus,
) -> (Status, String, Option<&'static str>) {
    match status {
        config::ProviderKeystoreStatus::Disabled => (
            Status::Warn,
            "[auth] keychain lookup disabled".to_string(),
            Some("capscode key set <id>"),
        ),
        config::ProviderKeystoreStatus::Empty => {
            (Status::Dim, "no keychain bundle".to_string(), None)
        }
        config::ProviderKeystoreStatus::Loaded(ids) => (
            Status::Ok,
            format!("keychain bundle loaded ({} key(s))", ids.len()),
            None,
        ),
        config::ProviderKeystoreStatus::Problem(error) => {
            let line = config::provider_keystore_status_line();
            let value = line
                .strip_prefix("Provider keys: ")
                .unwrap_or(&line)
                .to_string();
            let fix = if error.contains("corrupt") || error.contains("too new") {
                Some("capscode key reset")
            } else {
                None
            };
            (Status::Fail, value, fix)
        }
    }
}

fn format_tool_provider_kv_lines(statuses: &[ToolProviderStatus], caps: Caps) -> Vec<String> {
    let mut lines = Vec::with_capacity(statuses.len() * 2);
    for status in statuses {
        match (&status.provider, &status.error) {
            (Some(provider), _) => {
                let (st, detail) = if status.configured {
                    (Status::Ok, "credential configured")
                } else {
                    (Status::Fail, "credential missing")
                };
                lines.push(status_kv(
                    status.tool,
                    &format!("{provider} ({detail})"),
                    st,
                    caps,
                ));
                if !status.configured {
                    lines.push(format!(
                        "  {}",
                        styled(Status::Dim, &format!("capscode key set {provider}"), caps)
                    ));
                }
            }
            (None, Some(error)) => {
                lines.push(status_kv(
                    status.tool,
                    &format!("unset ({error})"),
                    Status::Fail,
                    caps,
                ));
            }
            (None, None) => {
                lines.push(status_kv(status.tool, "unset", Status::Dim, caps));
            }
        }
    }
    lines
}

fn format_vpn_doctor(status: &VpnStatus) -> (Status, String, Option<&'static str>) {
    match status {
        VpnStatus::NotConfigured => (
            Status::Dim,
            "not configured (containment-only)".to_string(),
            Some("capscode vpn import <provider>.conf"),
        ),
        VpnStatus::Ready { endpoint, .. } => {
            (Status::Ok, format!("ready (endpoint {endpoint})"), None)
        }
        VpnStatus::Misconfigured { reason, .. } => {
            (Status::Fail, format!("misconfigured ({reason})"), None)
        }
    }
}

fn format_egress_value(report: &capscode_core::NetVerifyReport) -> String {
    match (&report.egress_ip, &report.egress_loc) {
        (Some(ip), Some(loc)) => format!("{} — {ip} ({loc})", report.egress_dev),
        (Some(ip), None) => format!("{} — {ip}", report.egress_dev),
        _ => format!("{} — public IP unavailable", report.egress_dev),
    }
}

fn tool_provider_status_json(statuses: &[ToolProviderStatus]) -> serde_json::Value {
    let values: serde_json::Map<String, serde_json::Value> = statuses
        .iter()
        .map(|status| {
            (
                status.tool.to_string(),
                json!({
                    "provider": status.provider,
                    "credential_configured": status.configured,
                    "error": status.error,
                }),
            )
        })
        .collect();
    serde_json::Value::Object(values)
}

fn anthropic_auth_json(status: &config::AnthropicAuthStatus) -> serde_json::Value {
    json!({
        "configured": status.kind().is_some(),
        "source": status.source(),
        "kind": status.kind().map(config::credential_kind_name),
    })
}

pub(crate) fn init_vpn_status_line(runtime_flag: Option<RuntimeArg>) -> Result<String> {
    let config = config::load(runtime_flag)?;
    Ok(
        match vpn_status_for(config.network.wireguard_conf.as_deref()) {
            VpnStatus::NotConfigured => "VPN: not configured (containment-only)".to_string(),
            VpnStatus::Ready { endpoint, .. } => format!("VPN: ready (endpoint {endpoint})"),
            VpnStatus::Misconfigured { reason, .. } => format!("VPN: misconfigured ({reason})"),
        },
    )
}

fn vpn_status_json(status: &VpnStatus) -> serde_json::Value {
    match status {
        VpnStatus::NotConfigured => json!({
            "configured": false,
            "valid": null,
            "path": null,
            "endpoint": null,
            "error": null,
        }),
        VpnStatus::Ready { path, endpoint } => json!({
            "configured": true,
            "valid": true,
            "path": path,
            "endpoint": endpoint,
            "error": null,
        }),
        VpnStatus::Misconfigured { path, reason } => json!({
            "configured": true,
            "valid": false,
            "path": path,
            "endpoint": null,
            "error": reason,
        }),
    }
}

fn status_json_payload(
    runtime: &DashboardRuntime,
    anthropic: &DashboardCredential,
    codex: &DashboardCredential,
    defaults: &ProviderDefaults,
    provider_keys: &config::ProviderKeystoreStatus,
    vpn: &VpnStatus,
    posture: EgressPosture,
) -> serde_json::Value {
    json!({
        "runtime": {
            "engine": runtime.engine,
            "error": runtime.error,
        },
        "anthropic": dashboard_credential_json(anthropic),
        "codex": dashboard_credential_json(codex),
        "provider_defaults": provider_defaults_json(defaults),
        "provider_keys": provider_keystore_json(provider_keys),
        "vpn": dashboard_vpn_json(vpn, posture.mode == "tunnel"),
        "egress_posture": posture.mode,
        "egress_posture_detail": posture.detail,
    })
}

fn dashboard_credential_json(credential: &DashboardCredential) -> serde_json::Value {
    json!({
        "source": credential.source,
        "kind": credential.kind,
        "configured": credential.source != "none",
    })
}

fn provider_defaults_json(defaults: &ProviderDefaults) -> serde_json::Value {
    let values: serde_json::Map<String, serde_json::Value> = Tool::ALL
        .into_iter()
        .map(|tool| (tool.name().to_string(), json!(defaults.get(tool))))
        .collect();
    serde_json::Value::Object(values)
}

fn provider_keystore_json(status: &config::ProviderKeystoreStatus) -> serde_json::Value {
    match status {
        config::ProviderKeystoreStatus::Disabled => json!({
            "status": "disabled",
            "ids": [],
            "message": config::provider_keystore_status_line(),
        }),
        config::ProviderKeystoreStatus::Empty => json!({
            "status": "empty",
            "ids": [],
            "message": config::provider_keystore_status_line(),
        }),
        config::ProviderKeystoreStatus::Loaded(ids) => json!({
            "status": "loaded",
            "ids": ids,
            "message": config::provider_keystore_status_line(),
        }),
        config::ProviderKeystoreStatus::Problem(error) => json!({
            "status": "error",
            "ids": [],
            "error": error,
            "message": config::provider_keystore_status_line(),
        }),
    }
}

fn dashboard_vpn_json(status: &VpnStatus, enforced: bool) -> serde_json::Value {
    match status {
        VpnStatus::NotConfigured => json!({
            "path": null,
            "validated": false,
            "enforced": false,
            "error": null,
        }),
        VpnStatus::Ready { path, .. } => json!({
            "path": path,
            "validated": true,
            "enforced": enforced,
            "error": null,
        }),
        VpnStatus::Misconfigured { path, reason } => json!({
            "path": path,
            "validated": false,
            "enforced": enforced,
            "error": reason,
        }),
    }
}

struct StatusDashboardView<'a> {
    runtime: &'a DashboardRuntime,
    anthropic: &'a DashboardCredential,
    codex: &'a DashboardCredential,
    defaults: &'a ProviderDefaults,
    provider_keys: &'a config::ProviderKeystoreStatus,
    vpn: &'a VpnStatus,
    posture: EgressPosture,
}

fn print_status_dashboard(view: StatusDashboardView<'_>, caps: Caps) {
    for line in format_status_lines(&view, caps) {
        note(line);
    }
}

/// Pure formatter for status human output (testable with `Caps::PLAIN`).
fn format_status_lines(view: &StatusDashboardView<'_>, caps: Caps) -> Vec<String> {
    let mut lines = Vec::new();

    lines.push(styled(Status::Dim, &rule(Some("Runtime"), caps), caps));
    match (&view.runtime.engine, &view.runtime.error) {
        (Some(engine), _) => {
            lines.push(status_kv("runtime", engine, Status::Ok, caps));
        }
        (None, Some(error)) => {
            lines.push(status_kv(
                "runtime",
                &format!("unavailable ({error})"),
                Status::Fail,
                caps,
            ));
        }
        (None, None) => {
            lines.push(status_kv("runtime", "unavailable", Status::Fail, caps));
        }
    }

    lines.push(styled(
        Status::Dim,
        &rule(Some("Providers & Credentials"), caps),
        caps,
    ));
    let (st, value) = format_dashboard_credential(view.anthropic);
    lines.push(status_kv("anthropic", &value, st, caps));
    let (st, value) = format_dashboard_credential(view.codex);
    lines.push(status_kv("codex", &value, st, caps));
    for tool in Tool::ALL {
        lines.push(status_kv(
            tool.name(),
            view.defaults.get(tool).unwrap_or("auto"),
            Status::Dim,
            caps,
        ));
    }
    let (keys_status, keys_value) = format_provider_keys_status(view.provider_keys);
    lines.push(status_kv("keys", &keys_value, keys_status, caps));

    lines.push(styled(Status::Dim, &rule(Some("Network"), caps), caps));
    let (vpn_status, vpn_value) =
        format_vpn_status_dashboard(view.vpn, view.posture.mode == "tunnel");
    lines.push(status_kv("vpn", &vpn_value, vpn_status, caps));
    let posture_line = view.posture.line();
    let posture_value = posture_line
        .strip_prefix("Egress posture: ")
        .unwrap_or(&posture_line);
    lines.push(status_kv("posture", posture_value, Status::Dim, caps));

    lines
}

fn format_dashboard_credential(credential: &DashboardCredential) -> (Status, String) {
    match credential.kind.as_deref() {
        Some(kind) => (Status::Ok, format!("{} ({kind})", credential.source)),
        None if credential.source == "none" => (Status::Dim, "none".to_string()),
        None => (Status::Warn, credential.source.clone()),
    }
}

fn format_provider_keys_status(status: &config::ProviderKeystoreStatus) -> (Status, String) {
    match status {
        config::ProviderKeystoreStatus::Loaded(ids) if !ids.is_empty() => {
            (Status::Ok, format!("stored ids: {}", ids.join(", ")))
        }
        config::ProviderKeystoreStatus::Loaded(_) => {
            (Status::Dim, "no stored provider key ids".to_string())
        }
        config::ProviderKeystoreStatus::Disabled => {
            let line = config::provider_keystore_status_line();
            (
                Status::Warn,
                line.strip_prefix("Provider keys: ")
                    .unwrap_or(&line)
                    .to_string(),
            )
        }
        config::ProviderKeystoreStatus::Empty => (Status::Dim, "no keychain bundle".to_string()),
        config::ProviderKeystoreStatus::Problem(_) => {
            let line = config::provider_keystore_status_line();
            (
                Status::Fail,
                line.strip_prefix("Provider keys: ")
                    .unwrap_or(&line)
                    .to_string(),
            )
        }
    }
}

fn format_vpn_status_dashboard(status: &VpnStatus, enforced: bool) -> (Status, String) {
    let enforced_word = if enforced { "yes" } else { "no" };
    match status {
        VpnStatus::NotConfigured => (
            Status::Dim,
            "path: none; validated: no; enforced: no".to_string(),
        ),
        VpnStatus::Ready { path, .. } => (
            Status::Ok,
            format!(
                "path: {}; validated: yes; enforced: {enforced_word}",
                path.display()
            ),
        ),
        VpnStatus::Misconfigured { path, reason } => (
            Status::Fail,
            format!(
                "path: {}; validated: no; enforced: {enforced_word}; error: {reason}",
                path.display()
            ),
        ),
    }
}

async fn doctor_gateway(
    runtime: &dyn RuntimeAdapter,
    config: &capscode_core::Config,
    gateway_image: capscode_core::ImageRef,
) -> (Option<String>, Option<capscode_core::NetVerifyReport>) {
    let ca = match SessionCa::generate() {
        Ok(ca) => ca,
        Err(error) => return (Some(error.to_string()), None),
    };
    let session = SessionId::new();
    let stack = NetStack::new(config.network.clone());
    let doctor_credential = ProviderCredential::api_key(SecretString::from(
        "sk-capscode-doctor-sentinel".to_string(),
    ));
    let mut diagnostic_keys = config.broker.keys.clone();
    if diagnostic_keys.0.is_empty() {
        diagnostic_keys
            .0
            .insert(Provider::Anthropic, doctor_credential.clone());
    }
    let doctor_provider = match capscode_core::load_bundled_provider_registry() {
        Ok(registry) => match registry
            .into_iter()
            .find(|provider| provider.id == "anthropic")
        {
            Some(provider) => provider,
            None => {
                return (
                    Some("bundled Anthropic provider is missing".to_string()),
                    None,
                )
            }
        },
        Err(error) => return (Some(error.to_string()), None),
    };
    let context = NetContext {
        session,
        network: config.network.clone(),
        broker: config.broker.clone(),
        keys: diagnostic_keys,
        ca_cert_pem: ca.ca_cert_pem().to_string(),
        gateway_image,
    };
    let mut spec = match stack.gateway_spec(&context) {
        Ok(spec) => spec,
        Err(error) => return (Some(error.to_string()), None),
    };
    let manifest = match provider_manifest_json(&doctor_provider, Some(&doctor_credential)) {
        Ok(manifest) => manifest,
        Err(error) => return (Some(error.to_string()), None),
    };
    let aliases = match broker_aliases(&doctor_provider, Some(&doctor_credential)) {
        Ok(aliases) => aliases,
        Err(error) => return (Some(error.to_string()), None),
    };
    set_container_env(&mut spec, "CAPSCODE_PROVIDERS", manifest);
    set_container_env(&mut spec, "CAPSCODE_BROKER_ALIASES", aliases);
    let gateway = match runtime.create_container(&spec).await {
        Ok(gateway) => gateway,
        Err(error) => return (Some(error.to_string()), None),
    };
    let result = async {
        runtime.start(&gateway).await?;
        inject_gateway_secrets(
            runtime,
            &gateway,
            &config.network,
            &doctor_provider,
            Some(&doctor_credential),
            None,
            &[],
        )
        .await
        .map_err(|error| {
            capscode_core::RuntimeError::Container("doctor gateway".into(), error.to_string())
        })?;
        read_gateway_ca(runtime, &gateway).await.map_err(|error| {
            capscode_core::RuntimeError::Container("doctor gateway".into(), error.to_string())
        })?;
        stack.verify(&gateway, runtime).await.map_err(|error| {
            capscode_core::RuntimeError::Container("doctor gateway".into(), error.to_string())
        })
    }
    .await;
    let _ = runtime.stop(&gateway, Duration::from_secs(2)).await;
    let _ = runtime.remove(&gateway, true).await;
    match result {
        Ok(report) => (None, Some(report)),
        Err(error) => (Some(error.to_string()), None),
    }
}

fn signal_receiver() -> Result<watch::Receiver<Option<crate::session::state::SessionSignal>>> {
    use crate::session::state::SessionSignal;

    let (sender, receiver) = watch::channel(None);
    #[cfg(unix)]
    {
        // Register the shutdown signals synchronously so ownership is established before
        // this function returns (and before set_session_signal_owner(true)).
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("installing SIGINT handler")?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("installing SIGTERM handler")?;
        let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .context("installing SIGHUP handler")?;
        let mut quit = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit())
            .context("installing SIGQUIT handler")?;
        tokio::spawn(async move {
            let signal = tokio::select! {
                _ = interrupt.recv() => SessionSignal::Interrupt,
                _ = terminate.recv() => SessionSignal::Terminate,
                _ = hangup.recv() => SessionSignal::Hangup,
                _ = quit.recv() => SessionSignal::Quit,
            };
            if signal == SessionSignal::Hangup {
                crate::terminal::mark_hangup_received();
            }
            let _ = sender.send(Some(signal));
        });
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = sender.send(Some(SessionSignal::Interrupt));
    });
    Ok(receiver)
}

fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn probe_outcome_json(outcome: &capscode_core::ProbeOutcome) -> serde_json::Value {
    match outcome {
        capscode_core::ProbeOutcome::Dropped => json!("dropped"),
        capscode_core::ProbeOutcome::Reachable => json!("reachable"),
        capscode_core::ProbeOutcome::NotTested(reason) => json!({
            "status": "not_tested",
            "reason": reason,
        }),
    }
}

#[cfg(test)]
#[path = "commands_tests.rs"]
mod tests;
