use std::collections::BTreeMap;
use std::fs::File;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::session::timing::LaunchProfile;
use crate::terminal::TerminalGuard;
use crate::ui::{note, Caps, LaunchObserver, LaunchView, Status};

use anyhow::{anyhow, bail, Context, Result as AnyResult};
use capscode_broker::SessionCa;
use capscode_core::{
    ApiStyle, AttachIo, AuthKind, AuthScheme, Config, ContainerId, ContainerSpec, EgressPlan,
    EgressReport, ExecSpec, GitEngine, ImageRef, IngressPlan, IngressReceipt, Mount, NetContext,
    NetVerifyReport, NetworkConfig, NetworkMode, NetworkStack, ProbeOutcome, ProviderCredential,
    ProviderSpec, PullPolicy, RuntimeAdapter, SessionId, Tool, VolumeId, VolumeSpec, WorkspaceMode,
    LABEL_CREATED, LABEL_OWNER, LABEL_OWNER_VALUE, LABEL_ROLE, LABEL_SESSION, LABEL_TOOLCHAIN,
    ROLE_CACHE, ROLE_CAPSULE,
};
use capscode_git::shadow_egress;

use crate::session::shadow::ShadowOutcome;

/// Sentinel key handed to the capsule for API-key providers so tools don't error
/// on a missing key. The real key lives only in the gateway/broker.
const SENTINEL_API_KEY: &str = "sk-capscode-proxied";
const CAPSULE_PI_API_KEY_ENV: &str = "CAPSCODE_PI_API_KEY";
const CAPSULE_OMP_API_KEY_ENV: &str = "CAPSCODE_OMP_API_KEY";
/// Sentinel OAuth token handed to the capsule when the Anthropic credential is a
/// subscription setup-token: its `sk-ant-oat01-` shape makes Claude Code run in
/// OAuth mode (and emit its own `anthropic-beta` header) while the real token
/// never leaves the gateway/broker.
const SENTINEL_OAUTH_TOKEN: &str = "sk-ant-oat01-capscode-proxied";
const SENTINEL_CODEX_ACCESS_TOKEN: &str = "capscode-codex-access-proxied";
const CODEX_ACCESS_TOKEN_ENV: &str = "CODEX_ACCESS_TOKEN";
const CAPSULE_CODEX_TOKEN_ENV: &str = "CAPSCODE_CODEX_BROKER_TOKEN";
const CODEX_CHATGPT_VHOST: &str = "codex-chatgpt.broker.capscode.local";
/// Writes the generated tool config inside the capsule as root, handing `agent`
/// ownership of **every** directory level it creates. `install -d` applies
/// `-o`/`-g`/`-m` only to the final component, so a nested target such as
/// `~/.omp/agent/models.yml` can leave `~/.omp` root-owned and the tool
/// (uid 10001) could not create sibling runtime dirs under it — omp died with
/// `EACCES: mkdir '/home/agent/.omp/run'`.
const INJECT_TOOL_CONFIG_SCRIPT: &str = r#"umask 077
target=$1
case "$target" in
  /home/agent/?*) ;;
  *) echo "refusing to inject tool config outside /home/agent: $target" >&2; exit 1 ;;
esac
case "$target" in
  *..*) echo "refusing a parent reference in tool config path: $target" >&2; exit 1 ;;
  */) echo "refusing a directory as tool config path: $target" >&2; exit 1 ;;
