use std::collections::{BTreeMap, HashMap};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use capscode_core::{
    AttachIo, Config, ContainerId, ExecSpec, GitEngine, ImageRef, IngressPlan, NetworkStack,
    OwnedObject, OwnedObjectKind, ProviderCredential, ProviderSpec, RuntimeAdapter, SessionId,
    Tool, VolumeId, VolumeSpec, LABEL_BOX, LABEL_BOX_ID, LABEL_CREATED, LABEL_OWNER,
    LABEL_OWNER_VALUE, LABEL_PROJECT, LABEL_PROJECT_HASH, LABEL_PROVIDERS, LABEL_ROLE,
    LABEL_SESSION, LABEL_TOOLCHAIN, LABEL_WORKSPACE_MODE, ROLE_CACHE, ROLE_CAPSULE, ROLE_GATEWAY,
};
use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect};
use sha2::{Digest, Sha256};

use crate::config;
use crate::session::state::{self, BoxSpec, SessionOptions};
use crate::terminal::TerminalGuard;
use crate::ui::note;

use super::{
    cache_volume_matches_box, cache_volumes_for_box, index, name, BoxRef, BoxRow, CreateOptions,
    ResumeOptions,
};

type RoutedProvider = (ProviderSpec, ProviderCredential);

struct CacheVolumePlan {
    enabled: bool,
    remove: Vec<VolumeId>,
    create: bool,
}

pub(crate) fn for_project(project: &Path) -> Result<Vec<BoxRef>> {
    let project = canonical_project(project)?;
    let metadata = std::fs::metadata(&project).ok();
    Ok(index::read()?
        .into_iter()
        .filter(|entry| {
            entry.project == project
                || metadata.as_ref().is_some_and(|meta| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        entry.dev == Some(meta.dev()) && entry.ino == Some(meta.ino())
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = meta;
                        false
                    }
                })
        })
        .map(|entry| BoxRef {
            name: entry.name,
            id: entry.id,
            session: entry.session,
        })
        .collect())
}

pub(crate) async fn create(
    project: &Path,
    options: CreateOptions,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Result<BoxRef> {
    let project = canonical_project(project)?;
    let _ = ls_rows(Arc::clone(&runtime)).await?;
    let existing = for_project(&project)?;
    if should_refuse_existing_box(!existing.is_empty(), options.base.is_some()) {
        let existing = &existing[0];
        bail!(
            "this project already has box '{}'; enter it with `capscode {}`",
            existing.name,
            existing.name
        );
    }
    let base_input = options
        .base
        .as_deref()
        .or_else(|| project.file_name().and_then(|name| name.to_str()))
        .unwrap_or("project");
    let base = name::sanitise_base(base_input)?;
    let candidates = config::box_candidates()?;
    if candidates.is_empty() {
        bail!("no stored keys; run `capscode key set <id>`");
    }
    let defaults = config::tool_defaults()?;
    let routed = select_providers(&candidates, &defaults, options.providers.as_deref())?;
    let session = SessionId::new();
    let id = name::fresh_id(index::read()?.into_iter().map(|entry| entry.id));
    let box_name = format!("{base}-{id}");
    let mut entry = index_entry(&box_name, &id, &session, &project, &routed);
    entry.created = index::now();
    entry.no_cache = options.no_cache;
    let (mut config, gateway_image, capsule_image, seeder_image, current_toolchain, lock) =
        provision_config(None)?;
    let cache_enabled = config.capsule.cache_volume && !options.no_cache;
    entry.toolchain = current_toolchain.clone();
    entry.toolchain_pinned = options.pin;
    let workspace_mode = if options.mount {
        capscode_core::WorkspaceMode::Mount
    } else {
        capscode_core::WorkspaceMode::Copy
    };
    config.workspace.mode = workspace_mode;
    config.git.mount = workspace_mode == capscode_core::WorkspaceMode::Mount;
    entry.mode = workspace_mode.as_str().to_owned();
    print_creation(&routed, &defaults);
    let (selected, credential, extras) = split_credentials(routed)?;
    let mut box_spec = make_box_spec(
        &box_name,
        &id,
        &project,
        &selected,
        &extras,
        &defaults,
        &current_toolchain,
    );
    box_spec.cache_volume = cache_enabled;
    let git: Arc<dyn GitEngine> = Arc::new(capscode_git::DefaultGitEngine::new());
    let options = SessionOptions {
        session: session.clone(),
        config,
        host_repo: project.clone(),
        tool_command: "bash".into(),
        tool_args: Vec::new(),
        gateway_image,
        capsule_image: capsule_image.clone(),
        seeder_image,
        selected_provider: selected,
        selected_credential: Some(credential),
        selected_credential_source: Some("stored credential".into()),
        extra_providers: extras,
        interactive_handoff: false,
        verbose: false,
        r#box: Some(box_spec.clone()),
    };
    let volume = if workspace_mode == capscode_core::WorkspaceMode::Copy {
        let volume = VolumeId(format!("capscode-{}-workspace", session));
        let mut volume_labels = vec![
            (LABEL_OWNER.to_owned(), LABEL_OWNER_VALUE.to_owned()),
            (LABEL_SESSION.to_owned(), session.to_string()),
            (LABEL_ROLE.to_owned(), ROLE_CAPSULE.to_owned()),
            (LABEL_CREATED.to_owned(), index::now()),
            (LABEL_BOX.to_owned(), box_name.clone()),
            (LABEL_BOX_ID.to_owned(), id.clone()),
            (
                LABEL_PROJECT.to_owned(),
                project.to_string_lossy().into_owned(),
            ),
            (LABEL_PROJECT_HASH.to_owned(), box_spec.project_hash.clone()),
            (LABEL_PROVIDERS.to_owned(), box_spec.routed.join(",")),
            (
                LABEL_WORKSPACE_MODE.to_owned(),
                workspace_mode.as_str().to_owned(),
            ),
        ];
        volume_labels.sort_by(|left, right| left.0.cmp(&right.0));
        runtime
            .create_volume(&VolumeSpec {
                name: volume.0.clone(),
                labels: volume_labels,
            })
            .await
            .context("creating box workspace volume")?;
        let state_dir = crate::session::state::session_state_dir(&session);
        prepare_session_state_dir(&state_dir)?;
        let mut progress = |event: capscode_core::IngressProgress| {
            note(format!("Preparing workspace: {}", event.phase));
        };
        git.ingress(
            &IngressPlan {
                host_repo: project.clone(),
                session: session.clone(),
                state_dir,
                seeder_image: options.seeder_image.clone(),
                history: options.config.git.history,
                include: options.config.git.include.clone(),
                mount: false,
                verbose: options.verbose,
            },
            runtime.as_ref(),
            &mut progress,
        )
        .await
        .context("copying project into box workspace")?;
        Some(volume)
    } else {
        None
    };
    if cache_enabled {
        create_box_cache_volume(runtime.as_ref(), &options).await?;
    }
    let network: Arc<dyn NetworkStack> =
        Arc::new(capscode_net::NetStack::new(options.config.network.clone()));
    let mut driver =
        crate::session::state::SessionDriver::new(options, Arc::clone(&runtime), git, network)?;
    if let Some(volume) = volume {
        driver
            .provision_existing(volume)
            .await
            .context("provisioning box containers")?;
    } else {
        provision_until_running(&mut driver, runtime.as_ref(), &session).await?;
    }
    snapshot_if_present(runtime.as_ref(), &capsule_image, &lock, &current_toolchain).await?;
    index::upsert(entry)?;
    note(format!(
        "your work is in the box until `capscode sync {box_name}`"
    ));
    Ok(BoxRef {
        name: box_name,
        id,
        session: session.to_string(),
    })
}

fn should_refuse_existing_box(has_existing_box: bool, has_explicit_name: bool) -> bool {
    has_existing_box && !has_explicit_name
}

pub(crate) async fn enter(target: &BoxRef, runtime: Arc<dyn RuntimeAdapter>) -> Result<i32> {
    let objects = runtime.list_owned().await?;
    let capsule = capsule_for(&objects, target)?;
    if capsule.state.as_deref() != Some("running") {
        resume(target, ResumeOptions::default(), Arc::clone(&runtime)).await?;
    }
    let objects = runtime.list_owned().await?;
    let capsule = capsule_for(&objects, target)?;
    if capsule.state.as_deref() != Some("running") {
        bail!(
            "box '{}' did not start; run `capscode resume {}` and try again",
            target.name,
            target.name
        );
    }
    let entry = index::entry_for_session(&target.session)?.unwrap_or_default();
    let box_digest = box_toolchain(&entry, Some(&capsule));
    let current_config = config::load(None)?;
    let current_lock = crate::toolchain::resolve(current_config.image.toolchain_lock.as_deref())?;
    let current_digest = crate::toolchain::digest(&current_lock.lock);
    let cmd = super::shell_attach_command(&config::load(None)?.r#box.prefix)?;
    index::update_last_entered(&target.session)?;
    let code = {
        crate::ui::ensure_no_live_region()?;
        // Raw mode for the attach, so keys (Ctrl-c, the tmux prefix) reach the box unbuffered.
        let _guard = TerminalGuard::new();
        runtime
            .run_interactive(
                &ContainerId(capsule.id),
                ExecSpec {
                    cmd,
                    workdir: Some(PathBuf::from("/workspace")),
                    user: Some("agent".into()),
                    tty: true,
                    ..Default::default()
                },
                AttachIo {
                    stdin: capscode_runtime::cancellable_stdin()
                        .context("preparing terminal input")?,
                    stdout: Box::new(tokio::io::stdout()),
                    stderr: Box::new(tokio::io::stderr()),
                },
            )
            .await
            .context("entering box shell")?
    };
    if let Some(message) = running_toolchain_note(&target.name, &box_digest, &current_digest) {
        note(message);
    }
    note(format!(
        "your work is in the box until `capscode sync {}`",
        target.name
    ));
    Ok(code)
}

pub(crate) async fn resume(
    target: &BoxRef,
    options: ResumeOptions,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Result<()> {
    let objects = runtime.list_owned().await?;
    let existing_containers = box_container_members(&objects, target);
    let entry = index::entry_for_session(&target.session)?.ok_or_else(|| {
        anyhow!(
            "box '{}' has no indexed metadata; run `capscode ls` to rebuild the index",
            target.name
        )
    })?;
    if !matches!(entry.mode.as_str(), "copy" | "mount") {
        bail!("box has an unknown workspace mode; run `capscode ls` to refresh its metadata");
    }
    if entry.mode == "mount" && !entry.project.is_dir() {
        bail!("mount-mode box project path is missing: {}; restore the project folder before resuming", entry.project.display());
    }
    let was_running = existing_containers
        .iter()
        .any(|container| container.state.as_deref() == Some("running"));
    if was_running && !options.yes {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("{}", noninteractive_running_resume_error(&target.name));
        }
        if !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Resume '{}' will stop running tools and recreate its containers. Continue?",
                target.name
            ))
            .default(false)
            .interact()
            .context("reading resume confirmation")?
        {
            bail!("resume cancelled");
        }
    }
    let mut entry = entry;
    let current_config = config::load(None)?;
    let current_lock = crate::toolchain::resolve(current_config.image.toolchain_lock.as_deref())?;
    let current_digest = crate::toolchain::digest(&current_lock.lock);
    let capsule = existing_containers
        .iter()
        .find(|object| object.role.as_deref() == Some(ROLE_CAPSULE));
    let old_digest = box_toolchain(&entry, capsule);
    if old_digest.is_empty() && options.toolchain.as_deref() != Some("current") {
        bail!(
            "box '{}' has no recorded toolchain; resume it with `--toolchain current` to record the current toolchain",
            target.name
        );
    }
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let yanks = crate::update::known_yanks(&current_config, &current_lock.lock);
    let yanked =
        |digest: &str| toolchain_yanked(digest, &current_digest, &current_lock.lock, &yanks);
    let old_image_needs_rebuild = if old_digest != current_digest {
        toolchain_image_needs_rebuild(runtime.as_ref(), &old_digest).await?
    } else {
        false
    };
    let decision = resume_toolchain_decision(
        was_running,
        entry.toolchain_pinned,
        options.toolchain.as_deref(),
        &old_digest,
        &current_digest,
        interactive,
        &YankGate {
            yanked: &yanked,
            force: options.force,
        },
    )?;
    let selected_digest = match decision {
        ResumeToolchainDecision::Current => current_digest.clone(),
        ResumeToolchainDecision::Keep => old_digest.clone(),
        ResumeToolchainDecision::Digest(digest) => digest,
        ResumeToolchainDecision::Prompt => {
            let rebuild_note = if old_image_needs_rebuild {
                format!(
                    ". Toolchain {old_digest} was built for an older CapsCode and must be rebuilt: `capscode toolchain build {old_digest}`"
                )
            } else {
                String::new()
            };
            let keep = prompt_resume_toolchain_choice(&format!(
                "Box {} last ran toolchain {}. Current toolchain is {}{}",
                target.name, old_digest, current_digest, rebuild_note
            ))?;
            match keep {
                ResumeToolchainPromptChoice::Switch => current_digest.clone(),
                ResumeToolchainPromptChoice::Keep => old_digest.clone(),
                ResumeToolchainPromptChoice::Cancel => bail!("resume cancelled"),
            }
        }
    };
    validate_selected_toolchain(
        runtime.as_ref(),
        &selected_digest,
        &current_digest,
        &old_digest,
    )
    .await?;
    if selected_digest == old_digest && selected_digest != current_digest {
        if let Some(message) = running_toolchain_note(&target.name, &old_digest, &current_digest) {
            note(message);
        }
    }
    if let Some(entry) = yanked(&selected_digest) {
        note(format!(
            "warning: toolchain {} was withdrawn ({entry}); switch with `capscode resume {} --toolchain current`",
            short_digest(&selected_digest),
            target.name
        ));
    }
    let cache_enabled = resume_cache_enabled(
        current_config.capsule.cache_volume,
        entry.no_cache,
        options.cache,
    );
    if options.cache {
        entry.no_cache = false;
    }
    entry.toolchain = selected_digest.clone();
    entry.toolchain_pinned |= options.pin;
    let candidates = config::box_candidates()?;
    let defaults = config::tool_defaults()?;
    let routed = if let Some(spec) = options.providers.as_deref() {
        select_providers(&candidates, &defaults, Some(spec))?
    } else {
        let ids = entry.providers.join(",");
        select_providers(&candidates, &defaults, Some(&ids))?
    };
    let volume = if entry.mode == "mount" {
        None
    } else {
        Some(VolumeId(format!("capscode-{}-workspace", target.session)))
    };
    if let Some(volume) = volume.as_ref() {
        if !objects.iter().any(|object| {
            object.kind == OwnedObjectKind::Volume
                && object.id == volume.0
                && object
                    .session
                    .as_ref()
                    .is_some_and(|id| id.as_str() == target.session)
        }) {
            bail!(
                "box workspace volume is missing; refusing to recreate the box without its files"
            );
        }
    } else if !has_box_container_roles(&existing_containers) {
        bail!("box containers are missing; mount-mode boxes can only resume while their project is available");
    }
    let cache_plan = prepare_box_cache_volume(
        &objects,
        target,
        &selected_digest,
        cache_enabled,
        options.fresh_cache,
    );
    entry.providers = routed.iter().map(|(spec, _)| spec.id.clone()).collect();
    let refreshed_entry = entry.clone();
    let provision_runtime = Arc::clone(&runtime);
    replace_box_containers(
        runtime.as_ref(),
        &existing_containers,
        was_running,
        move || async move {
            provision_existing_box(
                target,
                entry,
                routed,
                defaults,
                volume,
                cache_plan,
                provision_runtime,
            )
            .await
        },
    )
    .await?;
    index::upsert(refreshed_entry)?;
    Ok(())
}