esac
rest=${target%/*}
rest=${rest#/home/agent}
rest=${rest#/}
cur=/home/agent
if [ -n "$rest" ]; then
  IFS=/
  for part in $rest; do
    cur="$cur/$part"
    install -d -m 700 -o agent -g agent "$cur" || exit 1
  done
  unset IFS
fi
cat > "$target" || exit 1
chown agent:agent "$target" || exit 1
chmod 600 "$target" || exit 1
"#;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const LAUNCH_PHASES: [&str; 6] = [
    "preflight",
    "images",
    "ingress",
    "gateway",
    "capsule",
    "handoff",
];
static NO_LAUNCH_OBSERVER: () = ();
const LABEL_WORKSPACE_MODE: &str = "capscode.workspace_mode";
pub const LABEL_BOX: &str = "capscode.box";
pub const LABEL_BOX_ID: &str = "capscode.box_id";
pub const LABEL_PROJECT: &str = "capscode.project";
pub const LABEL_PROJECT_HASH: &str = "capscode.project_hash";
pub const LABEL_PROVIDERS: &str = "capscode.providers";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionSignal {
    Interrupt,
    Terminate,
    Hangup,
    Quit,
}

impl SessionSignal {
    fn description(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Hangup => "SIGHUP",
            Self::Quit => "SIGQUIT",
        }
    }
}

#[derive(Clone, Copy)]
enum ContainerRole {
    Gateway,
    Capsule,
}

struct PendingContainerCreate {
    role: ContainerRole,
    task: JoinHandle<Result<ContainerId, capscode_core::RuntimeError>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Created,
    Ingesting,
    Provisioning,
    Running,
    Draining,
    Egressing,
    Failed,
    TearingDown,
    Destroyed,
}

pub struct SessionOptions {
    pub session: SessionId,
    pub config: Config,
    pub host_repo: PathBuf,
    pub tool_command: String,
    pub tool_args: Vec<String>,
    pub gateway_image: ImageRef,
    pub capsule_image: ImageRef,
    pub seeder_image: ImageRef,
    pub selected_provider: ProviderSpec,
    pub selected_credential: Option<ProviderCredential>,
    pub selected_credential_source: Option<String>,
    /// Extra providers routed at creation so a long-lived box can switch
    /// providers between attaches without a rebuild (devbox mode only).
    pub extra_providers: Vec<(ProviderSpec, ProviderCredential)>,
    pub interactive_handoff: bool,
    pub verbose: bool,
    pub r#box: Option<BoxSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxSpec {
    pub name: String,
    pub id: String,
    pub project: String,
    pub project_hash: String,
    pub routed: Vec<String>,
    pub tool_defaults: BTreeMap<Tool, String>,
    pub toolchain: String,
    /// Whether this box has its isolated persistent helper-cache volume mounted.
    pub cache_volume: bool,
}

#[derive(Default)]
struct Resources {
    volume: Option<VolumeId>,
    gateway: Option<ContainerId>,
    capsule: Option<ContainerId>,
    ingress_receipt: Option<IngressReceipt>,
}

pub struct SessionDriver {
    state: State,
    transitions: Vec<State>,
    options: SessionOptions,
    workspace_mode: WorkspaceMode,
    keeper_mode: bool,
    box_ready: bool,
    skip_ingress: bool,
    runtime: Arc<dyn RuntimeAdapter>,
    git: Arc<dyn GitEngine>,
    network: Arc<dyn NetworkStack>,
    resources: Resources,
    pending_creates: Vec<PendingContainerCreate>,
    ca: SessionCa,
    failure: Option<anyhow::Error>,
    work_captured: bool,
    wip_skipped: bool,
    preserve_state_dir: bool,
    silent_shutdown: bool,
    tool_exit_code: i32,
    profile: LaunchProfile,
    launch_view: Option<LaunchView>,
    launch_mark: Instant,
    launch_elapsed: [Duration; LAUNCH_PHASES.len()],
}

impl SessionDriver {
    pub fn new(
        options: SessionOptions,
        runtime: Arc<dyn RuntimeAdapter>,
        git: Arc<dyn GitEngine>,
        network: Arc<dyn NetworkStack>,
    ) -> AnyResult<Self> {
        validate_box_routes(&options)?;
        match options.tool_command.as_str() {
            "pi" => {
                pi_command(&options)?;
            }
            "omp" => {
                omp_command(&options)?;
            }
            _ => {}
        }
        let ca = SessionCa::generate().context("generating per-session CA")?;
        let keeper_mode = options.r#box.is_some();
        let profile = LaunchProfile::new(
            options.session.as_str(),
            &options.host_repo,
            &format!("{:?}", runtime.kind()),
            if options.config.network.wireguard_conf.is_some() && options.config.network.enforce_vpn
            {
                "tunnel"
            } else {
                "containment"
            },
        );
        Ok(Self {
            state: State::Created,
            transitions: vec![State::Created],
            workspace_mode: options.config.workspace.mode,
            options,
            keeper_mode,
            box_ready: false,
            skip_ingress: false,
            runtime,
            git,
            network,
            resources: Resources::default(),
            pending_creates: Vec::new(),
            ca,
            failure: None,
            work_captured: false,
            wip_skipped: false,
            preserve_state_dir: false,
            silent_shutdown: false,
            tool_exit_code: 0,
            profile,
            launch_view: None,
            launch_mark: Instant::now(),
            launch_elapsed: [Duration::ZERO; LAUNCH_PHASES.len()],
        })
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Recreate gateway and capsule around a stopped box's existing workspace.
    /// The volume is marked as durable before any provisioning work begins, so
    /// every failure path can remove containers without removing user files.
    pub async fn provision_existing(&mut self, volume: VolumeId) -> AnyResult<()> {
        if self.options.r#box.is_none() {
            bail!("existing-volume provisioning requires box options");
        }
        if self.workspace_mode == WorkspaceMode::Mount {
            bail!("mount-mode boxes do not have a workspace volume to resume");
        }
        self.resources.volume = Some(volume);
        self.skip_ingress = true;
        while self.state != State::Running {
            if let Err(error) = self.advance().await {
                self.remember_failure(error);
                if self.state != State::Destroyed {
                    self.transition(State::Failed);
                }
                while self.state != State::Destroyed {
                    if let Err(cleanup_error) = self.advance().await {
                        self.remember_failure(cleanup_error);
                        if self.state != State::Destroyed {
                            self.transition(State::TearingDown);
                        }
                    }
                }
                return Err(self
                    .failure
                    .take()
                    .expect("provisioning error was recorded"));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn transitions(&self) -> &[State] {
        &self.transitions
    }

    fn transition(&mut self, next: State) {
        tracing::debug!(from = ?self.state, to = ?next, session = %self.options.session, "session transition");
        self.state = next;
        self.transitions.push(next);
    }

    fn launch_observer(&self) -> &dyn LaunchObserver {
        self.launch_view
            .as_ref()
            .map_or(&NO_LAUNCH_OBSERVER, |view| view as &dyn LaunchObserver)
    }

    fn launch_lap(&mut self, profile_phase: &str) {
        let elapsed = self.launch_mark.elapsed();
        self.launch_mark = Instant::now();
        self.profile.lap(profile_phase);

        let (row, detail, settled) = match profile_phase {
            "preflight" => (0, "runtime ready", true),
            "image-gateway" => (1, "gateway image ready", false),
            "image-capsule" => (1, "capsule image ready", false),
            "image-seeder" => (1, "seeder image ready", true),
            "ingress" => (2, "workspace ready", true),
            "gateway-spec" => (3, "spec ready", false),
            "gateway-create" => (3, "container created", false),
            "gateway-start" => (3, "container started", false),
            "gateway-secrets" => (3, "secrets injected", false),
            "ca-wait" => (3, "broker ready", false),
            "net-verify" => (3, "network verified", true),
            "capsule-spec" => (4, "spec ready", false),
            "capsule-create" => (4, "container created", false),
            "capsule-start" => (4, "supervisor started", false),
            "tool-config" => (4, "tool configured", true),
            _ => return,
        };
        self.launch_elapsed[row] += elapsed;
        if settled {
            self.settle_launch_row(row);
        } else {
            self.launch_observer().message(LAUNCH_PHASES[row], detail);
        }
    }

    fn settle_launch_row(&self, row: usize) {
        let phase = LAUNCH_PHASES[row];
        let elapsed = self.launch_elapsed[row];
        if let Some(view) = self.launch_view.as_ref() {
            view.lap(phase, elapsed);
        } else {
            note(format!(
                "{phase} complete ({})",
                render_launch_elapsed(elapsed)
            ));
        }
    }

    fn finish_launch_view(&mut self) {
        self.launch_elapsed[5] += self.launch_mark.elapsed();
        self.settle_launch_row(5);
        if let Some(view) = self.launch_view.take() {
            view.finish();
        }
    }

    pub async fn drive(
        &mut self,
        mut shutdown: watch::Receiver<Option<SessionSignal>>,
    ) -> AnyResult<i32> {
        while self.state != State::Destroyed {
            if matches!(self.state, State::Failed | State::TearingDown) {
                if let Err(error) = self.advance().await {
                    self.remember_failure(error);
                    if self.state != State::Destroyed {
                        self.transition(State::TearingDown);
                    }
                }
                continue;
            }

            if let Some(signal) = *shutdown.borrow() {
                self.abort_for_signal(signal);
                continue;
            }

            tokio::select! {
                result = self.advance() => {
                    if let Err(error) = result {
                        self.remember_failure(error);
                        self.transition(State::Failed);
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_ok() {
                        if let Some(signal) = *shutdown.borrow() {
                            self.abort_for_signal(signal);
                        }
                    }
                }
            }
        }

        match self.failure.take() {
            Some(error) => Err(error),
            None => Ok(self.tool_exit_code),
        }
    }

    fn abort_for_signal(&mut self, signal: SessionSignal) {
        self.silent_shutdown = signal == SessionSignal::Hangup;
        if self.state == State::Egressing && self.workspace_mode == WorkspaceMode::Copy {
            self.publish_branch_after_interruption();
        }
        self.remember_failure(anyhow!(
            "session {} aborted by {} during {}",
            self.options.session,
            signal.description(),
            state_name(self.state)
        ));
        self.transition(State::Failed);
    }

    fn publish_branch_after_interruption(&self) {
        let Ok(branch) = crate::session::branch::publish_session_branch(
            &self.options.host_repo,
            &self.options.session,
        ) else {
            return;
        };
        if let Some(branch) = branch {
            if !self.silent_shutdown {
                note(crate::session::branch::branch_hint(&branch));
            }
        }
    }

    fn remember_failure(&mut self, error: anyhow::Error) {
        if self.failure.is_none() {
            self.failure = Some(error);
        } else {
            tracing::warn!(error = %error, "additional session failure during cleanup");
        }
    }

    /// Advance exactly one lifecycle transition. Errors are handled by [`Self::drive`],
    /// which routes them through `Failed -> TearingDown -> Destroyed`.
    pub async fn advance(&mut self) -> AnyResult<()> {
        match self.state {
            State::Created => {
                self.launch_view = LaunchView::new_with_verbose(
                    &LAUNCH_PHASES,
                    Caps::stderr(),
                    self.options.verbose,
                );
                self.launch_mark = Instant::now();
                self.runtime
                    .preflight()
                    .await
                    .context("runtime preflight")?;
                let caps = self.runtime.capabilities();
                if !caps.shared_netns || !caps.cap_add_net_admin {
                    bail!("runtime lacks shared network namespaces or NET_ADMIN support");
                }
                self.launch_lap("preflight");
                ensure_image(
                    self.runtime.as_ref(),
                    &self.options.gateway_image,
                    self.options.config.image.pull_policy,
                    false,
                    self.launch_observer(),
                )
                .await?;
                self.launch_lap("image-gateway");
                ensure_image(
                    self.runtime.as_ref(),
                    &self.options.capsule_image,
                    self.options.config.image.pull_policy,
                    false,
                    self.launch_observer(),
                )
                .await?;
                self.launch_lap("image-capsule");
                if self.workspace_mode == WorkspaceMode::Copy && !self.skip_ingress {
                    ensure_image(
                        self.runtime.as_ref(),
                        &self.options.seeder_image,
                        self.options.config.image.pull_policy,
                        true,
                        self.launch_observer(),
                    )
                    .await?;
                    self.launch_lap("image-seeder");
                }
                self.transition(State::Ingesting);
            }
            State::Ingesting => {
                let state_dir = ensure_session_state_dir(&self.options.session)
                    .context("creating private session state directory")?;
                persist_session_host_repo(&state_dir, &self.options.host_repo)
                    .context("persisting session host repository")?;
                persist_session_workspace_mode(&state_dir, self.workspace_mode)
                    .context("persisting session workspace mode")?;
                if self.skip_ingress {
                    if self.workspace_mode != WorkspaceMode::Mount
                        && self.resources.volume.is_none()
                    {
                        bail!("box workspace volume is missing; resume the box to recreate it");
                    }
                    self.transition(State::Provisioning);
                    return Ok(());
                }
                if self.workspace_mode == WorkspaceMode::Mount {
                    self.work_captured = true;
                    self.transition(State::Provisioning);
                    return Ok(());
                }
                let volume = workspace_volume(&self.options.session);
                self.resources.volume = Some(volume);
                if self.options.r#box.is_some() {
                    // Create the volume with box identity first. The ingress
                    // helper reuses the same name while seeding the workspace.
                    let volume = self.resources.volume.as_ref().expect("volume was set");
                    self.runtime
                        .create_volume(&VolumeSpec {
                            name: volume.0.clone(),
                            labels: workspace_volume_labels(&self.options),
                        })
                        .await
                        .context("creating box workspace volume")?;
                }
                let report = {
                    let observer = self.launch_observer();
                    let mut progress = |event: capscode_core::IngressProgress| {
                        observer.message("ingress", &event.phase);
                    };
                    self.git
                        .ingress(
                            &IngressPlan {
                                host_repo: self.options.host_repo.clone(),
                                session: self.options.session.clone(),
                                state_dir,
                                seeder_image: self.options.seeder_image.clone(),
                                history: self.options.config.git.history,
                                include: self.options.config.git.include.clone(),
                                verbose: self.options.verbose,
                            },
                            self.runtime.as_ref(),
                            &mut progress,
                        )
                        .await
                        .context("ingress")?
                };
                // `ingress` is the billed phase; the engine's own sub-steps are
                // spliced under it as a breakdown and excluded from the total.
                self.launch_lap("ingress");
                self.profile.absorb_ingress(&report);
                self.resources.ingress_receipt = Some(report.receipt.clone());
                if self.options.verbose {
                    let mode = match &report.receipt {
                        IngressReceipt::GitSnapshot { .. } => "GitSnapshot",
                        IngressReceipt::Shadow { .. } => "RsyncFallback",
                    };
                    note(format!(
                        "Ingress copied {} bytes in {:.2}s ({mode})",
                        report.copied_bytes,
                        report.elapsed.as_secs_f64()
                    ));
                } else {
                    note(format!(
                        "Workspace prepared in {:.2}s.",
                        report.elapsed.as_secs_f64()
                    ));
                }
                self.transition(State::Provisioning);
            }
            State::Provisioning => {
                self.provision().await?;
                self.finish_launch_view();
                self.profile.lap("handoff-ready");
                // Report before the tool takes the terminal, or the table is lost.
                self.profile.write_jsonl();
                if LaunchProfile::wants_table(self.options.verbose) {
                    note(self.profile.render_table());
                }
                self.transition(State::Running);
            }
            State::Running => {
                let capsule = self
                    .resources
                    .capsule
                    .as_ref()
                    .context("capsule missing in Running")?;
                let command = if self.keeper_mode {
                    crate::boxes::shell_attach_command(&self.options.config.r#box.prefix)?
                } else {
                    tool_command(&self.options)?
                };
                let mut code = {
                    crate::ui::ensure_no_live_region()?;
                    let _guard = self.options.interactive_handoff.then(TerminalGuard::new);
                    self.runtime
                        .run_interactive(
                            capsule,
                            ExecSpec {
                                cmd: command,
                                workdir: Some(PathBuf::from("/workspace")),
                                user: Some("agent".to_string()),
                                tty: true,
                                ..Default::default()
                            },
                            AttachIo {
                                stdin: capscode_runtime::cancellable_stdin()
                                    .context("preparing cancellable terminal input")?,
                                stdout: Box::new(tokio::io::stdout()),
                                stderr: Box::new(tokio::io::stderr()),
                            },
                        )
                        .await
                        .context("interactive tool")?
                };
                if self.keeper_mode && self.options.r#box.is_none() {
                    if let Some(tool_code) = self.read_keeper_tool_exit_status(capsule).await? {
                        code = tool_code;
                    }
                }
                self.tool_exit_code = code;
                if code != 0 {
                    let disposition = if self.keeper_mode {
                        "leaving the box running"
                    } else {
                        "preserving changes before teardown"
                    };
                    note(format!(
                        "Interactive session exited with status {code}; {disposition}."
                    ));
                }
                self.transition(State::Draining);
            }
            State::Draining => {
                if self.keeper_mode {
                    // The box is the durable workspace. Leaving its client must
                    // not stop it or copy a tree the agent may still be changing;
                    // explicit sync and rm own that snapshot boundary.
                    self.transition(State::TearingDown);
                    return Ok(());
                }
                if self.workspace_mode == WorkspaceMode::Mount {
                    // The host tree is the workspace already, so there is no
                    // capsule-local history or copy to commit and hand off.
                    self.work_captured = true;
                    self.transition(State::TearingDown);
                    return Ok(());
                }
                let receipt = self
                    .resources
                    .ingress_receipt
                    .clone()
                    .context("ingress receipt missing in Draining")?;
                match receipt {
                    IngressReceipt::GitSnapshot { .. } => {
                        self.commit_wip().await?;
                    }
                    IngressReceipt::Shadow { state_dir, .. } => {
                        let capsule = self
                            .resources
                            .capsule
                            .clone()
                            .context("capsule missing in file sync Draining")?;
                        // Quiesce BEFORE capture: stop the capsule so the tool's
                        // background writers can't race the export. A torn capture
                        // could read as empty, and teardown would then delete the
                        // only copy of the work.
                        crate::session::shadow::quiesce_and_capture(
                            self.runtime.as_ref(),
                            &capsule,
                            &state_dir,
                            self.options.verbose,
                        )
                        .await?;
                        // A durable host-side copy now exists in <state_dir>/export:
                        // the volume is disposable, and the state dir must be
                        // protected until egress decides its fate — a later handoff
                        // failure must not delete the captured work.
                        self.work_captured = !self.wip_skipped;
                        self.preserve_state_dir = true;
                    }
                }
                self.transition(State::Egressing);
            }
            State::Egressing => {
                let receipt = self
                    .resources
                    .ingress_receipt
                    .clone()
                    .context("ingress receipt missing in Egressing")?;
                match receipt {
                    IngressReceipt::GitSnapshot { .. } => {
                        let report = self.egress().await?;
                        if report.files_changed == 0 {
                            let ignored = self.has_ignored_workspace_files().await?;
                            self.work_captured = !self.wip_skipped;
                            self.git
                                .discard(&self.options.host_repo, &self.options.session)
                                .context("discarding unchanged capsule refs")?;
                            if ignored {
                                note("No tracked capsule changes. The agent created only files ignored by `.gitignore`; they were not included in the handoff.");
                            } else {
                                note("No capsule changes.");
                            }
                        } else {
                            self.work_captured = !self.wip_skipped;
                            print_summary(&report, self.options.verbose);
                            if self.options.interactive_handoff && std::io::stdin().is_terminal() {
                                self.exit_menu(report).await?;
                            } else {
                                let branch = self.save_as_generated_branch()?;
                                note(crate::session::branch::branch_hint(&branch));
                                note("Host working tree was not changed.");
                            }
                        }
                    }
                    IngressReceipt::Shadow {
                        git_dir,
                        baseline,
                        state_dir,
                    } => {
                        let baseline = baseline
                            .parse::<git2::Oid>()
                            .context("invalid file sync baseline OID")?;
                        let capsule = self
                            .resources
                            .capsule
                            .as_ref()
                            .context("capsule missing in file sync Egressing")?;
                        let handoff = shadow_egress(
                            capsule,
                            &self.options.host_repo,
                            &git_dir,
                            baseline,
                            &state_dir,
                            self.options.verbose,
                        )
                        .await
                        .context("capturing file sync egress")?;
                        // `Draining` already captured the export, so this reuses
                        // it rather than re-reading a capsule that is now stopped.
                        let outcome = crate::session::shadow::present_and_apply(
                            &handoff,
                            &self.options.host_repo,
                            self.options.interactive_handoff,
                        )?;
                        // Nothing to hand off means the durable copy is worthless,
                        // so teardown may clean it up. Anything else must survive:
                        // a later failure must not delete the only copy of the work.
                        self.preserve_state_dir = outcome != ShadowOutcome::NoChanges;
                    }
                }
                self.transition(State::TearingDown);
            }
            State::Failed => self.transition(State::TearingDown),
            State::TearingDown => {
                // In keeper mode even a failed handoff leaves the box and volume
                // for an explicit sync or rm. This makes volume deletion
                // unreachable from every keeper exit path.
                let outcome = if self.keeper_mode && self.box_ready {
                    self.detach().await
                } else {
                    self.teardown().await
                };
                if let Err(error) = outcome {
                    self.remember_failure(error);
                }
                self.transition(State::Destroyed);
            }
            State::Destroyed => {}
        }
        Ok(())
    }

    async fn provision(&mut self) -> AnyResult<()> {
        let net_context = NetContext {
            session: self.options.session.clone(),
            network: self.options.config.network.clone(),
            broker: self.options.config.broker.clone(),
            keys: self.options.config.broker.keys.clone(),
            ca_cert_pem: self.ca.ca_cert_pem().to_string(),
            gateway_image: self.options.gateway_image.clone(),
        };
        let mut gateway_spec = self
            .network
            .gateway_spec(&net_context)
            .context("building gateway specification")?;
        gateway_spec.labels.extend(box_labels(&self.options));
        set_container_env(
            &mut gateway_spec,
            "CAPSCODE_PROVIDERS",
            provider_manifest_json_multi(
                &self.options.selected_provider,
                self.options.selected_credential.as_ref(),
                &self.options.extra_providers,
            )?,
        );
        set_container_env(
            &mut gateway_spec,
            "CAPSCODE_BROKER_ALIASES",
            broker_aliases_multi(
                &self.options.selected_provider,
                self.options.selected_credential.as_ref(),
                &self.options.extra_providers,
            )?,
        );
        self.launch_lap("gateway-spec");
        let gateway = self
            .create_container_tracked(gateway_spec, ContainerRole::Gateway)
            .await
            .context("creating gateway")?;
        self.launch_lap("gateway-create");
        self.runtime
            .start(&gateway)
            .await
            .context("starting gateway")?;
        self.launch_lap("gateway-start");

        // Inject the high-value secrets (provider keys + WG config) into the
        // gateway's tmpfs over exec-stdin, then drop a `ready` sentinel. The
        // gateway entrypoint blocks on that sentinel, so the broker cannot serve
        // — and the tunnel cannot come up — until this completes (fail-closed).
        // Nothing secret is ever in the gateway env or on host disk.
        self.inject_gateway_secrets(&gateway).await?;
        self.launch_lap("gateway-secrets");

        // The broker is PID 1 inside the gateway and generates the authoritative
        // session CA there. Only its public certificate crosses into the capsule.
        let gateway_ca = self.read_gateway_ca(&gateway).await?;
        self.launch_lap("ca-wait");

        let report = self
            .network
            .verify(&gateway, self.runtime.as_ref())
            .await
            .context("verifying gateway")?;
        self.launch_lap("net-verify");
        let expects_tunnel = self.options.config.network.wireguard_conf.is_some()
            && self.options.config.network.enforce_vpn;
        if !capscode_net::verify_passed(&report, expects_tunnel) {
            bail!(
                "gateway verification failed closed: {}",
                report.details.join("; ")
            );
        }
        let workspace_volume = self
            .resources
            .volume
            .clone()
            .unwrap_or_else(|| workspace_volume(&self.options.session));
        let capsule_spec = capsule_spec_with(
            &self.options,
            &gateway,
            &workspace_volume,
            &gateway_ca,
            self.keeper_mode,
            self.workspace_mode,
        )?;
        assert_capsule_has_no_real_keys(
            &capsule_spec,
            &self.options.config,
            self.options.selected_credential.as_ref(),
            &self.options.extra_providers,
        )?;
        note(render_containment_panel(
            &report,
            expects_tunnel,
            Caps::stderr(),
        ));
        if self.workspace_mode == WorkspaceMode::Mount {
            note(format!(
                "workspace mode: mount — agent writes directly to {}; no copy-back review; auto-sync disabled",
                self.options.host_repo.display()
            ));
        }
        self.launch_lap("capsule-spec");
        let capsule = self
            .create_container_tracked(capsule_spec, ContainerRole::Capsule)
            .await
            .context("creating capsule")?;
        self.launch_lap("capsule-create");
        self.runtime
            .start(&capsule)
            .await
            .context("starting capsule supervisor")?;
        self.launch_lap("capsule-start");
        self.inject_tool_config(&capsule).await?;
        self.launch_lap("tool-config");
        // A box is ready only after every generated config is in place. Earlier
        // failures remove the containers while destroy() keeps the workspace volume.
        self.box_ready = true;
        Ok(())
    }

    async fn create_container_tracked(
        &mut self,
        spec: ContainerSpec,
        role: ContainerRole,
    ) -> AnyResult<ContainerId> {
        let runtime = Arc::clone(&self.runtime);
        self.pending_creates.push(PendingContainerCreate {
            role,
            task: tokio::spawn(async move { runtime.create_container(&spec).await }),
        });
        let index = self.pending_creates.len() - 1;
        let created = (&mut self.pending_creates[index].task).await;
        let pending = self.pending_creates.remove(index);
        match created.context("waiting for container creation")? {
            Ok(id) => {
                self.register_container(pending.role, id.clone());
                Ok(id)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn register_container(&mut self, role: ContainerRole, id: ContainerId) {
        match role {
            ContainerRole::Gateway => self.resources.gateway = Some(id),
            ContainerRole::Capsule => self.resources.capsule = Some(id),
        }
    }

    /// Write the provider keys and (in tunnel mode) the WireGuard config into the
    /// gateway secrets tmpfs, then a `ready` sentinel. Secrets travel over the
    /// exec stdin pipe — never argv, env, or host disk — and host-side buffers are
    /// zeroized after the write.
    async fn inject_gateway_secrets(&self, gateway: &ContainerId) -> AnyResult<()> {
        inject_gateway_secrets(
            self.runtime.as_ref(),
            gateway,
            &self.options.config.network,
            &self.options.selected_provider,
            self.options.selected_credential.as_ref(),
            self.options.selected_credential_source.as_deref(),
            &self.options.extra_providers,
        )
        .await
    }

    async fn inject_tool_config(&self, capsule: &ContainerId) -> AnyResult<()> {
        for (path, contents) in generated_tool_configs(&self.options)? {
            let output = self
                .runtime
                .exec(
                    capsule,
                    ExecSpec {
                        cmd: vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            INJECT_TOOL_CONFIG_SCRIPT.to_string(),
                            "sh".to_string(),
                            path.to_string(),
                        ],
                        stdin: Some(contents.into_bytes()),
                        // /home/agent is a fresh root-owned tmpfs mount. A tightly
                        // scoped root exec creates only the generated config path,
                        // then immediately hands ownership to the unprivileged tool.
                        user: Some("0".to_string()),
                        ..Default::default()
                    },
                )
                .await
                .with_context(|| format!("writing {path} in capsule"))?;
            if output.exit_code != 0 {
                bail!(
                    "writing {path} in capsule failed (exit {}): {}",
                    output.exit_code,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(())
    }

    /// Wait for the broker's public CA certificate before network verification.
    async fn read_gateway_ca(&self, gateway: &ContainerId) -> AnyResult<String> {
        read_gateway_ca(self.runtime.as_ref(), gateway).await
    }

    async fn commit_wip(&mut self) -> AnyResult<bool> {
        // Auto-sync WIP commits are capsule-local only under copy semantics.
        // Against a bind-mount they would land in the user's real .git while
        // they are away — never run them in mount mode.
        if self.workspace_mode == WorkspaceMode::Mount {
            return Ok(false);
        }
        let capsule = self
            .resources
            .capsule
            .as_ref()
            .context("capsule missing in Draining")?;
        let command = format!(
            "set -e; {}; git -C /workspace add -A; if ! git -C /workspace diff --cached --quiet; then git -C /workspace -c user.name=capsule -c user.email=capsule@localhost -c commit.gpgsign=false -c core.hooksPath=/dev/null commit --no-verify -m 'capsule: exit WIP'; fi",
            crate::session::git_operation_guard_shell()
        );
        let output = self
            .runtime
            .exec(
                capsule,
                ExecSpec {
                    cmd: vec!["sh".to_string(), "-c".to_string(), command],
                    user: Some("agent".to_string()),
                    ..Default::default()
                },
            )
            .await
            .context("auto-committing capsule WIP")?;
        if output.exit_code != 0 {
            bail!(
                "capsule WIP commit failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        self.wip_skipped =
            String::from_utf8_lossy(&output.stdout).contains("skipping the WIP commit");
        if self.wip_skipped {
            note(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }
        Ok(self.wip_skipped)
    }

    async fn read_keeper_tool_exit_status(&self, capsule: &ContainerId) -> AnyResult<Option<i32>> {
        let output = self
            .runtime
            .exec(
                capsule,
                ExecSpec {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        format!(
                            "if [ -r {KEEPER_TOOL_EXIT_STATUS_PATH} ]; then cat {KEEPER_TOOL_EXIT_STATUS_PATH}; rm -f {KEEPER_TOOL_EXIT_STATUS_PATH}; fi"
                        ),
                    ],
                    user: Some("agent".to_string()),
                    ..Default::default()
                },
            )
            .await
            .context("reading the session tool exit status")?;
        if output.exit_code != 0 {
            bail!(
                "reading the session tool exit status failed (exit {}): {}",
                output.exit_code,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        parse_keeper_tool_exit_status(&output.stdout)
    }

    async fn egress(&self) -> AnyResult<EgressReport> {
        let baseline = match self
            .resources
            .ingress_receipt
            .as_ref()
            .context("ingress receipt missing in Egressing")?
        {
            IngressReceipt::GitSnapshot { baseline } => baseline
                .as_deref()
                .map(str::parse::<git2::Oid>)
                .transpose()
                .context("invalid GitSnapshot baseline OID")?,
            IngressReceipt::Shadow { .. } => {
                bail!("host-repository egress is unavailable for a file sync session")
            }
        };
        self.git
            .egress(
                &EgressPlan {
                    host_repo: self.options.host_repo.clone(),
                    session: self.options.session.clone(),
                    capsule: self
                        .resources
                        .capsule
                        .clone()
                        .context("capsule missing in Egressing")?,
                    workspace_path: PathBuf::from("/workspace"),
                    baseline,
                    verbose: self.options.verbose,
                },
                self.runtime.as_ref(),
            )
            .await
            .context("egress")
    }

    async fn has_ignored_workspace_files(&self) -> AnyResult<bool> {
        let capsule = self
            .resources
            .capsule
            .as_ref()
            .context("capsule missing while checking ignored workspace files")?;
        let script = "tmp=$(mktemp) || exit 1; git -C /workspace ls-files --others --ignored --exclude-standard -z >\"$tmp\"; status=$?; if [ $status -ne 0 ]; then rm -f \"$tmp\"; exit $status; fi; if [ -s \"$tmp\" ]; then printf y; fi; rm -f \"$tmp\"";
        let output = self
            .runtime
            .exec(
                capsule,
                ExecSpec {
                    cmd: vec!["sh".into(), "-c".into(), script.into()],
                    user: Some("agent".into()),
                    ..Default::default()
                },
            )
            .await
            .context("checking for ignored workspace files")?;
        if output.exit_code != 0 {
            bail!(
                "checking ignored workspace files failed (exit {}): {}",
                output.exit_code,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout.contains(&b'y'))
    }

    fn save_as_generated_branch(&self) -> AnyResult<crate::session::branch::PublishedBranch> {
        crate::session::branch::publish_session_branch(
            &self.options.host_repo,
            &self.options.session,
        )?
        .context("saving capsule changes as a branch: no capsule ref was found")
    }

    async fn exit_menu(&mut self, mut report: EgressReport) -> AnyResult<()> {
        loop {
            let generated_branch = crate::session::branch::branch_candidate(
                &self.options.host_repo,
                &self.options.session,
            )?;
            let choices = handoff_choices(&generated_branch);
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("What next?")
                .items(&choices)
                .default(0)
                .interact()
                .context("reading exit menu")?;
            match selection {
                0 => {
                    let branch = self.save_as_generated_branch()?;
                    note(crate::session::branch::branch_hint(&branch));
                    note("Host working tree was not changed.");
                    return Ok(());
                }
                1 => {
                    let branch = self.save_as_generated_branch()?;
                    note(crate::session::branch::branch_hint(&branch));
                    self.git
                        .apply_to_worktree(
                            &self.options.host_repo,
                            &self.options.session,
                            report.base_sha,
                        )
                        .with_context(|| {
                            format!(
                                "applying capsule changes to this folder; recovery branch {} was preserved",
                                branch.name
                            )
                        })?;
                    note("Applied capsule changes to this folder.");
                    return Ok(());
                }
                2 => show_diff(
                    &self.options.host_repo,
                    &self.options.session,
                    report.base_sha,
                )?,
                3 => {
                    let capsule = self.resources.capsule.clone().context("capsule missing")?;
                    self.work_captured = false;
                    {
                        crate::ui::ensure_no_live_region()?;
                        let _guard = TerminalGuard::new();
                        self.runtime
                            .run_interactive(
                                &capsule,
                                ExecSpec {
                                    cmd: vec!["sh".to_string()],
                                    workdir: Some(PathBuf::from("/workspace")),
                                    user: Some("agent".to_string()),
                                    tty: true,
                                    ..Default::default()
                                },
                                AttachIo {
                                    stdin: capscode_runtime::cancellable_stdin()
                                        .context("preparing cancellable terminal input")?,
                                    stdout: Box::new(tokio::io::stdout()),
                                    stderr: Box::new(tokio::io::stderr()),
                                },
                            )
                            .await
                            .context("opening capsule shell")?;
                    }
                    self.commit_wip().await?;
                    report = self.egress().await?;
                    if report.files_changed == 0 {
                        let ignored = self.has_ignored_workspace_files().await?;
                        self.work_captured = !self.wip_skipped;
                        self.git
                            .discard(&self.options.host_repo, &self.options.session)
                            .context("discarding unchanged capsule refs")?;
                        if ignored {
                            note("No tracked capsule changes. The agent created only files ignored by `.gitignore`; they were not included in the handoff.");
                        } else {
                            note("No capsule changes.");
                        }
                        return Ok(());
                    }
                    self.work_captured = !self.wip_skipped;
                    print_summary(&report, self.options.verbose);
                }
                _ => {
                    if !Confirm::with_theme(&crate::ui::theme())
                        .with_prompt("Discard all capsule changes?")
                        .default(false)
                        .interact()
                        .context("reading discard confirmation")?
                    {
                        continue;
                    }
                    self.git
                        .discard(&self.options.host_repo, &self.options.session)
                        .context("discarding fetched refs")?;
                    return Ok(());
                }
            }
        }
    }

    async fn teardown(&mut self) -> AnyResult<()> {
        self.destroy().await
    }

    /// Leave capsule, gateway, and volume running, and retain the session state.
    async fn detach(&mut self) -> AnyResult<()> {
        if !self.silent_shutdown {
            note(format!(
                "detached; box still running; run `capscode sync {}` to recover its work.",
                self.options.session
            ));
            if let Some(error) = self.failure.as_ref() {
                note(format!("The session handoff did not finish: {error}"));
            }
        }
        Ok(())
    }

    /// Stop and remove capsule + gateway; remove volume when work was captured;
    /// remove the session state dir when appropriate. Idempotent via
    /// `self.resources.take()`.
    async fn destroy(&mut self) -> AnyResult<()> {
        let mut failures = Vec::new();
        for pending in std::mem::take(&mut self.pending_creates) {
            match pending.task.await {
                Ok(Ok(id)) => self.register_container(pending.role, id),
                Ok(Err(error)) => {
                    failures.push(format!("finish pending container creation: {error}"))
                }
                Err(error) => failures.push(format!("join pending container creation: {error}")),
            }
        }
        if let Some(capsule) = self.resources.capsule.take() {
            if let Err(error) = self
                .runtime
                .stop(&capsule, crate::boxes::SESSION_STOP_GRACE)
                .await
            {
                failures.push(format!("stop capsule: {error}"));
            }
            if let Err(error) = self.runtime.remove(&capsule, true).await {
                failures.push(format!("remove capsule: {error}"));
            }
        }
        if let Some(gateway) = self.resources.gateway.take() {
            if let Err(error) = self
                .runtime
                .stop(&gateway, crate::boxes::SESSION_STOP_GRACE)
                .await
            {
                failures.push(format!("stop gateway: {error}"));
            }
            if let Err(error) = self.runtime.remove(&gateway, true).await {
                failures.push(format!("remove gateway: {error}"));
            }
        }
        let mut volume_removed = true;
        if let Some(volume) = self.resources.volume.take() {
            if self.options.r#box.is_some() {
                volume_removed = false;
            } else if self.work_captured {
                if let Err(error) = self.runtime.remove_volume(&volume).await {
                    volume_removed = false;
                    failures.push(format!("remove volume: {error}"));
                }
            } else {
                volume_removed = false;
                if !self.silent_shutdown {
                    note(format!(
                        "Session work preserved in volume {} (state dir {}); recover with your container runtime's volume export tools before deleting it.",
                        volume.0,
                        session_state_dir(&self.options.session).display()
                    ));
                }
            }
        }
        if self.options.r#box.is_none()
            && self.work_captured
            && volume_removed
            && !self.preserve_state_dir
        {
            let state_dir = session_state_dir(&self.options.session);
            if state_dir.exists() {
                let safe_target = (|| -> std::io::Result<PathBuf> {
                    let sessions_dir = state_dir.parent().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "session state directory has no sessions parent",
                        )
                    })?;
                    let canonical_sessions = sessions_dir.canonicalize()?;
                    let canonical_target = state_dir.canonicalize()?;
                    if canonical_target.parent() != Some(canonical_sessions.as_path())
                        || canonical_target.file_name().and_then(|name| name.to_str())
                            != Some(self.options.session.as_str())
                    {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "session state target is outside its private sessions directory",
                        ));
                    }
                    Ok(canonical_target)
                })();
                match safe_target {
                    Ok(canonical_target) => {
                        if let Err(error) = std::fs::remove_dir_all(&canonical_target) {
                            failures.push(format!(
                                "remove session state {}: {error}",
                                canonical_target.display()
                            ));
                        }
                    }
                    Err(error) => failures.push(format!(
                        "refusing to remove session state {}: {error}",
                        state_dir.display()
                    )),
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!("teardown incomplete: {}", failures.join("; "))
        }
    }
}

fn validate_box_routes(options: &SessionOptions) -> AnyResult<()> {
    let Some(box_spec) = options.r#box.as_ref() else {
        return Ok(());
    };
    if box_spec.routed.is_empty() {
        bail!("a box needs at least one routed provider with a stored key; run `capscode key set <provider>`");
    }
    if options.selected_credential.is_none()
        || !box_spec
            .routed
            .iter()
            .any(|id| id == &options.selected_provider.id)
    {
        bail!("box primary provider must be routed and have a stored key");
    }
    let mut available = std::collections::HashSet::from([options.selected_provider.id.as_str()]);
    for (provider, _) in &options.extra_providers {
        if !box_spec.routed.iter().any(|id| id == &provider.id) {
            bail!(
                "provider '{}' has credentials but is not in the box routed set",
                provider.id
            );
        }
        available.insert(provider.id.as_str());
    }
    for id in &box_spec.routed {
        if !available.contains(id.as_str()) {
            bail!("provider '{id}' is listed as routed but has no stored key; run `capscode key set {id}`");
        }
    }
    Ok(())
}

/// Inject the gateway's high-value secrets in the same order used by a real
/// session: provider keys, optional WireGuard config, then the readiness
/// sentinel. Secret bytes are streamed over exec stdin and zeroized after
/// each write.
pub(crate) async fn inject_gateway_secrets(
    runtime: &dyn RuntimeAdapter,
    gateway: &ContainerId,
    network: &NetworkConfig,
    provider: &ProviderSpec,
    credential: Option<&ProviderCredential>,
    credential_source: Option<&str>,
    extra: &[(ProviderSpec, ProviderCredential)],
) -> AnyResult<()> {
    use secrecy::ExposeSecret;
    use zeroize::Zeroize;

    let dir = capscode_net::spec::SECRETS_DIR;

    // keys.env contains only the selected provider's credential. The provider
    // manifest tells the broker which environment-variable name to read.
    let mut keys_env = String::new();
    if let Some(cred) = credential {
        validate_injected_credential(provider, cred, credential_source)?;
        keys_env.push_str(manifest_key_env(provider, Some(cred)));
        keys_env.push('=');
        keys_env.push_str(cred.secret.expose_secret());
        keys_env.push('\n');
    }
    // Every extra provider's key, so a box can switch to it later. The broker
    // keeps only keys its manifest references, so an unrouted key is dropped.
    for (spec, cred) in extra {
        validate_injected_credential(spec, cred, None)?;
        keys_env.push_str(manifest_key_env(spec, Some(cred)));
        keys_env.push('=');
        keys_env.push_str(cred.secret.expose_secret());
        keys_env.push('\n');
    }
    if !keys_env.is_empty() {
        let bytes = keys_env.clone().into_bytes();
        keys_env.zeroize();
        write_gateway_secret(runtime, gateway, &format!("{dir}/keys.env"), bytes)
            .await
            .context("injecting provider keys into gateway tmpfs")?;
    }

    // wg0.conf — the validated WireGuard config (tunnel mode only).
    if let Some(mut text) = tunnel_wg_text(network)? {
        let bytes = text.clone().into_bytes();
        text.zeroize();
        write_gateway_secret(runtime, gateway, &format!("{dir}/wg0.conf"), bytes)
            .await
            .context("injecting WireGuard config into gateway tmpfs")?;
    }

    // ready sentinel — unblocks the gateway entrypoint. Written last so the
    // gateway never observes a partially-populated secrets dir.
    write_gateway_secret(runtime, gateway, &format!("{dir}/ready"), Vec::new())
        .await
        .context("writing gateway readiness sentinel")?;
    Ok(())
}

/// Write bytes to a gateway secret path using the same exec-stdin contract as
/// the session path. The target path is an argv value, never a shell word.
async fn write_gateway_secret(
    runtime: &dyn RuntimeAdapter,
    gateway: &ContainerId,
    path: &str,
    bytes: Vec<u8>,
) -> AnyResult<()> {
    // `$1` is the target path, passed as argv so it is never interpolated into
    // the shell word; `cat` copies stdin (the secret) into it.
    let output = runtime
        .exec(
            gateway,
            ExecSpec {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "umask 077; cat > \"$1\"".to_string(),
                    "sh".to_string(),
                    path.to_string(),
                ],
                stdin: Some(bytes),
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("exec writing gateway secret {path}"))?;
    if output.exit_code != 0 {
        bail!(
            "writing {path} into gateway failed (exit {}): {}",
            output.exit_code,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Read and re-validate the WireGuard config text for injection (tunnel mode).
/// Fail-closed: an unreadable or invalid file aborts the caller rather than
/// silently downgrading to containment-only.
fn tunnel_wg_text(network: &NetworkConfig) -> AnyResult<Option<String>> {
    let Some(path) = network.wireguard_conf.as_ref() else {
        return Ok(None);
    };
    if !network.enforce_vpn {
        return Ok(None);
    }
    use zeroize::Zeroize;

    let mut text = std::fs::read_to_string(path)
        .with_context(|| format!("reading WireGuard config {}", path.display()))?;
    let parsed = capscode_net::parse_wireguard_conf(&text);
    text.zeroize();
    let parsed = parsed.context("validating WireGuard config before injection")?;
    Ok(Some(parsed.to_wg_quick()))
}

/// Wait for the broker's public CA certificate before any network verification.
pub(crate) async fn read_gateway_ca(
    runtime: &dyn RuntimeAdapter,
    gateway: &ContainerId,
) -> AnyResult<String> {
    let output = runtime
        .exec(
            gateway,
            ExecSpec {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "i=0; while [ ! -s /etc/capscode/broker-ca.pem ] && [ $i -lt 30 ]; do sleep 1; i=$((i+1)); done; cat /etc/capscode/broker-ca.pem"
                        .to_string(),
                ],
                ..Default::default()
            },
        )
        .await
        .context("reading gateway broker CA certificate")?;
    if output.exit_code != 0 {
        bail!(
            "gateway broker did not publish its CA certificate within 30s; the broker \
             process failed to start (check the provider credential and providers manifest)"
        );
    }
    let certificate = String::from_utf8(output.stdout)
        .context("gateway broker CA certificate is not valid UTF-8")?;
    if !certificate.contains("-----BEGIN CERTIFICATE-----") || certificate.contains("PRIVATE KEY") {
        bail!("gateway broker returned an invalid public CA certificate");
    }
    Ok(certificate)
}

#[derive(Debug)]
struct ProviderRoute {
    vhost: String,
    authority: String,
    base_path: String,
}

impl ProviderRoute {
    fn capsule_url(&self) -> String {
        format!("https://{}:8443", self.vhost)
    }
}

#[derive(Serialize)]
struct ProviderManifestEntry {
    id: String,
    vhost: String,
    authority: String,
    base_path: String,
    api_style: &'static str,
    inject: &'static str,
    key_env: String,
}

#[derive(Serialize)]
struct CodexConfig {
    model_provider: String,
    model_providers: std::collections::BTreeMap<String, CodexModelProviderConfig>,
    features: CodexFeatures,
    check_for_update_on_startup: bool,
}

#[derive(Serialize)]
struct CodexModelProviderConfig {
    name: String,
    base_url: String,
    env_key: String,
    wire_api: &'static str,
    requires_openai_auth: bool,
}

#[derive(Serialize)]
struct CodexFeatures {
    shell_snapshot: bool,
}

fn provider_route(provider: &ProviderSpec) -> AnyResult<ProviderRoute> {
    if provider.id.is_empty()
        || !provider
            .id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!(
            "provider '{}' cannot be used in a gateway hostname; IDs must contain only lowercase letters, digits, and '-'",
            provider.id
        );
    }
    let rest = provider
        .base_url
        .strip_prefix("https://")
        .with_context(|| format!("provider '{}' base URL must use https", provider.id))?;
    if rest.contains(['?', '#', '@']) {
        bail!(
            "provider '{}' base URL must not contain credentials, a query, or a fragment",
            provider.id
        );
    }
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, ""), |(authority, path)| (authority, path));
    if authority.is_empty() || authority.contains(':') {
        bail!(
            "provider '{}' base URL must use a hostname with the default HTTPS port",
            provider.id
        );
    }
    Ok(ProviderRoute {
        vhost: format!("{}.broker.capscode.local", provider.id),
        authority: authority.to_string(),
        base_path: if path.is_empty() {
            String::new()
        } else {
            format!("/{path}")
        },
    })
}

fn manifest_key_env<'a>(
    provider: &'a ProviderSpec,
    credential: Option<&ProviderCredential>,
) -> &'a str {
    match credential.map(|credential| credential.kind) {
        Some(AuthKind::OAuth) => "CLAUDE_CODE_OAUTH_TOKEN",
        Some(AuthKind::CodexAccessToken) => CODEX_ACCESS_TOKEN_ENV,
        Some(AuthKind::ApiKey) | None => &provider.key_env,
    }
}

fn is_codex_chatgpt(credential: Option<&ProviderCredential>) -> bool {
    credential.is_some_and(|credential| credential.kind == AuthKind::CodexAccessToken)
}

fn session_routes(
    provider: &ProviderSpec,
    credential: Option<&ProviderCredential>,
) -> AnyResult<Vec<(String, ProviderRoute)>> {
    if is_codex_chatgpt(credential) {
        if provider.id != "openai" || provider.api_style != ApiStyle::OpenAI {
            bail!("Codex ChatGPT credentials require the bundled 'openai' provider");
        }
        return Ok(vec![(
            "openai-chatgpt".to_string(),
            ProviderRoute {
                vhost: CODEX_CHATGPT_VHOST.to_string(),
                authority: "chatgpt.com".to_string(),
                base_path: "/backend-api/codex".to_string(),
            },
        )]);
    }
    Ok(vec![(provider.id.clone(), provider_route(provider)?)])
}

pub(crate) fn broker_aliases(
    provider: &ProviderSpec,
    credential: Option<&ProviderCredential>,
) -> AnyResult<String> {
    Ok(session_routes(provider, credential)?
        .into_iter()
        .map(|(_, route)| route.vhost)
        .collect::<Vec<_>>()
        .join(" "))
}

/// Manifest for the selected provider plus every EXTRA provider the box should
/// be able to reach without a rebuild.
///
/// Devbox boxes are long-lived and the route table is create-time only (the
/// broker parses `CAPSCODE_PROVIDERS` once at startup), so a box that routed
/// only the launch provider could never switch. Routing every provider that has
/// a stored key at creation is what makes per-attach switching possible.
pub(crate) fn provider_manifest_json_multi(
    provider: &ProviderSpec,
    credential: Option<&ProviderCredential>,
    extra: &[(ProviderSpec, ProviderCredential)],
) -> AnyResult<String> {
    let mut json = provider_manifest_json(provider, credential)?;
    if extra.is_empty() {
        return Ok(json);
    }
    let mut entries: Vec<serde_json::Value> = serde_json::from_str(&json)?;
    let mut seen: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|entry| entry.get("vhost").and_then(|v| v.as_str()))
        .map(str::to_string)
        .collect();
    for (spec, cred) in extra {
        let more = provider_manifest_json(spec, Some(cred))?;
        for entry in serde_json::from_str::<Vec<serde_json::Value>>(&more)? {
            let vhost = entry
                .get("vhost")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if seen.insert(vhost) {
                entries.push(entry);
            }
        }
    }
    json = serde_json::to_string(&entries)?;
    Ok(json)
}

/// Broker vhost aliases for the selected provider plus every extra one.
pub(crate) fn broker_aliases_multi(
    provider: &ProviderSpec,
    credential: Option<&ProviderCredential>,
    extra: &[(ProviderSpec, ProviderCredential)],
) -> AnyResult<String> {
    let mut aliases = broker_aliases(provider, credential)?;
    for (spec, cred) in extra {
        let more = broker_aliases(spec, Some(cred))?;
        for alias in more.split_whitespace() {
            if !aliases.split_whitespace().any(|existing| existing == alias) {
                aliases.push(' ');
                aliases.push_str(alias);
            }
        }
    }
    Ok(aliases)
}

pub(crate) fn provider_manifest_json(
    provider: &ProviderSpec,
    credential: Option<&ProviderCredential>,
) -> AnyResult<String> {
    let inject = match credential.map(|credential| credential.kind) {
        Some(AuthKind::OAuth | AuthKind::CodexAccessToken) => "bearer",
        Some(AuthKind::ApiKey) | None => match provider.auth {
            AuthScheme::Bearer => "bearer",
            AuthScheme::XApiKey => "x-api-key",
        },
    };
    let entries: Vec<ProviderManifestEntry> = session_routes(provider, credential)?
        .into_iter()
        .map(|(id, route)| ProviderManifestEntry {
            id,
            vhost: route.vhost,
            authority: route.authority,
            base_path: route.base_path,
            api_style: match provider.api_style {
                ApiStyle::Anthropic => "anthropic",
                ApiStyle::OpenAI => "openai",
            },
            inject,
            key_env: manifest_key_env(provider, credential).to_string(),
        })
        .collect();
    serde_json::to_string(&entries).context("serializing CAPSCODE_PROVIDERS")
}

fn capsule_sentinel(
    tool_command: &str,
    provider: &ProviderSpec,
    credential: Option<&ProviderCredential>,
) -> (&'static str, &'static str) {
    match tool_command {
        "pi" => return (CAPSULE_PI_API_KEY_ENV, SENTINEL_API_KEY),
        "omp" => return (CAPSULE_OMP_API_KEY_ENV, SENTINEL_API_KEY),
        _ => {}
    }
    match credential.map(|credential| credential.kind) {
        Some(AuthKind::OAuth) => ("CLAUDE_CODE_OAUTH_TOKEN", SENTINEL_OAUTH_TOKEN),
        Some(AuthKind::CodexAccessToken) => (CAPSULE_CODEX_TOKEN_ENV, SENTINEL_CODEX_ACCESS_TOKEN),
        Some(AuthKind::ApiKey) | None => match provider.api_style {
            ApiStyle::Anthropic if tool_command == "claude" => {
                ("ANTHROPIC_AUTH_TOKEN", SENTINEL_API_KEY)
            }
            ApiStyle::Anthropic => ("ANTHROPIC_API_KEY", SENTINEL_API_KEY),
            ApiStyle::OpenAI => ("OPENAI_API_KEY", SENTINEL_API_KEY),
        },
    }
}

fn box_tool_environment(
    options: &SessionOptions,
    box_spec: &BoxSpec,
) -> AnyResult<Vec<(String, String)>> {
    let mut env = vec![
        ("CAPSCODE_BOX".to_string(), box_spec.name.clone()),
        ("DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
        ("OMP_SKIP_SETUP".to_string(), "1".to_string()),
    ];
    let provider_for = |tool: Tool| -> Option<(&ProviderSpec, Option<&ProviderCredential>)> {
        let id = box_spec.tool_defaults.get(&tool)?;
        if !box_spec.routed.iter().any(|routed| routed == id) {
            return None;
        }
        if options.selected_provider.id == *id {
            Some((
                &options.selected_provider,
                options.selected_credential.as_ref(),
            ))
        } else {
            options
                .extra_providers
                .iter()
                .find(|(provider, _)| provider.id == *id)
                .map(|(provider, credential)| (provider, Some(credential)))
        }
    };

    if let Some((provider, credential)) = provider_for(Tool::Claude) {
        let (name, value) = match credential.map(|credential| credential.kind) {
            Some(AuthKind::OAuth) => {
                env.retain(|(key, _)| key != "ANTHROPIC_AUTH_TOKEN");
                ("CLAUDE_CODE_OAUTH_TOKEN", SENTINEL_OAUTH_TOKEN)
            }
            _ => ("ANTHROPIC_AUTH_TOKEN", SENTINEL_API_KEY),
        };
        set_env_entry(&mut env, name, value);
        if provider.api_style == ApiStyle::Anthropic {
            env.push((
                "ANTHROPIC_BASE_URL".to_string(),
                provider_route(provider)?.capsule_url(),
            ));
        }
    }
    if let Some((provider, credential)) = provider_for(Tool::Codex) {
        let (name, value) =
            if credential.is_some_and(|credential| credential.kind == AuthKind::CodexAccessToken) {
                env.retain(|(key, _)| key != "OPENAI_API_KEY");
                (CAPSULE_CODEX_TOKEN_ENV, SENTINEL_CODEX_ACCESS_TOKEN)
            } else {
                ("OPENAI_API_KEY", SENTINEL_API_KEY)
            };
        set_env_entry(&mut env, name, value);
        if provider.api_style == ApiStyle::OpenAI {
            let route = session_routes(provider, credential)?
                .into_iter()
                .next()
                .context("Codex provider route is missing")?
                .1;
            env.push(("OPENAI_BASE_URL".to_string(), route.capsule_url()));
        }
    }
    if provider_for(Tool::Opencode).is_some() {
        set_env_entry(&mut env, "CAPSCODE_OPENCODE_API_KEY", SENTINEL_API_KEY);
    }
    if provider_for(Tool::Pi).is_some() {
        set_env_entry(&mut env, CAPSULE_PI_API_KEY_ENV, SENTINEL_API_KEY);
    }
    if provider_for(Tool::Omp).is_some() {
        set_env_entry(&mut env, CAPSULE_OMP_API_KEY_ENV, SENTINEL_API_KEY);
    }
    // The box shims pass these to `--provider`; they name the default
    // provider's entry in the generated pi/omp model catalogs.
    if let Some((provider, _)) = provider_for(Tool::Pi) {
        env.push((
            "CAPSCODE_PI_DEFAULT_PROVIDER".to_string(),
            catalog_provider_id(provider),
        ));
    }
    if let Some((provider, _)) = provider_for(Tool::Omp) {
        env.push((
            "CAPSCODE_OMP_DEFAULT_PROVIDER".to_string(),
            catalog_provider_id(provider),
        ));
    }
    if let Some((provider, _)) = provider_for(Tool::Pi) {
        if let Some(model) = provider
            .default_model
            .as_ref()
            .filter(|model| !model.trim().is_empty())
        {
            env.push(("CAPSCODE_PI_DEFAULT_MODEL".to_string(), model.clone()));
        }
    }
    if let Some((provider, _)) = provider_for(Tool::Omp) {
        if let Some(model) = provider
            .default_model
            .as_ref()
            .filter(|model| !model.trim().is_empty())
        {
            env.push(("CAPSCODE_OMP_DEFAULT_MODEL".to_string(), model.clone()));
        }
    }
    Ok(env)
}

fn set_env_entry(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    env.retain(|(existing, _)| existing != key);
    env.push((key.to_string(), value.to_string()));
}

fn tool_base_url_env(options: &SessionOptions) -> &'static str {
    match options.tool_command.as_str() {
        "claude" => "ANTHROPIC_BASE_URL",
        "codex" | "opencode" | "pi" | "omp" => match options.selected_provider.api_style {
            ApiStyle::Anthropic => "ANTHROPIC_BASE_URL",
            ApiStyle::OpenAI => "OPENAI_BASE_URL",
        },
        _ => "CAPSCODE_BROKER_BASE_URL",
    }
}

fn generated_tool_config(options: &SessionOptions) -> AnyResult<Option<(&'static str, String)>> {
    let routes = session_routes(
        &options.selected_provider,
        options.selected_credential.as_ref(),
    )?;
    let route = &routes[0].1;
    let (sentinel_var, _) = capsule_sentinel(
        &options.tool_command,
        &options.selected_provider,
        options.selected_credential.as_ref(),
    );
    match options.tool_command.as_str() {
        "opencode" => {
            let mut models = serde_json::Map::new();
            for id in options.selected_provider.model_ids() {
                models.insert(
                    id.clone(),
                    serde_json::json!({ "name": format!("CapsCode {id}") }),
                );
            }
            let mut providers = serde_json::Map::new();
            providers.insert(
                options.selected_provider.id.clone(),
                serde_json::json!({
                    "npm": match options.selected_provider.api_style {
                        ApiStyle::OpenAI => "@ai-sdk/openai-compatible",
                        ApiStyle::Anthropic => "@ai-sdk/anthropic",
                    },
                    "name": format!("CapsCode {}", options.selected_provider.id),
                    "models": models,
                    "options": {
                        "baseURL": route.capsule_url(),
                        "apiKey": format!("{{env:{sentinel_var}}}"),
                    }
                }),
            );
            let document = serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "enabled_providers": [&options.selected_provider.id],
                "provider": providers,
            });
            Ok(Some((
                "/home/agent/.config/opencode/opencode.json",
                serde_json::to_string_pretty(&document)?,
            )))
        }
        "pi" => {
            let mut providers = serde_json::Map::new();
            providers.insert(
                catalog_provider_id(&options.selected_provider),
                serde_json::json!({
                    "baseUrl": route.capsule_url(),
                    "apiKey": format!("${sentinel_var}"),
                    "api": match options.selected_provider.api_style {
                        ApiStyle::OpenAI => "openai-completions",
                        ApiStyle::Anthropic => "anthropic-messages",
                    },
                    "models": catalog_models(&options.selected_provider),
                }),
            );
            Ok(Some((
                "/home/agent/.pi/agent/models.json",
                serde_json::to_string_pretty(&serde_json::json!({
                    "providers": providers
                }))?,
            )))
        }
        "omp" => {
            // omp reads apiKey as an env-var NAME (no `$`), unlike pi's `"$VAR"`.
            let mut providers = serde_json::Map::new();
            providers.insert(
                catalog_provider_id(&options.selected_provider),
                serde_json::json!({
                    "baseUrl": route.capsule_url(),
                    "apiKey": sentinel_var,
                    "api": match options.selected_provider.api_style {
                        ApiStyle::OpenAI => "openai-completions",
                        ApiStyle::Anthropic => "anthropic-messages",
                    },
                    "models": catalog_models(&options.selected_provider),
                }),
            );
            Ok(Some((
                "/home/agent/.omp/agent/models.yml",
                serde_json::to_string_pretty(&serde_json::json!({
                    "providers": providers
                }))?,
            )))
        }
        "codex" if is_codex_chatgpt(options.selected_credential.as_ref()) => {
            let provider_id = "capscode-chatgpt".to_string();
            let mut model_providers = std::collections::BTreeMap::new();
            model_providers.insert(
                provider_id.clone(),
                CodexModelProviderConfig {
                    name: "CapsCode ChatGPT".to_string(),
                    base_url: route.capsule_url(),
                    env_key: CAPSULE_CODEX_TOKEN_ENV.to_string(),
                    wire_api: "responses",
                    requires_openai_auth: false,
                },
            );
            Ok(Some((
                "/home/agent/.codex/config.toml",
                toml::to_string(&CodexConfig {
                    model_provider: provider_id,
                    model_providers,
                    // Capsules already start from a normalized environment and
                    // last for one run, so persisting shell snapshots adds no
                    // value and can stumble over multiline injected CA data.
                    features: CodexFeatures {
                        shell_snapshot: false,
                    },
                    check_for_update_on_startup: false,
                })?,
            )))
        }
        "claude" => {
            // The fresh capsule home has no ~/.claude.json, so Claude Code shows its
            // interactive "Select login method" onboarding instead of using the
            // injected CLAUDE_CODE_OAUTH_TOKEN / ANTHROPIC_API_KEY sentinel + broker
            // ANTHROPIC_BASE_URL. Seed a completed-onboarding marker so it proceeds
            // headlessly and trusts the injected credential.
            let document = serde_json::json!({
                "hasCompletedOnboarding": true,
            });
            Ok(Some((
                "/home/agent/.claude.json",
                serde_json::to_string_pretty(&document)?,
            )))
        }
        "codex" => Ok(None),
        tool => bail!("unsupported capsule tool '{tool}'"),
    }
}

/// Config files created in a box. The normal one-shot path deliberately keeps
/// generating only the selected tool's config.
fn routed_providers_for<'a>(
    options: &'a SessionOptions,
    box_spec: &BoxSpec,
    tool: Tool,
) -> Vec<(&'a ProviderSpec, &'a ProviderCredential)> {
    let mut routed: Vec<(&ProviderSpec, &ProviderCredential)> = options
        .extra_providers
        .iter()
        .filter(|(provider, _)| {
            box_spec.routed.iter().any(|id| id == &provider.id) && provider.supports_tool(tool)
        })
        .map(|(provider, credential)| (provider, credential))
        .collect();
    if box_spec
        .routed
        .iter()
        .any(|id| id == &options.selected_provider.id)
        && options.selected_provider.supports_tool(tool)
    {
        if let Some(credential) = options.selected_credential.as_ref() {
            routed.push((&options.selected_provider, credential));
        }
    }
    routed
}

pub(crate) fn generated_tool_configs(
    options: &SessionOptions,
) -> AnyResult<Vec<(&'static str, String)>> {
    let Some(box_spec) = options.r#box.as_ref() else {
        return Ok(generated_tool_config(options)?.into_iter().collect());
    };
    let mut files = Vec::new();
    let routed_for = |tool| routed_providers_for(options, box_spec, tool);

    files.push((
        "/home/agent/.claude.json",
        serde_json::to_string_pretty(&serde_json::json!({"hasCompletedOnboarding": true}))?,
    ));

    let codex_providers = routed_for(Tool::Codex);
    let chosen_codex = box_spec
        .tool_defaults
        .get(&Tool::Codex)
        .and_then(|id| {
            codex_providers
                .iter()
                .find(|(provider, _)| provider.id == *id)
        })
        .or_else(|| codex_providers.first());
    let mut codex_models = std::collections::BTreeMap::new();
    for (provider, credential) in &codex_providers {
        let chatgpt = credential.kind == AuthKind::CodexAccessToken;
        let name = if chatgpt {
            "capscode-openai-chatgpt".to_string()
        } else {
            format!("capscode-{}", provider.id)
        };
        let route = session_routes(provider, Some(credential))?
            .into_iter()
            .next()
            .context("Codex provider route is missing")?
            .1;
        codex_models.insert(
            name.clone(),
            CodexModelProviderConfig {
                name: format!("CapsCode {}", provider.id),
                base_url: route.capsule_url(),
                env_key: if chatgpt {
                    CAPSULE_CODEX_TOKEN_ENV.to_string()
                } else {
                    "OPENAI_API_KEY".to_string()
                },
                wire_api: "responses",
                requires_openai_auth: false,
            },
        );
    }
    let codex_config = CodexConfig {
        model_provider: chosen_codex
            .map(|(provider, credential)| {
                if credential.kind == AuthKind::CodexAccessToken {
                    "capscode-openai-chatgpt".to_string()
                } else {
                    format!("capscode-{}", provider.id)
                }
            })
            .unwrap_or_else(|| "capscode-unrouted".to_string()),
        model_providers: codex_models,
        features: CodexFeatures {
            shell_snapshot: false,
        },
        check_for_update_on_startup: false,
    };
    files.push((
        "/home/agent/.codex/config.toml",
        toml::to_string(&codex_config)?,
    ));

    let mut open_providers = serde_json::Map::new();
    for (provider, _) in routed_for(Tool::Opencode) {
        let models = provider
            .model_ids()
            .into_iter()
            .map(|id| {
                (
                    id.clone(),
                    serde_json::json!({"name": format!("CapsCode {id}")}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        open_providers.insert(
            provider.id.clone(),
            serde_json::json!({
                "npm": match provider.api_style { ApiStyle::OpenAI => "@ai-sdk/openai-compatible", ApiStyle::Anthropic => "@ai-sdk/anthropic" },
                "name": format!("CapsCode {}", provider.id),
                "models": models,
                "options": { "baseURL": provider_route(provider)?.capsule_url(), "apiKey": "{env:CAPSCODE_OPENCODE_API_KEY}" }
            }),
        );
    }
    files.push((
        "/home/agent/.config/opencode/opencode.json",
        serde_json::to_string_pretty(&serde_json::json!({
            "$schema": "https://opencode.ai/config.json",
            "enabled_providers": open_providers.keys().collect::<Vec<_>>(),
            "provider": open_providers,
            "autoupdate": false
        }))?,
    ));

    let pi_providers = catalog_configs(&routed_for(Tool::Pi), CAPSULE_PI_API_KEY_ENV, true)?;
    files.push((
        "/home/agent/.pi/agent/models.json",
        serde_json::to_string_pretty(&serde_json::json!({"providers": pi_providers}))?,
    ));
    let omp_providers = catalog_configs(&routed_for(Tool::Omp), CAPSULE_OMP_API_KEY_ENV, false)?;
    files.push((
        "/home/agent/.omp/agent/models.yml",
        serde_json::to_string_pretty(&serde_json::json!({"providers": omp_providers}))?,
    ));
    Ok(files)
}

fn catalog_configs(
    providers: &[(&ProviderSpec, &ProviderCredential)],
    key_env: &str,
    dollar_prefix: bool,
) -> AnyResult<serde_json::Map<String, serde_json::Value>> {
    let mut values = serde_json::Map::new();
    for (provider, _) in providers {
        values.insert(
            catalog_provider_id(provider),
            serde_json::json!({
                "baseUrl": provider_route(provider)?.capsule_url(),
                "apiKey": if dollar_prefix { format!("${key_env}") } else { key_env.to_string() },
                "api": match provider.api_style { ApiStyle::OpenAI => "openai-completions", ApiStyle::Anthropic => "anthropic-messages" },
                "models": catalog_models(provider),
            }),
        );
    }
    Ok(values)
}

fn tool_command(options: &SessionOptions) -> AnyResult<Vec<String>> {
    match options.tool_command.as_str() {
        "pi" => return pi_command(options),
        "omp" => return omp_command(options),
        _ => {}
    }
    let mut command = vec![options.tool_command.clone()];
    command.extend(options.tool_args.clone());
    Ok(command)
}

fn pi_command(options: &SessionOptions) -> AnyResult<Vec<String>> {
    for forbidden in ["--provider", "--api-key"] {
        if has_long_option(&options.tool_args, forbidden) {
            bail!(
                "Pi argument '{forbidden}' is controlled by CapsCode so credentials cannot bypass the selected broker route"
            );
        }
    }
    let default_model = options
        .selected_provider
        .default_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .with_context(|| {
            format!(
                "provider '{}' has no default_model; add one to providers.yaml before using Pi",
                options.selected_provider.id
            )
        })?;

    let mut command = vec![
        "pi".to_string(),
        "--provider".to_string(),
        catalog_provider_id(&options.selected_provider),
    ];
    if !has_long_option(&options.tool_args, "--model") {
        command.extend(["--model".to_string(), default_model.to_string()]);
    }
    command.extend(options.tool_args.clone());
    Ok(command)
}

fn omp_command(options: &SessionOptions) -> AnyResult<Vec<String>> {
    // `--profile` relocates omp's whole config root to ~/.omp/profiles/<name>/agent,
    // where the generated models.yml does not exist — the pinned broker route would
    // simply not be found.
    for forbidden in ["--provider", "--api-key", "--profile"] {
        if has_long_option(&options.tool_args, forbidden) {
            bail!(
                "Omp argument '{forbidden}' is controlled by CapsCode so credentials cannot bypass the selected broker route"
            );
        }
    }
    let default_model = options
        .selected_provider
        .default_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .with_context(|| {
            format!(
                "provider '{}' has no default_model; add one to providers.yaml before using Omp",
                options.selected_provider.id
            )
        })?;

    // Prefer --model=provider/model over legacy --provider.
    let mut command = vec!["omp".to_string()];
    if !has_long_option(&options.tool_args, "--model") {
        command.extend([
            "--model".to_string(),
            format!(
                "{}/{}",
                catalog_provider_id(&options.selected_provider),
                default_model
            ),
        ]);
    }
    command.extend(options.tool_args.clone());
    Ok(command)
}

fn catalog_provider_id(provider: &ProviderSpec) -> String {
    format!("capscode-{}", provider.id)
}

fn catalog_models(provider: &ProviderSpec) -> Vec<serde_json::Value> {
    provider
        .model_ids()
        .iter()
        .map(|id| {
            let mut model = serde_json::json!({
                "id": id,
                "name": format!("CapsCode {id}"),
            });
            if id == "k3" {
                model["contextWindow"] = serde_json::json!(1_048_576);
            }
            model
        })
        .collect()
}

fn has_long_option(args: &[String], option: &str) -> bool {
    args.iter()
        .any(|arg| arg == option || arg.starts_with(&format!("{option}=")))
}

pub(crate) fn set_container_env(spec: &mut ContainerSpec, name: &str, value: String) {
    spec.env.retain(|(key, _)| key != name);
    spec.env.push((name.to_string(), value));
}

/// Capabilities re-added to the capsule after `cap_drop ALL`. They are held only by the root
/// init and the entrypoint; `setpriv` clears them when it drops to `agent`, and
/// no-new-privileges keeps the agent from regaining them. A box also needs KILL: its init
/// (root) must forward `docker stop`'s SIGTERM to the keeper, which runs as `agent`, and
/// signalling another user's process requires that capability. Without it the keeper never
/// gets a graceful shutdown and running agents are killed without warning.
fn capsule_cap_add(keeper_mode: bool) -> Vec<String> {
    let mut caps: Vec<String> = ["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETUID", "SETGID"]
        .into_iter()
        .map(String::from)
        .collect();
    if keeper_mode {
        caps.push("KILL".into());
    }
    caps
}

fn box_labels(options: &SessionOptions) -> Vec<(String, String)> {
    let Some(box_spec) = options.r#box.as_ref() else {
        return Vec::new();
    };
    vec![
        (LABEL_BOX.to_string(), box_spec.name.clone()),
        (LABEL_BOX_ID.to_string(), box_spec.id.clone()),
        (LABEL_PROJECT.to_string(), box_spec.project.clone()),
        (
            LABEL_PROJECT_HASH.to_string(),
            box_spec.project_hash.clone(),
        ),
        (LABEL_PROVIDERS.to_string(), box_spec.routed.join(",")),
        (LABEL_TOOLCHAIN.to_string(), box_spec.toolchain.clone()),
    ]
}

pub(crate) fn workspace_volume_labels(options: &SessionOptions) -> Vec<(String, String)> {
    let mut labels = vec![
        (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
        (LABEL_SESSION.to_string(), options.session.to_string()),
        (LABEL_ROLE.to_string(), ROLE_CAPSULE.to_string()),
        (LABEL_CREATED.to_string(), now_label()),
    ];
    labels.extend(box_labels(options));
    labels
}

pub(crate) fn cache_volume_labels(options: &SessionOptions) -> Vec<(String, String)> {
    let mut labels = workspace_volume_labels(options);
    if let Some((_, role)) = labels.iter_mut().find(|(key, _)| key == LABEL_ROLE) {
        *role = ROLE_CACHE.to_string();
    }
    labels.sort_by(|left, right| left.0.cmp(&right.0));
    labels
}

/// Written by tool-launcher.sh so the tmux shell cannot mask the tool status.
pub(crate) const KEEPER_TOOL_EXIT_STATUS_PATH: &str = "/run/capscode-keeper/tool-exit-status";

pub(crate) fn parse_keeper_tool_exit_status(bytes: &[u8]) -> AnyResult<Option<i32>> {
    let status = std::str::from_utf8(bytes)
        .context("session tool exit status is not valid UTF-8")?
        .trim();
    if status.is_empty() {
        return Ok(None);
    }
    let status = status
        .parse::<i32>()
        .context("session tool exit status is not an integer")?;
    if !(0..=255).contains(&status) {
        bail!("session tool exit status is outside the valid range 0..255");
    }
    Ok(Some(status))
}

/// Default size for the capsule `/home/agent` tmpfs (MiB). Override with
/// `CAPSCODE_HOME_TMPFS_MIB`. Docker's unbounded default is half of host RAM.
const DEFAULT_HOME_TMPFS_MIB: u64 = 512;
/// Default size for the capsule `/tmp` tmpfs (MiB). Override with
/// `CAPSCODE_TMP_TMPFS_MIB` for larger runtime caches.
const DEFAULT_TMP_TMPFS_MIB: u64 = 1024;
/// Default size for `/dev/shm` (MiB). Override with `CAPSCODE_SHM_TMPFS_MIB`.
/// Docker's 64 MiB default crashes Chromium on non-trivial pages.
const DEFAULT_SHM_TMPFS_MIB: u64 = 256;
/// Both npm, Bun, and XDG caches use this root so a future cache mount changes
/// the three paths together.
const CAPSULE_CACHE_ROOT: &str = "/tmp/.cache";

/// Parse a positive MiB size from an optional env raw value, or return `default`
/// when unset. `var` is only used in error messages.
fn parse_tmpfs_mib(raw: Option<&str>, var: &str, default: u64) -> AnyResult<u64> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value: u64 = raw
        .parse()
        .map_err(|_| anyhow!("invalid {var}={raw:?}: expected a positive integer (MiB)"))?;
    if value == 0 {
        bail!("invalid {var}={raw:?}: size must be greater than 0");
    }
    Ok(value)
}

/// Read a positive MiB size from process env `var`, or return `default` when unset.
fn tmpfs_mib_from_env(var: &str, default: u64) -> AnyResult<u64> {
    match std::env::var(var) {
        Err(std::env::VarError::NotPresent) => parse_tmpfs_mib(None, var, default),
        Err(err) => bail!("reading {var}: {err}"),
        Ok(raw) => parse_tmpfs_mib(Some(raw.as_str()), var, default),
    }
}

#[cfg(test)]
fn capsule_spec(
    options: &SessionOptions,
    gateway: &ContainerId,
    volume: &VolumeId,
    ca_cert_pem: &str,
) -> AnyResult<ContainerSpec> {
    capsule_spec_with(
        options,
        gateway,
        volume,
        ca_cert_pem,
        options.r#box.is_some(),
        options.config.workspace.mode,
    )
}

/// `capsule_spec` with the devbox decision passed in, so tests exercise both
/// modes without mutating process env (which races across the test threads).
fn capsule_spec_with(
    options: &SessionOptions,
    gateway: &ContainerId,
    volume: &VolumeId,
    ca_cert_pem: &str,
    keeper_mode: bool,
    workspace_mode: WorkspaceMode,
) -> AnyResult<ContainerSpec> {
    let routes = session_routes(
        &options.selected_provider,
        options.selected_credential.as_ref(),
    )?;
    let route = &routes[0].1;
    let (sentinel_var, sentinel_value) = capsule_sentinel(
        &options.tool_command,
        &options.selected_provider,
        options.selected_credential.as_ref(),
    );
    let mut env = vec![
        ("TZ".to_string(), "UTC".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("HISTFILE".to_string(), "/dev/null".to_string()),
        ("CAPSCODE_CA_CERT_PEM".to_string(), ca_cert_pem.to_string()),
        (
            "CAPSCODE_WORKSPACE_MODE".to_string(),
            workspace_mode.as_str().to_string(),
        ),
        (
            "CAPSCODE_BROKER_ALIASES".to_string(),
            routes
                .iter()
                .map(|(_, route)| route.vhost.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        (sentinel_var.to_string(), sentinel_value.to_string()),
        (
            "NODE_EXTRA_CA_CERTS".to_string(),
            "/home/agent/.capscode/ca.pem".to_string(),
        ),
        (
            "SSL_CERT_FILE".to_string(),
            "/home/agent/.capscode/ca.pem".to_string(),
        ),
        (
            "REQUESTS_CA_BUNDLE".to_string(),
            "/home/agent/.capscode/ca.pem".to_string(),
        ),
        (
            "CURL_CA_BUNDLE".to_string(),
            "/home/agent/.capscode/ca.pem".to_string(),
        ),
        (
            "GIT_SSL_CAINFO".to_string(),
            "/home/agent/.capscode/ca.pem".to_string(),
        ),
        (
            "GIT_CONFIG_GLOBAL".to_string(),
            "/home/agent/.gitconfig".to_string(),
        ),
        // The workspace volume is seeded by the ingress helper (root-owned), but
        // git runs in the capsule as the unprivileged `agent` user, so git's
        // ownership check would reject it. Trust /workspace for every git process
        // (the tool's own git, the exit WIP commit, and the egress upload-pack) via
        // GIT_CONFIG_* env, which all execs inherit from the container.
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), "safe.directory".to_string()),
        ("GIT_CONFIG_VALUE_0".to_string(), "/workspace".to_string()),
    ];

    if let Some(box_spec) = options.r#box.as_ref() {
        env.retain(|(key, _)| {
            !matches!(
                key.as_str(),
                "ANTHROPIC_AUTH_TOKEN"
                    | "ANTHROPIC_API_KEY"
                    | "CLAUDE_CODE_OAUTH_TOKEN"
                    | "OPENAI_API_KEY"
                    | CAPSULE_CODEX_TOKEN_ENV
                    | "CAPSCODE_OPENCODE_API_KEY"
                    | CAPSULE_PI_API_KEY_ENV
                    | CAPSULE_OMP_API_KEY_ENV
            ) && key != "ANTHROPIC_BASE_URL"
                && key != "OPENAI_BASE_URL"
        });
        env.extend(box_tool_environment(options, box_spec)?);
    } else {
        match options.tool_command.as_str() {
            "claude" | "codex" | "opencode" | "pi" => {
                env.push((tool_base_url_env(options).to_string(), route.capsule_url()))
            }
            "omp" => env.push(("OMP_SKIP_SETUP".to_string(), "1".to_string())),
            tool => bail!("unsupported capsule tool '{tool}'"),
        }

        if options.tool_command == "omp" {
            env.push((tool_base_url_env(options).to_string(), route.capsule_url()));
        }
    }

    if let Some(resolvers) = tunnel_resolvers(&options.config)? {
        env.push(("CAPSCODE_TUNNEL_DNS".to_string(), resolvers));
    }

    let cache_root = if options
        .r#box
        .as_ref()
        .is_some_and(|box_spec| box_spec.cache_volume)
    {
        "/cache"
    } else {
        CAPSULE_CACHE_ROOT
    };
    env.push(("XDG_CACHE_HOME".to_string(), cache_root.to_string()));
    env.push(("npm_config_cache".to_string(), format!("{cache_root}/npm")));
    env.push((
        "BUN_INSTALL_CACHE_DIR".to_string(),
        format!("{cache_root}/bun"),
    ));
    env.push(("OPENCODE_DISABLE_AUTOUPDATE".to_string(), "1".to_string()));
    if !options.config.capsule.opencode_lsp_download {
        env.push(("OPENCODE_DISABLE_LSP_DOWNLOAD".to_string(), "1".to_string()));
    }

    if keeper_mode
        && options.r#box.as_ref().is_some_and(|box_spec| {
            !routed_providers_for(options, box_spec, Tool::Codex).is_empty()
        })
    {
        // Keep the shared Codex daemon alive for every pane in a box. The
        // one-shot path still lets Codex start its own daemon with the TUI.
        env.push(("CAPSCODE_CODEX_DAEMON".to_string(), "1".to_string()));
    }

    if options.config.network.telemetry_sinkhole {
        env.push((
            "CAPSCODE_SINKHOLE_HOSTS".to_string(),
            capscode_net::sinkhole::TELEMETRY_HOSTS.join(" "),
        ));
    }

    // The entrypoint uses this generated allowlist for both the keeper server
    // and the classic command path. Keep mode-specific keeper controls out of
    // it so the tool environment is identical in both modes.
    let tool_env_keys = env
        .iter()
        .filter(|(key, _)| key != "CAPSCODE_CODEX_DAEMON")
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    env.push(("CAPSCODE_TOOL_ENV_KEYS".to_string(), tool_env_keys));

    if keeper_mode {
        // Read by images/entrypoint.sh: prepare the keeper socket dir, then exec
        // the tmux server as the same unprivileged `agent` uid used by its pane.
        env.push(("CAPSCODE_KEEPER".to_string(), "1".to_string()));

        // In mount mode /workspace IS the user's directory (Mount::Bind below),
        // so the keeper's periodic WIP commit would land in their real .git every
        // AUTOSYNC_SECS while they are away — or, for a non-git folder, create a
        // .capscode-autosync.git inside it. The entry banner promises that
        // auto-sync is disabled here, so turn the loop off too.
        if workspace_mode == WorkspaceMode::Mount {
            env.push(("KEEPER_AUTOSYNC".to_string(), "0".to_string()));
        }
    }

    let home_tmpfs_mib = tmpfs_mib_from_env("CAPSCODE_HOME_TMPFS_MIB", DEFAULT_HOME_TMPFS_MIB)?;
    let tmp_tmpfs_mib = tmpfs_mib_from_env("CAPSCODE_TMP_TMPFS_MIB", DEFAULT_TMP_TMPFS_MIB)?;
    let shm_tmpfs_mib = tmpfs_mib_from_env("CAPSCODE_SHM_TMPFS_MIB", DEFAULT_SHM_TMPFS_MIB)?;

    let mut spec = ContainerSpec {
        image: options.capsule_image.clone(),
        hostname: "workspace".to_string(),
        env,
        // The entrypoint initializes trust/identity, then remains as a harmless
        // supervisor. The attached tool is started exactly once by run_interactive.
        cmd: Some(vec!["sleep".to_string(), "infinity".to_string()]),
        entrypoint: None,
        mounts: vec![
            // Mount mode bind-mounts the host directory: the agent edits your
            // real files, so ingress/egress and the copy-back review are no-ops
            // by construction. Opt-in only (config key + --mount flag + entry
            // banner) — never wired silently.
            if workspace_mode == WorkspaceMode::Mount {
                Mount::Bind {
                    source: options.host_repo.clone(),
                    target: PathBuf::from("/workspace"),
                    read_only: false,
                }
            } else {
                Mount::Volume {
                    id: volume.clone(),
                    target: PathBuf::from("/workspace"),
                    read_only: false,
                }
            },
            Mount::Tmpfs {
                target: PathBuf::from("/tmp"),
                size_mb: Some(tmp_tmpfs_mib),
                exec: true,
            },
            // Explicit size: engine's bare tmpfs_home auto-push inherits Docker's
            // half-of-RAM default. Listing /home/agent here also skips that push.
            Mount::Tmpfs {
                target: PathBuf::from("/home/agent"),
                size_mb: Some(home_tmpfs_mib),
                exec: false,
            },
            // Docker's 64 MiB /dev/shm crashes Chromium on non-trivial pages.
            Mount::Tmpfs {
                target: PathBuf::from("/dev/shm"),
                size_mb: Some(shm_tmpfs_mib),
                exec: false,
            },
            // Keeper socket directory. It MUST be a writable mount: the capsule
            // runs read_only_rootfs, so the directory baked into the image is
            // read-only at runtime and the tmux server could not create its
            // socket. The entrypoint recreates it 0700-owned by agent; the
            // server and panes share that uid and can both reach the socket.
        ],
        network: NetworkMode::SharedWith(gateway.clone()),
        published_ports: Vec::new(),
        // cap_drop ALL then re-add only what the root entrypoint needs to
        // install the CA + `setpriv` down to `agent`; no-new-privileges stops any re-escalation.
        cap_add: capsule_cap_add(keeper_mode),
        cap_drop: vec!["ALL".into()],
        no_new_privileges: true,
        devices: Vec::new(),
        sysctls: Vec::new(),
        labels: vec![
            (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
            (LABEL_SESSION.to_string(), options.session.to_string()),
            (LABEL_ROLE.to_string(), ROLE_CAPSULE.to_string()),
            (
                LABEL_WORKSPACE_MODE.to_string(),
                workspace_mode.as_str().to_string(),
            ),
            (LABEL_CREATED.to_string(), now_label()),
        ],
        tmpfs_home: true,
        read_only_rootfs: true,
        // Docker's init (tini) reaps orphans and forwards signals for the
        // long-lived keeper. It stays off on the classic path to preserve that
        // path's existing container stop behavior; its interactive tool starts
        // later as an exec process.
        init: keeper_mode,
        pids_limit: options.config.capsule.pids_limit(),
        memory_bytes: options.config.capsule.memory_bytes(),
    };

    if options
        .r#box
        .as_ref()
        .is_some_and(|box_spec| box_spec.cache_volume)
    {
        spec.mounts.push(Mount::Volume {
            id: cache_volume(&options.session),
            target: PathBuf::from("/cache"),
            read_only: false,
        });
    }

    if keeper_mode {
        // The session socket directory must be a WRITABLE mount. The capsule
        // runs read_only_rootfs, so the /run/capscode-keeper baked into the
        // image is read-only at runtime and the tmux server could not create its
        // socket at all. The entrypoint recreates it 0700-owned by agent while
        // still root. The agent shares the keeper uid and can reach the socket;
        // devboxes have the same privacy boundary as the Docker socket.
        spec.mounts.push(Mount::Tmpfs {
            target: PathBuf::from("/run/capscode-keeper"),
            size_mb: Some(1),
            exec: false,
        });
    }

    spec.labels.extend(box_labels(options));
    Ok(spec)
}

fn assert_capsule_has_no_real_keys(
    spec: &ContainerSpec,
    config: &Config,
    selected_credential: Option<&ProviderCredential>,
    extra_providers: &[(ProviderSpec, ProviderCredential)],
) -> AnyResult<()> {
    use secrecy::ExposeSecret;
    for (_, value) in &spec.env {
        for secret in config.broker.keys.secrets() {
            if value == secret.expose_secret() {
                bail!(
                    "refusing to create capsule: provider credential crossed the gateway boundary"
                );
            }
        }
        if selected_credential.is_some_and(|credential| value == credential.secret.expose_secret())
        {
            bail!("refusing to create capsule: selected provider credential crossed the gateway boundary");
        }
        if extra_providers
            .iter()
            .any(|(_, credential)| value == credential.secret.expose_secret())
        {
            bail!("refusing to create capsule: routed provider credential crossed the gateway boundary");
        }
    }
    Ok(())
}

fn tunnel_resolvers(config: &Config) -> AnyResult<Option<String>> {
    let Some(path) = config.network.wireguard_conf.as_ref() else {
        return Ok(None);
    };
    if !config.network.enforce_vpn {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading WireGuard config {}", path.display()))?;
    let parsed = capscode_net::parse_wireguard_conf(&text)?;
    Ok(Some(parsed.tunnel_resolvers().join(" ")))
}

async fn ensure_image(
    runtime: &dyn RuntimeAdapter,
    image: &ImageRef,
    policy: PullPolicy,
    may_pull: bool,
    observer: &dyn LaunchObserver,
) -> AnyResult<()> {
    // Always inspect before deciding whether a pull is permitted. Docker's
    // `run` defaults to pulling, so this is the gate that keeps local-build
    // images from ever being fetched from a registry.
    let present = runtime.image_present(image).await?;
    let local_build = capscode_runtime::is_capscode_image(image);
    if local_build && matches!(policy, PullPolicy::Always) {
        bail!("pull policy 'always' is not allowed for local-build image {image}");
    }
    if local_build && !present {
        bail!(
            "image {image} is not built. Build it with: {}",
            crate::build_context::image_build_hint()
        );
    }
    if !may_pull {
        if !present {
            bail!("required image {image} is missing; only the digest-pinned seeder image may be pulled");
        }
        if matches!(policy, PullPolicy::Always) {
            bail!("pull policy 'always' may only pull the digest-pinned seeder image");
        }
        return Ok(());
    }

    let pull = matches!(policy, PullPolicy::Always)
        || (!present && matches!(policy, PullPolicy::IfMissing));
    if !present && matches!(policy, PullPolicy::Never) {
        bail!("required image {image} is missing and pull policy is never");
    }
    if !pull {
        return Ok(());
    }

    let mut on = |event: capscode_core::PullProgress| {
        observer.message("images", &format!("{} {}", image.0, event.status));
    };
    runtime
        .pull_image(image, &mut on)
        .await
        .with_context(|| format!("pulling required image {image}"))
}

fn render_launch_elapsed(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else if elapsed < Duration::from_secs(60) {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    }
}

fn render_containment_panel(report: &NetVerifyReport, expects_tunnel: bool, caps: Caps) -> String {
    const SECTION_WIDTH: usize = 14;
    const LABEL_WIDTH: usize = 10;

    let mut rows = Vec::new();
    let mut push = |section: &str, label: &str, value: String| {
        rows.push(format!(
            "{section:<SECTION_WIDTH$}{label:<LABEL_WIDTH$}{value}"
        ));
    };

    let rfc1918 = if report.rfc1918_blocked {
        crate::ui::styled(Status::Ok, "RFC1918 10.255.255.1:80 dropped", caps)
    } else {
        crate::ui::styled(
            Status::Fail,
            "RFC1918 10.255.255.1:80 not verified — failed",
            caps,
        )
    };
    push("containment", "probes", rfc1918);

    for (name, outcome) in &report.host_gateway {
        push(
            "",
            "",
            render_probe_outcome(&format!("host-gateway {name} TCP/22"), outcome, caps),
        );
    }
    push(
        "",
        "",
        render_probe_outcome("IPv6 host alias", &report.ipv6_contained, caps),
    );

    let egress_dev = if report.egress_dev.trim().is_empty() {
        "unavailable"
    } else {
        report.egress_dev.as_str()
    };
    if expects_tunnel {
        let route = format!("route to 1.1.1.1 via {egress_dev}");
        let route = if report.egress_via_wireguard_only {
            crate::ui::styled(Status::Ok, &route, caps)
        } else {
            crate::ui::styled(Status::Fail, &format!("{route} — failed"), caps)
        };
        let resolver = if report.resolver_in_tunnel {
            crate::ui::styled(Status::Ok, "first resolver inside tunnel", caps)
        } else {
            crate::ui::styled(
                Status::Fail,
                "first resolver not inside tunnel — failed",
                caps,
            )
        };
        push("", "tunnel", format!("{route} · {resolver}"));
    } else {
        push(
            "",
            "tunnel",
            crate::ui::styled(Status::Dim, "VPN not enforced", caps),
        );
    }

    let observed_ip = match (&report.egress_ip, &report.egress_loc) {
        (Some(ip), Some(loc)) => format!("{ip} ({loc})"),
        (Some(ip), None) => ip.clone(),
        (None, _) => "public IP unavailable".to_string(),
    };
    let observed_path = if expects_tunnel {
        String::new()
    } else {
        format!(" via {egress_dev}")
    };
    push(
        "",
        "egress",
        crate::ui::styled(
            Status::Dim,
            &format!("{observed_ip}{observed_path} — observed, not a containment check"),
            caps,
        ),
    );

    match report.public_probe.as_ref() {
        Some(ProbeOutcome::Dropped) => push(
            "",
            "",
            crate::ui::styled(
                Status::Dim,
                "public probe 1.1.1.1:443 dropped — failed",
                caps,
            ),
        ),
        Some(ProbeOutcome::NotTested(reason)) => push(
            "",
            "",
            crate::ui::styled(
                Status::Dim,
                &format!("public probe 1.1.1.1:443: {reason} — not tested"),
                caps,
            ),
        ),
        Some(ProbeOutcome::Reachable) | None => {}
    }

    push(
        "",
        "capsule",
        crate::ui::styled(Status::Ok, "no env value equals a real credential", caps),
    );
    rows.join("\n")
}

fn render_probe_outcome(label: &str, outcome: &ProbeOutcome, caps: Caps) -> String {
    match outcome {
        ProbeOutcome::Dropped => crate::ui::styled(Status::Ok, &format!("{label} dropped"), caps),
        ProbeOutcome::Reachable => {
            crate::ui::styled(Status::Fail, &format!("{label} reachable — failed"), caps)
        }
        ProbeOutcome::NotTested(reason) => crate::ui::styled(
            Status::Dim,
            &format!("{label}: {reason} — not tested"),
            caps,
        ),
    }
}

fn show_diff(host: &Path, session: &SessionId, base: git2::Oid) -> AnyResult<()> {
    let head = format!("refs/capscode/{}/head", session.as_str());
    let base = base.to_string();
    let status = Command::new("git")
        .arg("-C")
        .arg(host)
        .args(["diff", &base, &head])
        .status()
        .context("launching git diff")?;
    if !status.success() {
        bail!("git diff exited with {status}");
    }
    Ok(())
}

fn handoff_choices(generated_branch: &str) -> Vec<String> {
    vec![
        format!("Save as a branch (auto-generated: {generated_branch})"),
        "Apply changes to this folder".to_string(),
        "Review diff".to_string(),
        "Continue in capsule shell".to_string(),
        "Discard changes".to_string(),
    ]
}

fn print_summary(report: &EgressReport, verbose: bool) {
    let line = if verbose {
        format!(
            "{} commits on {} (+{} -{}, {} files)",
            report.commits,
            &report.base_sha.to_string()[..12],
            report.insertions,
            report.deletions,
            report.files_changed
        )
    } else {
        format!(
            "Capsule changes: {} files (+{} -{}).",
            report.files_changed, report.insertions, report.deletions
        )
    };
    note(line);
}

fn workspace_volume(session: &SessionId) -> VolumeId {
    VolumeId(format!("capscode-{}-workspace", session.as_str()))
}

pub(crate) fn cache_volume(session: &SessionId) -> VolumeId {
    VolumeId(format!("capscode-{}-cache", session.as_str()))
}

const HOST_REPO_STATE_FILE: &str = "host-repo.json";
const WORKSPACE_MODE_STATE_FILE: &str = "workspace-mode.json";

pub(crate) fn session_state_dir(session: &SessionId) -> PathBuf {
    Config::config_home()
        .join("sessions")
        .join(session.as_str())
}

pub(crate) fn read_session_host_repo(session: &SessionId) -> std::io::Result<PathBuf> {
    read_session_host_repo_if_present(session)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("persisted host repository is missing for session {session}"),
        )
    })
}

pub(crate) fn read_session_host_repo_if_present(
    session: &SessionId,
) -> std::io::Result<Option<PathBuf>> {
    let path = session_state_dir(session).join(HOST_REPO_STATE_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_reader(file).map(Some).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid persisted host repository {}: {error}",
                path.display()
            ),
        )
    })
}

pub(crate) fn read_session_workspace_mode(
    session: &SessionId,
) -> std::io::Result<Option<WorkspaceMode>> {
    let path = session_state_dir(session).join(WORKSPACE_MODE_STATE_FILE);
    match File::open(&path) {
        Ok(file) => serde_json::from_reader(file).map(Some).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid persisted workspace mode {}: {error}",
                    path.display()
                ),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn persist_session_host_repo(state_dir: &Path, host_repo: &Path) -> std::io::Result<()> {
    let path = state_dir.join(HOST_REPO_STATE_FILE);
    if path.exists() {
        let persisted: PathBuf = serde_json::from_reader(File::open(&path)?).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid persisted host repository {}: {error}",
                    path.display()
                ),
            )
        })?;
        if persisted == host_repo {
            return Ok(());
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "session state already names host repository {} instead of {}",
                persisted.display(),
                host_repo.display()
            ),
        ));
    }

    let mut contents = serde_json::to_vec(host_repo).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "serializing host repository {}: {error}",
                host_repo.display()
            ),
        )
    })?;
    contents.push(b'\n');
    crate::atomic_file::write(&path, &contents, Some(0o600))
}

fn persist_session_workspace_mode(state_dir: &Path, mode: WorkspaceMode) -> std::io::Result<()> {
    let path = state_dir.join(WORKSPACE_MODE_STATE_FILE);
    let file = File::create(&path)?;
    serde_json::to_writer(file, &mode).map_err(|error| {
        std::io::Error::other(format!(
            "writing workspace mode {}: {error}",
            path.display()
        ))
    })
}

fn ensure_session_state_dir(session: &SessionId) -> std::io::Result<PathBuf> {
    let home = Config::ensure_config_home()?;
    let sessions = home.join("sessions");
    let state_dir = sessions.join(session.as_str());
    std::fs::create_dir_all(&state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(state_dir)
}

fn validate_injected_credential(
    provider: &ProviderSpec,
    credential: &ProviderCredential,
    source: Option<&str>,
) -> AnyResult<()> {
    use secrecy::ExposeSecret;

    let fallback_source = format!("provider:{}", provider.id);
    let source = source.unwrap_or(&fallback_source);
    crate::config::validate_credential_line(credential.secret.expose_secret(), source)
}

/// RFC3339 UTC timestamp for the `capscode.created_at_rfc3339` label.
///
/// Formatted by hand (no chrono in this crate) via days-from-civil. Older
/// volumes can still carry the numeric form written by previous releases.
fn now_label() -> String {
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => return "1970-01-01T00:00:00Z".to_string(),
    };
    format_rfc3339(secs)
}

pub(crate) fn format_created_at(value: &str) -> String {
    value
        .parse::<i64>()
        .map(format_rfc3339)
        .unwrap_or_else(|_| value.to_string())
}

/// Unix seconds -> `YYYY-MM-DDTHH:MM:SSZ` (Howard Hinnant's civil-from-days).
fn format_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Created => "Created",
        State::Ingesting => "Ingesting",
        State::Provisioning => "Provisioning",
        State::Running => "Running",
        State::Draining => "Draining",
        State::Egressing => "Egressing",
        State::Failed => "Failed",
        State::TearingDown => "TearingDown",
        State::Destroyed => "Destroyed",
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

#[cfg(test)]
mod created_label_tests {
    use super::format_rfc3339;

    #[test]
    fn formats_unix_seconds_as_rfc3339_utc() {
        // The label is consumed by `capscode ls` to show object creation times.
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_786_007_224), "2026-08-06T09:07:04Z");
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