async fn provision_existing_box(
    target: &BoxRef,
    entry: index::Entry,
    routed: Vec<RoutedProvider>,
    defaults: BTreeMap<Tool, String>,
    volume: Option<VolumeId>,
    cache: CacheVolumePlan,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Result<()> {
    let selected_digest = entry.toolchain.clone();
    let (mut config, gateway_image, capsule_image, seeder_image, current_digest, lock) =
        provision_config(Some(&selected_digest))?;
    let (selected, credential, extras) = split_credentials(routed)?;
    let project = entry.project.clone();
    let mut spec = make_box_spec(
        &target.name,
        &target.id,
        &project,
        &selected,
        &extras,
        &defaults,
        &selected_digest,
    );
    spec.cache_volume = cache.enabled;
    config.workspace.mode = match entry.mode.as_str() {
        "copy" => capscode_core::WorkspaceMode::Copy,
        "mount" => capscode_core::WorkspaceMode::Mount,
        _ => bail!("box has an unknown workspace mode; run `capscode ls` to refresh its metadata"),
    };
    config.git.mount = config.workspace.mode == capscode_core::WorkspaceMode::Mount;
    let network: Arc<dyn NetworkStack> =
        Arc::new(capscode_net::NetStack::new(config.network.clone()));
    let git: Arc<dyn GitEngine> = Arc::new(capscode_git::DefaultGitEngine::new());
    let options = SessionOptions {
        session: SessionId::from(target.session.clone()),
        config,
        host_repo: project,
        tool_command: "bash".into(),
        tool_args: Vec::new(),
        gateway_image,
        capsule_image: capsule_image.clone(),
        seeder_image,
        selected_provider: selected,
        selected_credential: Some(credential),
        selected_credential_source: Some("stored credential".into()),
        extra_providers: extras,
        interactive_handoff: false,
        verbose: false,
        r#box: Some(spec),
    };
    // The caller has removed the old capsule first; Docker cannot remove a mounted volume.
    remove_planned_cache_volumes(runtime.as_ref(), &cache.remove).await?;
    if cache.create {
        create_box_cache_volume(runtime.as_ref(), &options).await?;
    }
    let mut driver =
        crate::session::state::SessionDriver::new(options, Arc::clone(&runtime), git, network)?;
    if let Some(volume) = volume {
        driver
            .provision_existing(volume)
            .await
            .context("recreating box around its workspace")?;
    } else {
        provision_until_running(
            &mut driver,
            runtime.as_ref(),
            &SessionId::from(target.session.clone()),
        )
        .await?;
    }
    if selected_digest == current_digest {
        snapshot_if_present(runtime.as_ref(), &capsule_image, &lock, &selected_digest).await?;
    }
    Ok(())
}

fn prepare_box_cache_volume(
    objects: &[OwnedObject],
    target: &BoxRef,
    toolchain: &str,
    enabled: bool,
    fresh: bool,
) -> CacheVolumePlan {
    let caches = cache_volumes_for_box(objects, target);
    let reset = fresh
        || caches.len() > 1
        || caches.first().is_some_and(|cache| {
            cache.labels.get(LABEL_TOOLCHAIN).map(String::as_str) != Some(toolchain)
        });
    CacheVolumePlan {
        enabled,
        remove: if reset {
            caches
                .iter()
                .map(|cache| VolumeId(cache.id.clone()))
                .collect()
        } else {
            Vec::new()
        },
        create: enabled && (reset || caches.is_empty()),
    }
}

fn resume_cache_enabled(global_enabled: bool, box_no_cache: bool, requested_cache: bool) -> bool {
    global_enabled && (!box_no_cache || requested_cache)
}

async fn remove_planned_cache_volumes(
    runtime: &dyn RuntimeAdapter,
    volumes: &[VolumeId],
) -> Result<()> {
    for volume in volumes {
        runtime
            .remove_volume(volume)
            .await
            .with_context(|| format!("removing cache volume {}", volume.0))?;
    }
    Ok(())
}

async fn create_box_cache_volume(
    runtime: &dyn RuntimeAdapter,
    options: &SessionOptions,
) -> Result<()> {
    let volume = state::cache_volume(&options.session);
    runtime
        .create_volume(&VolumeSpec {
            name: volume.0.clone(),
            labels: state::cache_volume_labels(options),
        })
        .await
        .with_context(|| format!("creating box cache volume {}", volume.0))?;
    Ok(())
}

async fn replace_box_containers<F, Fut>(
    runtime: &dyn RuntimeAdapter,
    containers: &[OwnedObject],
    was_running: bool,
    provision: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    // The workspace is never part of this cleanup path. SessionDriver owns
    // provisioning cleanup and is told to preserve the existing volume.
    for object in containers {
        let container = ContainerId(object.id.clone());
        if object.state.as_deref() == Some("running") {
            runtime
                .stop(&container, super::SESSION_STOP_GRACE)
                .await
                .context("stopping box container")?;
        }
        runtime
            .remove(&container, true)
            .await
            .context("removing old box container")?;
    }
    if was_running {
        note("Running tools were stopped. Start them again from the shell.");
    }
    provision().await
}

fn box_container_members(objects: &[OwnedObject], target: &BoxRef) -> Vec<OwnedObject> {
    objects
        .iter()
        .filter(|object| {
            object.kind == OwnedObjectKind::Container
                && object
                    .session
                    .as_ref()
                    .is_some_and(|id| id.as_str() == target.session)
                && matches!(object.role.as_deref(), Some(ROLE_CAPSULE | ROLE_GATEWAY))
        })
        .cloned()
        .collect()
}

fn has_box_container_roles(containers: &[OwnedObject]) -> bool {
    [ROLE_CAPSULE, ROLE_GATEWAY].iter().all(|role| {
        containers
            .iter()
            .any(|object| object.role.as_deref() == Some(*role))
    })
}

pub(crate) async fn stop(target: &BoxRef, runtime: Arc<dyn RuntimeAdapter>) -> Result<()> {
    let objects = runtime.list_owned().await?;
    let (capsule, gateway) = box_containers(&objects, target)?;
    for object in [&capsule, &gateway] {
        if object.state.as_deref() == Some("running") {
            runtime
                .stop(&ContainerId(object.id.clone()), super::SESSION_STOP_GRACE)
                .await
                .context("stopping box")?;
        }
    }
    Ok(())
}

pub(crate) async fn ls_rows(runtime: Arc<dyn RuntimeAdapter>) -> Result<Vec<BoxRow>> {
    let objects = runtime.list_owned().await?;
    let config = config::load(None)?;
    let resolved_lock = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let current_toolchain = crate::toolchain::digest(&resolved_lock.lock);
    let yanks = crate::update::known_yanks(&config, &resolved_lock.lock);
    let mut by_session: HashMap<String, Vec<&OwnedObject>> = HashMap::new();
    for object in &objects {
        if let Some(session) = &object.session {
            by_session
                .entry(session.to_string())
                .or_default()
                .push(object);
        }
    }
    let old = index::read()?;
    let entries = entries_from_labels(&objects, &old);
    index::write(&entries)?;
    let mut rows: Vec<_> = entries
        .iter()
        .map(|entry| {
            let members = by_session.get(&entry.session).cloned().unwrap_or_default();
            let target = BoxRef {
                name: entry.name.clone(),
                id: entry.id.clone(),
                session: entry.session.clone(),
            };
            let cache_volume = members
                .iter()
                .find(|object| cache_volume_matches_box(object, &target))
                .map(|object| object.id.clone());
            let running = members.iter().any(|object| {
                object.kind == OwnedObjectKind::Container
                    && object.state.as_deref() == Some("running")
            });
            let project_exists = entry.project.exists();
            let mut state = if running {
                "running".to_owned()
            } else {
                "stopped".to_owned()
            };
            if !project_exists {
                state = "path missing".into();
            }
            Ok(BoxRow {
                name: entry.name.clone(),
                session: Some(entry.session.clone()),
                state,
                project: entry.project.clone(),
                mode: entry.mode.clone(),
                providers: entry.providers.clone(),
                last_entered: entry.last_entered.clone(),
                toolchain: entry.toolchain.clone(),
                toolchain_current: entry.toolchain == current_toolchain,
                toolchain_pinned: entry.toolchain_pinned,
                toolchain_tested: toolchain_tested(
                    &entry.toolchain,
                    &current_toolchain,
                    &resolved_lock.lock,
                ),
                toolchain_yanked: toolchain_yanked(
                    &entry.toolchain,
                    &current_toolchain,
                    &resolved_lock.lock,
                    &yanks,
                )
                .is_some(),
                cache_volume,
            })
        })
        .collect::<Result<_>>()?;
    for (session, members) in &by_session {
        if is_legacy_box(members) {
            let project =
                crate::session::state::read_session_host_repo(&SessionId::from(session.clone()))
                    .unwrap_or_default();
            rows.push(BoxRow {
                name: session.clone(),
                session: Some(session.clone()),
                state: "legacy".into(),
                project,
                mode: "-".into(),
                providers: Vec::new(),
                last_entered: String::new(),
                toolchain: String::new(),
                toolchain_current: false,
                toolchain_pinned: false,
                toolchain_tested: false,
                toolchain_yanked: false,
                cache_volume: None,
            });
        }
    }
    Ok(rows)
}

fn is_legacy_box(members: &[&OwnedObject]) -> bool {
    let has_workspace_volume = members.iter().any(|object| {
        object.kind == OwnedObjectKind::Volume
            && object.role.as_deref() != Some(ROLE_CACHE)
            && object.labels.get(LABEL_ROLE).map(String::as_str) != Some(ROLE_CACHE)
    });
    members.iter().any(|object| {
        object.kind == OwnedObjectKind::Container
            && object.role.as_deref() == Some(ROLE_CAPSULE)
            && !object.labels.contains_key(LABEL_BOX)
    }) && has_workspace_volume
}

fn entries_from_labels(objects: &[OwnedObject], old: &[index::Entry]) -> Vec<index::Entry> {
    let mut by_session: HashMap<String, Vec<&OwnedObject>> = HashMap::new();
    for object in objects {
        if object.role.as_deref() == Some(ROLE_CACHE)
            || object.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CACHE)
        {
            continue;
        }
        if let Some(session) = &object.session {
            by_session
                .entry(session.to_string())
                .or_default()
                .push(object);
        }
    }
    let mut entries = Vec::new();
    for (session, members) in &by_session {
        let metadata_object = members
            .iter()
            .find(|object| {
                object.kind == OwnedObjectKind::Container
                    && object.role.as_deref() == Some(ROLE_CAPSULE)
            })
            .or_else(|| {
                members.iter().find(|object| {
                    object.kind == OwnedObjectKind::Container
                        && object.role.as_deref() == Some(ROLE_GATEWAY)
                })
            })
            .or_else(|| members.first());
        let Some(object) = metadata_object else {
            continue;
        };
        let Some(box_name) = object.labels.get(LABEL_BOX).cloned() else {
            continue;
        };
        let id = object.labels.get(LABEL_BOX_ID).cloned().unwrap_or_default();
        if id.is_empty() || box_name.is_empty() {
            continue;
        }
        let project = object
            .labels
            .get(LABEL_PROJECT)
            .map(PathBuf::from)
            .unwrap_or_default();
        let mode = members
            .iter()
            .find_map(|member| member.labels.get(LABEL_WORKSPACE_MODE))
            .cloned()
            .unwrap_or_else(|| "copy".into());
        let capsule = members.iter().find(|member| {
            member.kind == OwnedObjectKind::Container
                && member.role.as_deref() == Some(ROLE_CAPSULE)
        });
        let providers = object
            .labels
            .get(LABEL_PROVIDERS)
            .map(|value| {
                value
                    .split(',')
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let old_entry = old.iter().find(|old| old.session == *session);
        let toolchain = object
            .labels
            .get(LABEL_TOOLCHAIN)
            .filter(|digest| crate::toolchain::is_digest(digest))
            .cloned()
            .or_else(|| {
                capsule
                    .and_then(|capsule| capsule.image.as_deref())
                    .and_then(toolchain_digest_from_image)
            })
            .or_else(|| old_entry.map(|old| old.toolchain.clone()))
            .unwrap_or_default();
        let mut entry = index::Entry {
            name: box_name,
            id,
            session: session.clone(),
            project: project.clone(),
            dev: None,
            ino: None,
            mode,
            providers,
            created: object
                .labels
                .get(LABEL_CREATED)
                .cloned()
                .unwrap_or_default(),
            last_entered: String::new(),
            last_synced: String::new(),
            toolchain,
            toolchain_pinned: old_entry.is_some_and(|old| old.toolchain_pinned),
            no_cache: old_entry.is_some_and(|old| old.no_cache),
        };
        if let Some(old_entry) = old_entry {
            entry.dev = old_entry.dev.or(entry.dev);
            entry.ino = old_entry.ino.or(entry.ino);
            entry.last_entered = old_entry.last_entered.clone();
            entry.last_synced = old_entry.last_synced.clone();
            entry.created = old_entry.created.clone();
        }
        #[cfg(unix)]
        if entry.dev.is_none() || entry.ino.is_none() {
            if let Ok(meta) = std::fs::metadata(&project) {
                use std::os::unix::fs::MetadataExt;
                entry.dev.get_or_insert_with(|| meta.dev());
                entry.ino.get_or_insert_with(|| meta.ino());
            }
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

fn toolchain_tested(
    digest: &str,
    current_digest: &str,
    current: &crate::toolchain::ToolchainLock,
) -> bool {
    let lock = if digest == current_digest {
        Some(current.clone())
    } else {
        crate::toolchain::read_snapshot(digest)
            .ok()
            .map(|snapshot| snapshot.lock)
    };
    lock.is_some_and(|lock| {
        !lock.untested
            && lock
                .tools
                .values()
                .all(|tool| tool.smoke != crate::toolchain::SmokeGrade::None)
    })
}

/// The yank entry that withdraws a box's toolchain, checked against its snapshot.
fn toolchain_yanked(
    digest: &str,
    current_digest: &str,
    current: &crate::toolchain::ToolchainLock,
    yanks: &[String],
) -> Option<String> {
    if digest.is_empty() || yanks.is_empty() {
        return None;
    }
    let lock = if digest == current_digest {
        Some(current.clone())
    } else {
        crate::toolchain::read_snapshot(digest)
            .ok()
            .map(|snapshot| snapshot.lock)
    };
    crate::toolchain::yanked_by(digest, lock.as_ref(), yanks)
}

fn short_digest(digest: &str) -> &str {
    digest.get(..7).unwrap_or(digest)
}

fn running_toolchain_note(
    box_name: &str,
    box_digest: &str,
    current_digest: &str,
) -> Option<String> {
    (!box_digest.is_empty() && box_digest != current_digest).then(|| {
        format!(
            "toolchain {} in use; current is {}. Switch with: `capscode resume {} --toolchain current`",
            short_digest(box_digest),
            short_digest(current_digest),
            box_name
        )
    })
}

fn noninteractive_running_resume_error(box_name: &str) -> String {
    format!(
        "box '{box_name}' is already running; resuming will stop its tools and recreate its containers. Rerun with `--yes` to confirm"
    )
}

fn select_providers(
    candidates: &[(ProviderSpec, ProviderCredential)],
    defaults: &BTreeMap<Tool, String>,
    spec: Option<&str>,
) -> Result<Vec<(ProviderSpec, ProviderCredential)>> {
    let available: HashMap<_, _> = candidates
        .iter()
        .map(|(provider, _)| (provider.id.as_str(), provider))
        .collect();
    let ids: Vec<String> = if let Some(spec) = spec {
        if spec == "all" {
            candidates
                .iter()
                .map(|(provider, _)| provider.id.clone())
                .collect()
        } else {
            spec.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect()
        }
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
        let selected_defaults: Vec<bool> = candidates
            .iter()
            .map(|(provider, _)| defaults.values().any(|id| id == &provider.id))
            .collect();
        let labels: Vec<_> = candidates
            .iter()
            .map(|(provider, _)| provider.id.clone())
            .collect();
        let picks = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Choose providers to route into this box")
            .items(&labels)
            .defaults(&selected_defaults)
            .interact()
            .context("choosing box providers")?;
        picks
            .into_iter()
            .map(|index| candidates[index].0.id.clone())
            .collect()
    } else {
        let mut seen = std::collections::HashSet::new();
        defaults
            .values()
            .filter(|id| seen.insert((*id).clone()))
            .cloned()
            .collect()
    };
    if ids.is_empty() {
        bail!("select at least one provider with a stored key");
    }
    let mut unique = std::collections::HashSet::new();
    for id in &ids {
        if !available.contains_key(id.as_str()) {
            bail!("provider '{id}' has no stored key; run `capscode key set {id}`");
        }
        if !unique.insert(id) {
            bail!("provider '{id}' was selected more than once");
        }
    }
    Ok(candidates
        .iter()
        .filter(|(provider, _)| unique.contains(&provider.id))
        .cloned()
        .collect())
}

fn split_credentials(
    mut routed: Vec<RoutedProvider>,
) -> Result<(ProviderSpec, ProviderCredential, Vec<RoutedProvider>)> {
    if routed.is_empty() {
        bail!("no stored keys; run `capscode key set <id>`");
    }
    let first = routed.remove(0);
    Ok((first.0, first.1, routed))
}

fn make_box_spec(
    name: &str,
    id: &str,
    project: &Path,
    selected: &ProviderSpec,
    extras: &[(ProviderSpec, ProviderCredential)],
    defaults: &BTreeMap<Tool, String>,
    toolchain: &str,
) -> BoxSpec {
    let mut routed = vec![selected.id.clone()];
    routed.extend(extras.iter().map(|(spec, _)| spec.id.clone()));
    routed.sort();
    let project_text = project.to_string_lossy().into_owned();
    let project_hash = format!("{:x}", Sha256::digest(project_text.as_bytes()));
    BoxSpec {
        name: name.into(),
        id: id.into(),
        project: project_text,
        project_hash,
        routed,
        tool_defaults: defaults.clone(),
        toolchain: toolchain.to_owned(),
        cache_volume: false,
    }
}

fn index_entry(
    name: &str,
    id: &str,
    session: &SessionId,
    project: &Path,
    routed: &[(ProviderSpec, ProviderCredential)],
) -> index::Entry {
    let mut entry = index::Entry {
        name: name.into(),
        id: id.into(),
        session: session.to_string(),
        project: project.to_path_buf(),
        mode: "copy".into(),
        providers: routed.iter().map(|(spec, _)| spec.id.clone()).collect(),
        ..Default::default()
    };
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(project) {
        use std::os::unix::fs::MetadataExt;
        entry.dev = Some(meta.dev());
        entry.ino = Some(meta.ino());
    }
    entry
}

fn print_creation(
    routed: &[(ProviderSpec, ProviderCredential)],
    defaults: &BTreeMap<Tool, String>,
) {
    note(format!(
        "Routed providers: {}",
        routed
            .iter()
            .map(|(provider, _)| provider.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for tool in Tool::ALL {
        let status = defaults
            .get(&tool)
            .map(|id| {
                if routed.iter().any(|(provider, _)| &provider.id == id) {
                    "usable".to_owned()
                } else {
                    format!("not usable ({id} not routed)")
                }
            })
            .unwrap_or_else(|| "not usable (no default provider)".into());
        note(format!("{}: {status}", tool.name()));
    }
}

fn canonical_project(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("resolving project path {}", path.display()))
}

fn prepare_session_state_dir(state_dir: &Path) -> Result<()> {
    Config::ensure_config_home().context("preparing private CapsCode state directory")?;
    let sessions_dir = state_dir
        .parent()
        .context("session state directory has no parent")?;
    std::fs::create_dir_all(sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(sessions_dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

async fn provision_until_running(
    driver: &mut crate::session::state::SessionDriver,
    runtime: &dyn RuntimeAdapter,
    session: &SessionId,
) -> Result<()> {
    while driver.state() != crate::session::state::State::Running {
        if let Err(error) = driver.advance().await {
            cleanup_session_containers(runtime, session).await;
            return Err(error).context("provisioning box containers");
        }
    }
    Ok(())
}

async fn cleanup_session_containers(runtime: &dyn RuntimeAdapter, session: &SessionId) {
    let Ok(objects) = runtime.list_owned().await else {
        return;
    };
    let mut containers: Vec<_> = objects
        .into_iter()
        .filter(|object| {
            object.kind == OwnedObjectKind::Container && object.session.as_ref() == Some(session)
        })
        .collect();
    containers.sort_by_key(|object| {
        if object.role.as_deref() == Some(ROLE_CAPSULE) {
            0
        } else {
            1
        }
    });
    for object in containers {
        let id = ContainerId(object.id);
        if object.state.as_deref() == Some("running") {
            let _ = runtime.stop(&id, super::SESSION_STOP_GRACE).await;
        }
        let _ = runtime.remove(&id, true).await;
    }
}

fn provision_config(
    selected_toolchain: Option<&str>,
) -> Result<(
    Config,
    ImageRef,
    ImageRef,
    ImageRef,
    String,
    crate::toolchain::ToolchainLock,
)> {
    let config = config::load_with_auth_status(None)?.0;
    let resolved = crate::toolchain::resolve(config.image.toolchain_lock.as_deref())?;
    let current_digest = crate::toolchain::digest(&resolved.lock);
    let gateway = config::gateway_image(&config)?;
    let capsule = if let Some(digest) = selected_toolchain {
        if !crate::toolchain::is_digest(digest) {
            bail!("toolchain digest must be 12 lowercase hexadecimal characters");
        }
        ImageRef(crate::toolchain::capsule_all_image(digest))
    } else {
        config::capsule_all_image(&config)?
    };
    let seeder = config::seeder_image(&config)?;
    Ok((
        config,
        gateway,
        capsule,
        seeder,
        current_digest,
        resolved.lock,
    ))
}

async fn snapshot_if_present(
    runtime: &dyn RuntimeAdapter,
    image: &ImageRef,
    lock: &crate::toolchain::ToolchainLock,
    digest: &str,
) -> Result<()> {
    if runtime.image_present(image).await? {
        crate::toolchain::write_snapshot(lock, digest)?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ResumeToolchainDecision {
    Current,
    Keep,
    Digest(String),
    Prompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResumeToolchainPromptChoice {
    Switch,
    Keep,
    Cancel,
}

fn resume_prompt_choice(key: &console::Key) -> Option<ResumeToolchainPromptChoice> {
    match key {
        console::Key::Char('s' | 'S') => Some(ResumeToolchainPromptChoice::Switch),
        console::Key::Char('k' | 'K') | console::Key::Enter => {
            Some(ResumeToolchainPromptChoice::Keep)
        }
        console::Key::Char('c' | 'C') | console::Key::Escape => {
            Some(ResumeToolchainPromptChoice::Cancel)
        }
        _ => None,
    }
}

fn prompt_resume_toolchain_choice(prompt: &str) -> Result<ResumeToolchainPromptChoice> {
    let terminal = console::Term::stderr();
    note(format!("{prompt}\n[s]witch, [k]eep (Enter), or [c]ancel?"));
    loop {
        let key = terminal.read_key().context("reading toolchain choice")?;
        if let Some(choice) = resume_prompt_choice(&key) {
            return Ok(choice);
        }
        note("Choose s to switch, k or Enter to keep, or c to cancel.");
    }
}

fn require_current_image_contract(
    digest: &str,
    image: Option<&capscode_core::LocalImageInfo>,
) -> Result<()> {
    if image.is_none_or(|info| !crate::commands::image_contract_is_current(info)) {
        bail!(
            "toolchain {digest} was built for an older CapsCode and must be rebuilt: `capscode toolchain build {digest}`"
        );
    }
    Ok(())
}

async fn toolchain_image_needs_rebuild(runtime: &dyn RuntimeAdapter, digest: &str) -> Result<bool> {
    let image = crate::toolchain::capsule_all_image(digest);
    if !runtime.image_present(&ImageRef(image.clone())).await? {
        return Ok(false);
    }
    let images = runtime.list_local_image_details().await?;
    Ok(images
        .iter()
        .find(|info| info.reference == image)
        .is_none_or(|info| !crate::commands::image_contract_is_current(info)))
}

/// Withdrawn-toolchain lookup plus `--force`, for an explicitly requested digest.
struct YankGate<'a> {
    yanked: &'a dyn Fn(&str) -> Option<String>,
    force: bool,
}

fn resume_toolchain_decision(
    running: bool,
    pinned: bool,
    requested: Option<&str>,
    old: &str,
    current: &str,
    interactive: bool,
    gate: &YankGate<'_>,
) -> Result<ResumeToolchainDecision> {
    if let Some(requested) = requested {
        return match requested {
            "current" => Ok(ResumeToolchainDecision::Current),
            "keep" => Ok(ResumeToolchainDecision::Keep),
            digest if crate::toolchain::is_digest(digest) => {
                if let Some(entry) = (gate.yanked)(digest) {
                    if !gate.force {
                        bail!(
                            "toolchain {digest} was withdrawn ({entry}); choose `--toolchain current`, or pass `--force` to use it anyway"
                        );
                    }
                    note(format!(
                        "warning: using withdrawn toolchain {digest} ({entry}) because of --force"
                    ));
                }
                Ok(ResumeToolchainDecision::Digest(digest.to_owned()))
            }
            _ => bail!("--toolchain must be current, keep, or a 12-character digest"),
        };
    }
    if old == current || running || pinned || !interactive {
        return Ok(ResumeToolchainDecision::Keep);
    }
    Ok(ResumeToolchainDecision::Prompt)
}

async fn validate_selected_toolchain(
    runtime: &dyn RuntimeAdapter,
    selected: &str,
    current: &str,
    old: &str,
) -> Result<()> {
    if !crate::toolchain::is_digest(selected) {
        bail!("toolchain digest must be 12 lowercase hexadecimal characters");
    }
    let image = ImageRef(crate::toolchain::capsule_all_image(selected));
    let image_present = runtime.image_present(&image).await?;
    if !image_present && selected == current {
        return Ok(());
    }
    let snapshot = crate::toolchain::snapshot_path(selected)?;
    let snapshot_present = snapshot.is_file();
    if !image_present && selected == old {
        return Err(missing_old_toolchain_error(selected));
    }
    if !image_present && snapshot_present {
        bail!("toolchain image missing; build it with `capscode toolchain build {selected}`");
    }
    if !image_present {
        bail!(
            "toolchain {selected} has no saved snapshot or local image; use `capscode toolchain ls`"
        );
    }
    let images = runtime.list_local_image_details().await?;
    require_current_image_contract(
        selected,
        images.iter().find(|info| info.reference == image.0),
    )?;
    if snapshot_present {
        let _ = crate::toolchain::read_snapshot(selected)?;
    }
    Ok(())
}

fn missing_old_toolchain_error(digest: &str) -> anyhow::Error {
    anyhow!(
        "old toolchain image missing; rerun with `--toolchain current` or build it: `capscode toolchain build {digest}`"
    )
}

fn box_toolchain(entry: &index::Entry, capsule: Option<&OwnedObject>) -> String {
    capsule
        .and_then(|object| object.labels.get(LABEL_TOOLCHAIN))
        .filter(|digest| crate::toolchain::is_digest(digest))
        .cloned()
        .or_else(|| {
            capsule
                .and_then(|object| object.image.as_deref())
                .and_then(toolchain_digest_from_image)
        })
        .or_else(|| {
            (!entry.toolchain.is_empty() && crate::toolchain::is_digest(&entry.toolchain))
                .then(|| entry.toolchain.clone())
        })
        .unwrap_or_default()
}

fn toolchain_digest_from_image(image: &str) -> Option<String> {
    let tag = image.strip_prefix("capscode/capsule-all:")?;
    crate::toolchain::is_digest(tag).then(|| tag.to_owned())
}

fn capsule_for(objects: &[OwnedObject], target: &BoxRef) -> Result<OwnedObject> {
    objects
        .iter()
        .find(|object| {
            object.kind == OwnedObjectKind::Container
                && object.role.as_deref() == Some(ROLE_CAPSULE)
                && object
                    .session
                    .as_ref()
                    .is_some_and(|id| id.as_str() == target.session)
        })
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "box '{}' has no capsule; run `capscode ls` to inspect it",
                target.name
            )
        })
}

fn box_containers(objects: &[OwnedObject], target: &BoxRef) -> Result<(OwnedObject, OwnedObject)> {
    let is_session = |object: &&OwnedObject| {
        object
            .session
            .as_ref()
            .is_some_and(|id| id.as_str() == target.session)
    };
    let capsule = objects
        .iter()
        .filter(is_session)
        .find(|object| {
            object.kind == OwnedObjectKind::Container
                && object.role.as_deref() == Some(ROLE_CAPSULE)
        })
        .cloned()
        .ok_or_else(|| anyhow!("box '{}' capsule was not found", target.name))?;
    let gateway = objects
        .iter()
        .filter(is_session)
        .find(|object| {
            object.kind == OwnedObjectKind::Container
                && object.role.as_deref() == Some(ROLE_GATEWAY)
        })
        .cloned()
        .ok_or_else(|| anyhow!("box '{}' gateway was not found", target.name))?;
    Ok((capsule, gateway))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn only_unnamed_create_refuses_a_second_box_for_the_project() {
        assert!(should_refuse_existing_box(true, false));
        assert!(!should_refuse_existing_box(true, true));
        assert!(!should_refuse_existing_box(false, false));
        assert!(!should_refuse_existing_box(false, true));
    }

    const NO_YANKS: YankGate<'static> = YankGate {
        yanked: &|_| None,
        force: false,
    };

    #[test]
    fn resume_refuses_an_explicit_yanked_digest_without_force() {
        let old = "6d07f8901234";
        let current = "fb24882402b5";
        let yanked = |digest: &str| (digest == old).then(|| "codex@0.160.0".to_owned());
        let refused = resume_toolchain_decision(
            false,
            false,
            Some(old),
            old,
            current,
            false,
            &YankGate {
                yanked: &yanked,
                force: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("withdrawn"), "{refused}");
        assert!(refused.contains("--force"), "{refused}");
        assert_eq!(
            resume_toolchain_decision(
                false,
                false,
                Some(old),
                old,
                current,
                false,
                &YankGate {
                    yanked: &yanked,
                    force: true,
                },
            )
            .unwrap(),
            ResumeToolchainDecision::Digest(old.to_owned())
        );
    }

    #[test]
    fn resume_toolchain_decision_table_preserves_running_same_pinned_and_non_tty_boxes() {
        let old = "6d07f8901234";
        let current = "fb24882402b5";
        assert_eq!(
            resume_toolchain_decision(true, false, None, old, current, true, &NO_YANKS).unwrap(),
            ResumeToolchainDecision::Keep,
            "running boxes do not prompt to switch"
        );
        assert_eq!(
            resume_toolchain_decision(false, false, None, current, current, true, &NO_YANKS)
                .unwrap(),
            ResumeToolchainDecision::Keep,
            "same toolchain does not prompt"
        );
        assert_eq!(
            resume_toolchain_decision(false, false, None, old, current, true, &NO_YANKS).unwrap(),
            ResumeToolchainDecision::Prompt
        );
        assert_eq!(
            resume_toolchain_decision(false, false, None, old, current, false, &NO_YANKS).unwrap(),
            ResumeToolchainDecision::Keep,
            "non-tty resumes keep the recorded toolchain"
        );
        assert_eq!(
            resume_toolchain_decision(false, true, None, old, current, true, &NO_YANKS).unwrap(),
            ResumeToolchainDecision::Keep,
            "pinned boxes keep without prompting"
        );
    }

    #[test]
    fn resume_prompt_key_mapping_accepts_letters_and_defaults_enter_to_keep() {
        assert_eq!(
            resume_prompt_choice(&console::Key::Char('s')),
            Some(ResumeToolchainPromptChoice::Switch)
        );
        assert_eq!(
            resume_prompt_choice(&console::Key::Char('K')),
            Some(ResumeToolchainPromptChoice::Keep)
        );
        assert_eq!(
            resume_prompt_choice(&console::Key::Enter),
            Some(ResumeToolchainPromptChoice::Keep)
        );
        assert_eq!(
            resume_prompt_choice(&console::Key::Char('c')),
            Some(ResumeToolchainPromptChoice::Cancel)
        );
        assert_eq!(resume_prompt_choice(&console::Key::ArrowDown), None);
    }

    #[test]
    fn old_toolchain_image_contract_fails_with_a_rebuild_command() {
        let image = |contract: Option<&str>| {
            let mut labels = BTreeMap::from([
                (capscode_core::LABEL_OWNER.to_owned(), "1".to_owned()),
                (
                    capscode_core::LABEL_ROLE.to_owned(),
                    ROLE_CAPSULE.to_owned(),
                ),
            ]);
            if let Some(contract) = contract {
                labels.insert(
                    capscode_core::LABEL_CONTRACT.to_owned(),
                    contract.to_owned(),
                );
            }
            capscode_core::LocalImageInfo {
                reference: "capscode/capsule-all:6d07f8901234".into(),
                size_bytes: None,
                created_unix: None,
                labels,
            }
        };

        require_current_image_contract("6d07f8901234", Some(&image(Some("2")))).unwrap();
        for old in [image(None), image(Some("1")), image(Some("unknown"))] {
            let error = require_current_image_contract("6d07f8901234", Some(&old))
                .unwrap_err()
                .to_string();
            assert!(error.contains("built for an older CapsCode"));
            assert!(error.contains("capscode toolchain build 6d07f8901234"));
        }
    }

    #[test]
    fn non_tty_running_resume_error_explains_the_confirmation_flag() {
        let message = noninteractive_running_resume_error("alpha-123abc");
        assert!(message.contains("already running"));
        assert!(message.contains("stop its tools and recreate its containers"));
        assert!(message.contains("--yes"));
    }

    #[test]
    fn resume_toolchain_flag_accepts_current_keep_and_digest() {
        let old = "6d07f8901234";
        let current = "fb24882402b5";
        assert_eq!(
            resume_toolchain_decision(
                false,
                false,
                Some("current"),
                old,
                current,
                false,
                &NO_YANKS
            )
            .unwrap(),
            ResumeToolchainDecision::Current
        );
        assert_eq!(
            resume_toolchain_decision(false, false, Some("keep"), old, current, false, &NO_YANKS)
                .unwrap(),
            ResumeToolchainDecision::Keep
        );
        assert_eq!(
            resume_toolchain_decision(false, false, Some(old), current, current, false, &NO_YANKS)
                .unwrap(),
            ResumeToolchainDecision::Digest(old.to_owned())
        );
        assert!(resume_toolchain_decision(
            false,
            false,
            Some("bad"),
            old,
            current,
            false,
            &NO_YANKS
        )
        .is_err());
    }

    #[test]
    fn missing_old_image_error_names_both_recovery_commands() {
        let message = missing_old_toolchain_error("6d07f8901234").to_string();
        assert!(message.contains("--toolchain current"));
        assert!(message.contains("capscode toolchain build 6d07f8901234"));
    }

    #[test]
    fn running_box_gets_one_post_attach_note_when_its_toolchain_is_older() {
        let message =
            running_toolchain_note("alpha-123abc", "6d07f8901234", "fb24882402b5").unwrap();
        assert!(message.contains("6d07f89 in use"));
        assert!(message.contains("current is fb24882"));
        assert!(message.contains("capscode resume alpha-123abc --toolchain current"));
        assert!(running_toolchain_note("alpha-123abc", "same", "same").is_none());
    }

    #[test]
    fn legacy_box_uses_the_stopped_capsule_image_tag_as_toolchain_fallback() {
        let entry = index::Entry::default();
        let capsule = OwnedObject {
            kind: OwnedObjectKind::Container,
            id: "capsule".into(),
            session: Some(SessionId::from("s1".to_owned())),
            role: Some(ROLE_CAPSULE.into()),
            created_at: None,
            state: Some("exited".into()),
            image: Some("capscode/capsule-all:6d07f8901234".into()),
            labels: Default::default(),
        };
        assert_eq!(box_toolchain(&entry, Some(&capsule)), "6d07f8901234");
    }
    use capscode_core::{
        AttachIo, ContainerSpec, OwnedObjectKind, PublishMode, PullProgress, RuntimeCapabilities,
        RuntimeError, RuntimeInfo, RuntimeKind, SessionId, VolumeSpec,
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockRuntime(Mutex<Vec<String>>, Mutex<Vec<OwnedObject>>);

    impl MockRuntime {
        fn unused(&self) -> RuntimeError {
            RuntimeError::CapabilityUnsupported(self.kind(), "unused mock operation".into())
        }
    }

    #[async_trait::async_trait]
    impl RuntimeAdapter for MockRuntime {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Docker
        }
        async fn preflight(&self) -> std::result::Result<RuntimeInfo, RuntimeError> {
            Err(self.unused())
        }
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                localhost_publish: PublishMode::Native,
                shared_netns: true,
                tmpfs_mounts: true,
                cap_add_net_admin: true,
                dev_net_tun: true,
            }
        }
        async fn create_volume(
            &self,
            _: &VolumeSpec,
        ) -> std::result::Result<VolumeId, RuntimeError> {
            Err(self.unused())
        }
        async fn remove_volume(&self, id: &VolumeId) -> std::result::Result<(), RuntimeError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("remove-volume:{}", id.0));
            Ok(())
        }
        async fn image_present(&self, _: &ImageRef) -> std::result::Result<bool, RuntimeError> {
            Err(self.unused())
        }
        async fn pull_image(
            &self,
            _: &ImageRef,
            _: &mut (dyn FnMut(PullProgress) + Send),
        ) -> std::result::Result<(), RuntimeError> {
            Err(self.unused())
        }
        async fn create_container(
            &self,
            _: &ContainerSpec,
        ) -> std::result::Result<ContainerId, RuntimeError> {
            Err(self.unused())
        }
        async fn start(&self, _: &ContainerId) -> std::result::Result<(), RuntimeError> {
            Err(self.unused())
        }
        async fn exec(
            &self,
            _: &ContainerId,
            _: ExecSpec,
        ) -> std::result::Result<capscode_core::ExecOutput, RuntimeError> {
            Err(self.unused())
        }
        async fn run_interactive(
            &self,
            _: &ContainerId,
            _: ExecSpec,
            _: AttachIo,
        ) -> std::result::Result<i32, RuntimeError> {
            Err(self.unused())
        }
        async fn stop(
            &self,
            id: &ContainerId,
            _: Duration,
        ) -> std::result::Result<(), RuntimeError> {
            self.0.lock().unwrap().push(format!("stop:{}", id.0));
            Ok(())
        }
        async fn remove(&self, id: &ContainerId, _: bool) -> std::result::Result<(), RuntimeError> {
            self.0.lock().unwrap().push(format!("remove:{}", id.0));
            Ok(())
        }
        async fn list_owned(&self) -> std::result::Result<Vec<OwnedObject>, RuntimeError> {
            Ok(self.1.lock().unwrap().clone())
        }
    }

    fn object(
        kind: OwnedObjectKind,
        session: &str,
        labels: &[(&str, &str)],
        state: Option<&str>,
    ) -> OwnedObject {
        let labels = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        OwnedObject {
            kind,
            id: format!("{kind:?}"),
            session: Some(SessionId::from(session.to_owned())),
            role: None,
            created_at: None,
            state: state.map(str::to_owned),
            image: None,
            labels,
        }
    }

    #[tokio::test]
    async fn stop_uses_the_runtime_selected_by_the_caller() {
        let target = BoxRef {
            name: "alpha-123abc".into(),
            id: "123abc".into(),
            session: "s1".into(),
        };
        let runtime = Arc::new(MockRuntime::default());
        *runtime.1.lock().unwrap() = [ROLE_CAPSULE, ROLE_GATEWAY]
            .into_iter()
            .map(|role| OwnedObject {
                kind: OwnedObjectKind::Container,
                id: format!("{role}-id"),
                session: Some(SessionId::from("s1".to_owned())),
                role: Some(role.to_owned()),
                created_at: None,
                state: Some("running".into()),
                image: None,
                labels: Default::default(),
            })
            .collect();

        stop(&target, runtime.clone()).await.unwrap();

        assert_eq!(
            *runtime.0.lock().unwrap(),
            vec!["stop:capsule-id".to_owned(), "stop:gateway-id".to_owned()]
        );
    }

    #[test]
    fn labels_rebuild_the_index_and_drop_stale_rows() {
        let objects = vec![
            object(
                OwnedObjectKind::Container,
                "s1",
                &[
                    (LABEL_BOX, "alpha-123abc"),
                    (LABEL_BOX_ID, "123abc"),
                    (LABEL_PROJECT, "/tmp/project"),
                    (LABEL_PROVIDERS, "anthropic,openai"),
                    (LABEL_WORKSPACE_MODE, "copy"),
                ],
                Some("running"),
            ),
            object(
                OwnedObjectKind::Volume,
                "s1",
                &[
                    (LABEL_BOX, "alpha-123abc"),
                    (LABEL_BOX_ID, "123abc"),
                    (LABEL_PROJECT, "/tmp/project"),
                    (LABEL_PROVIDERS, "anthropic,openai"),
                ],
                None,
            ),
        ];
        let stale = index::Entry {
            name: "stale-abcdef".into(),
            session: "gone".into(),
            ..Default::default()
        };
        let entries = entries_from_labels(&objects, &[stale]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "alpha-123abc");
        assert_eq!(entries[0].providers, ["anthropic", "openai"]);
    }

    #[test]
    fn labels_reconcile_keeps_project_identity_when_its_path_is_missing() {
        let objects = vec![object(
            OwnedObjectKind::Container,
            "s1",
            &[
                (LABEL_BOX, "alpha-123abc"),
                (LABEL_BOX_ID, "123abc"),
                (LABEL_PROJECT, "/path/that/no/longer/exists"),
                (LABEL_PROVIDERS, "anthropic"),
                (LABEL_WORKSPACE_MODE, "copy"),
            ],
            Some("exited"),
        )];
        let stale = index::Entry {
            name: "alpha-123abc".into(),
            id: "123abc".into(),
            session: "s1".into(),
            dev: Some(7),
            ino: Some(11),
            last_entered: "2026-09-24T00:00:00Z".into(),
            last_synced: "2026-09-24T00:01:00Z".into(),
            ..Default::default()
        };
        let entries = entries_from_labels(&objects, &[stale]);
        assert_eq!(entries[0].dev, Some(7));
        assert_eq!(entries[0].ino, Some(11));
        assert_eq!(entries[0].last_entered, "2026-09-24T00:00:00Z");
        assert_eq!(entries[0].last_synced, "2026-09-24T00:01:00Z");
    }

    #[tokio::test]
    async fn failed_resume_provisioning_never_removes_the_workspace_volume() {
        let runtime = MockRuntime::default();
        let capsule = object(OwnedObjectKind::Container, "s1", &[], Some("exited"));
        let gateway = object(OwnedObjectKind::Container, "s1", &[], Some("exited"));
        let result = replace_box_containers(&runtime, &[capsule, gateway], false, || async {
            bail!("injected provisioning failure")
        })
        .await;
        assert!(result.is_err());
        assert!(!runtime
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("remove-volume:")));
    }

    #[tokio::test]
    async fn resume_can_provision_again_after_failure_removed_all_containers() {
        let runtime = MockRuntime::default();
        let provisioned = std::sync::atomic::AtomicBool::new(false);
        replace_box_containers(&runtime, &[], false, || async {
            provisioned.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
        assert!(provisioned.load(std::sync::atomic::Ordering::SeqCst));
        assert!(runtime.0.lock().unwrap().is_empty());
    }

    #[test]
    fn legacy_box_detection_requires_a_persistent_workspace() {
        let missing_containers = box_container_members(
            &[],
            &BoxRef {
                name: "alpha-123abc".into(),
                id: "123abc".into(),
                session: "s1".into(),
            },
        );
        assert!(missing_containers.is_empty());
        assert!(!has_box_container_roles(&missing_containers));
        let mut capsule = object(OwnedObjectKind::Container, "s1", &[], Some("exited"));
        capsule.role = Some(ROLE_CAPSULE.into());
        let volume = object(OwnedObjectKind::Volume, "s1", &[], None);
        assert!(is_legacy_box(&[&capsule, &volume]));
        assert!(!is_legacy_box(&[&capsule]));
    }

    #[tokio::test]
    async fn cache_volume_is_kept_on_toolchain_keep_and_recreated_after_container_removal() {
        let target = BoxRef {
            name: "alpha-123abc".into(),
            id: "123abc".into(),
            session: "s1".into(),
        };
        let old = "6d07f8901234";
        let current = "fb24882402b5";
        let cache = || OwnedObject {
            kind: OwnedObjectKind::Volume,
            id: "cache-volume-exact".into(),
            session: Some(SessionId::from("s1".to_owned())),
            role: Some(ROLE_CACHE.into()),
            created_at: None,
            state: None,
            image: None,
            labels: BTreeMap::from([
                (LABEL_OWNER.into(), LABEL_OWNER_VALUE.into()),
                (LABEL_SESSION.into(), "s1".into()),
                (LABEL_BOX.into(), "alpha-123abc".into()),
                (LABEL_BOX_ID.into(), "123abc".into()),
                (LABEL_ROLE.into(), ROLE_CACHE.into()),
                (LABEL_TOOLCHAIN.into(), old.into()),
            ]),
        };

        let keep_plan = prepare_box_cache_volume(&[cache()], &target, old, true, false);
        assert!(!keep_plan.create);
        assert!(keep_plan.remove.is_empty());

        let mut similar_cache = cache();
        similar_cache.id = "cache-volume-similar-box".into();
        similar_cache
            .labels
            .insert(LABEL_BOX.into(), "alpha-123abc2".into());
        similar_cache
            .labels
            .insert(LABEL_BOX_ID.into(), "123abc2".into());
        let mut workspace = cache();
        workspace.id = "capscode-s1-workspace".into();
        workspace.role = Some(ROLE_CAPSULE.into());
        workspace
            .labels
            .insert(LABEL_ROLE.into(), ROLE_CAPSULE.into());
        let inventory = [cache(), similar_cache, workspace];
        let switch_plan = prepare_box_cache_volume(&inventory, &target, current, true, false);
        assert!(switch_plan.create);
        assert_eq!(
            switch_plan
                .remove
                .iter()
                .map(|volume| volume.0.as_str())
                .collect::<Vec<_>>(),
            ["cache-volume-exact"]
        );

        let fresh_plan = prepare_box_cache_volume(&[cache()], &target, old, true, true);
        assert!(fresh_plan.create);
        assert_eq!(
            fresh_plan
                .remove
                .iter()
                .map(|volume| volume.0.as_str())
                .collect::<Vec<_>>(),
            ["cache-volume-exact"]
        );

        let switch_runtime = MockRuntime::default();
        let mut capsule = object(OwnedObjectKind::Container, "s1", &[], Some("exited"));
        capsule.id = "capsule-id".into();
        let runtime_ref = &switch_runtime;
        replace_box_containers(&switch_runtime, &[capsule], false, move || async move {
            remove_planned_cache_volumes(runtime_ref, &switch_plan.remove).await?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            *switch_runtime.0.lock().unwrap(),
            ["remove:capsule-id", "remove-volume:cache-volume-exact"]
        );
    }

    #[test]
    fn no_cache_is_persistent_until_resume_cache_and_global_disable_still_wins() {
        assert!(!resume_cache_enabled(true, true, false));
        assert!(resume_cache_enabled(true, true, true));
        assert!(resume_cache_enabled(true, false, false));
        assert!(!resume_cache_enabled(false, false, true));
    }
}
