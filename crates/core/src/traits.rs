//! Cross-seam traits and their associated spec/report types .

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{BrokerConfig, HistoryDial, NetworkConfig, ProviderKeys};
use crate::error::{GitError, NetError, RuntimeError};
use crate::ids::{ContainerId, ImageRef, RuntimeKind, SessionId, VolumeId};

/// The single most important seam; defined in `core`, implemented in `runtime`.
#[async_trait]
pub trait RuntimeAdapter: Send + Sync {
    fn kind(&self) -> RuntimeKind;

    /// Detect + verify reachability and required capabilities. Used by `doctor` and preflight.
    async fn preflight(&self) -> Result<RuntimeInfo, RuntimeError>;
    fn capabilities(&self) -> RuntimeCapabilities;

    async fn create_volume(&self, spec: &VolumeSpec) -> Result<VolumeId, RuntimeError>;
    async fn remove_volume(&self, id: &VolumeId) -> Result<(), RuntimeError>;

    async fn image_present(&self, image: &ImageRef) -> Result<bool, RuntimeError>;
    async fn pull_image(
        &self,
        image: &ImageRef,
        on_progress: &mut (dyn FnMut(PullProgress) + Send),
    ) -> Result<(), RuntimeError>;

    async fn create_container(&self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError>;
    async fn start(&self, id: &ContainerId) -> Result<(), RuntimeError>;
    /// Non-interactive command (used for ingress WIP, git-upload-pack, nft apply, doctor probes).
    async fn exec(&self, id: &ContainerId, spec: ExecSpec) -> Result<ExecOutput, RuntimeError>;
    /// Interactive attach for the AI tool itself (TTY passthrough).
    async fn run_interactive(
        &self,
        id: &ContainerId,
        spec: ExecSpec,
        io: AttachIo,
    ) -> Result<i32, RuntimeError>;
    async fn stop(&self, id: &ContainerId, timeout: Duration) -> Result<(), RuntimeError>;
    async fn remove(&self, id: &ContainerId, force: bool) -> Result<(), RuntimeError>;

    /// Enumerate CapsCode-owned objects by label (backs `ls` / `rm --all`).
    async fn list_owned(&self) -> Result<Vec<OwnedObject>, RuntimeError>;

    /// Remove a CapsCode-owned network returned by [`RuntimeAdapter::list_owned`].
    async fn remove_network(&self, id: &str) -> Result<(), RuntimeError> {
        Err(RuntimeError::CapabilityUnsupported(
            self.kind(),
            format!("network removal is not supported for {id}"),
        ))
    }

    /// List locally tagged images. Used by the explicit uninstall image purge.
    async fn list_local_images(&self) -> Result<Vec<String>, RuntimeError> {
        Err(RuntimeError::CapabilityUnsupported(
            self.kind(),
            "local image listing is not supported".to_string(),
        ))
    }

    /// List local image tags with size and build time when the runtime exposes them.
    async fn list_local_image_details(&self) -> Result<Vec<LocalImageInfo>, RuntimeError> {
        Ok(self
            .list_local_images()
            .await?
            .into_iter()
            .map(|reference| LocalImageInfo {
                reference,
                size_bytes: None,
                created_unix: None,
                labels: BTreeMap::new(),
            })
            .collect())
    }

    /// Remove one locally tagged image. Used by the explicit uninstall image purge.
    async fn remove_image(&self, image: &str) -> Result<(), RuntimeError> {
        Err(RuntimeError::CapabilityUnsupported(
            self.kind(),
            format!("image removal is not supported for {image}"),
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishMode {
    Native,
    HostProxyRequired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    pub localhost_publish: PublishMode,
    /// `--network container:<id>`.
    pub shared_netns: bool,
    pub tmpfs_mounts: bool,
    pub cap_add_net_admin: bool,
    pub dev_net_tun: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub kind: RuntimeKind,
    pub version: String,
    pub socket_path: Option<String>,
    pub capabilities: RuntimeCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSpec {
    pub name: String,
    pub labels: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullProgress {
    pub layer_id: Option<String>,
    pub status: String,
    pub current: u64,
    pub total: Option<u64>,
}

pub struct ContainerSpec {
    pub image: ImageRef,
    /// ALWAYS "workspace" for the capsule.
    pub hostname: String,
    /// Never contains a real provider key for the capsule.
    pub env: Vec<(String, String)>,
    pub cmd: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    /// Volume | Tmpfs | (Bind only in `--mount` mode).
    pub mounts: Vec<Mount>,
    pub network: NetworkMode,
    /// 127.0.0.1 only.
    pub published_ports: Vec<PortPublish>,
    pub cap_add: Vec<String>,
    /// Linux capabilities to drop (Docker `HostConfig.CapDrop`). `["ALL"]` on the
    /// untrusted capsule; empty elsewhere.
    pub cap_drop: Vec<String>,
    /// Set the `no-new-privileges` security-opt (Docker `HostConfig.SecurityOpt`).
    /// True on the capsule so the agent can never regain privileges via a setuid binary.
    pub no_new_privileges: bool,
    pub devices: Vec<String>,
    pub sysctls: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
    pub tmpfs_home: bool,
    /// True for the capsule; writable paths are explicit tmpfs.
    pub read_only_rootfs: bool,
    /// Run Docker's init (`--init` / tini) as PID 1. True for the long-lived
    /// capsule so orphaned agent children are reaped; false elsewhere.
    pub init: bool,
    /// Process/thread cap (Docker `HostConfig.PidsLimit`). Required on every
    /// container; zero is refused because Docker reads it as "unlimited".
    pub pids_limit: u32,
    /// Memory cap in bytes (Docker `HostConfig.Memory`, with `MemorySwap` set to
    /// the same value so swap cannot extend it). `None` sets no memory limit.
    pub memory_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkMode {
    None,
    Bridge(String),
    SharedWith(ContainerId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mount {
    Volume {
        id: VolumeId,
        target: PathBuf,
        read_only: bool,
    },
    Tmpfs {
        target: PathBuf,
        size_mb: Option<u64>,
        exec: bool,
    },
    Bind {
        source: PathBuf,
        target: PathBuf,
        read_only: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortProtocol {
    Tcp,
    Udp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortPublish {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ExecSpec {
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<PathBuf>,
    pub user: Option<String>,
    pub tty: bool,
    /// Optional bytes written to the exec's stdin, which is then closed (EOF).
    /// Used to inject secret material (provider keys, WireGuard config) into a
    /// running container without ever placing it in argv, env, or on host disk.
    /// Redacted in [`Debug`] so a secret can never leak through a log line.
    pub stdin: Option<Vec<u8>>,
}

// Hand-written so `stdin` is length-only in Debug output; every other field keeps
// its derived representation. The derived Debug would print the raw secret bytes.
impl std::fmt::Debug for ExecSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct RedactedBytes(usize);
        impl std::fmt::Debug for RedactedBytes {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "<{} bytes redacted>", self.0)
            }
        }
        f.debug_struct("ExecSpec")
            .field("cmd", &self.cmd)
            .field("env", &self.env)
            .field("workdir", &self.workdir)
            .field("user", &self.user)
            .field("tty", &self.tty)
            .field(
                "stdin",
                &self.stdin.as_ref().map(|b| RedactedBytes(b.len())),
            )
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

/// TTY passthrough streams for [`RuntimeAdapter::run_interactive`].
pub struct AttachIo {
    pub stdin: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    pub stdout: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    pub stderr: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnedObjectKind {
    Container,
    Volume,
    Network,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedObject {
    pub kind: OwnedObjectKind,
    pub id: String,
    pub session: Option<SessionId>,
    pub role: Option<String>,
    pub created_at: Option<String>,
    /// Runtime lifecycle state for containers (`running`, `exited`, `paused`, …),
    /// as the engine reports it. `None` for volumes and networks, which have no
    /// such state. Listings must read this rather than assuming a returned
    /// container is live: owned-object listing includes stopped containers.
    pub state: Option<String>,
    /// Image reference from the runtime for containers; `None` otherwise.
    pub image: Option<String>,
    /// Labels returned by the runtime; box metadata is authoritative here.
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalImageInfo {
    pub reference: String,
    pub size_bytes: Option<u64>,
    /// Runtime creation time as Unix seconds.
    pub created_unix: Option<i64>,
    /// Image labels returned by the runtime, used to distinguish project-owned images.
    pub labels: BTreeMap<String, String>,
}

// --- capscode-git seam ------------------------------------------------------

/// Implemented in `capscode-git`.
#[async_trait]
pub trait GitEngine: Send + Sync {
    async fn ingress(
        &self,
        plan: &IngressPlan,
        rt: &dyn RuntimeAdapter,
        on: &mut (dyn FnMut(IngressProgress) + Send),
    ) -> Result<IngressReport, GitError>;
    /// Fetch capsule refs into `refs/capscode/<session>`.
    async fn egress(
        &self,
        plan: &EgressPlan,
        rt: &dyn RuntimeAdapter,
    ) -> Result<EgressReport, GitError>;
    /// Creates branch, NEVER touches the working tree.
    fn apply_as_branch(
        &self,
        host_repo: &Path,
        session: &SessionId,
        name: &str,
    ) -> Result<String, GitError>;
    /// Applies only the capsule delta to the host working tree. The index is
    /// left untouched, no hooks run, and fetched session refs remain available.
    fn apply_to_worktree(
        &self,
        host_repo: &Path,
        session: &SessionId,
        baseline: git2::Oid,
    ) -> Result<(), GitError>;
    fn discard(&self, host_repo: &Path, session: &SessionId) -> Result<(), GitError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressPlan {
    pub host_repo: PathBuf,
    pub session: SessionId,
    pub state_dir: PathBuf,
    pub seeder_image: ImageRef,
    pub history: HistoryDial,
    pub include: Vec<PathBuf>,
    pub verbose: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressProgress {
    pub phase: String,
    pub bytes_copied: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum IngressReceipt {
    GitSnapshot {
        baseline: Option<String>,
    },
    Shadow {
        git_dir: PathBuf,
        baseline: String,
        state_dir: PathBuf,
    },
}

/// One measured step of a launch, in the order it ran. Purely observational:
/// nothing branches on a timing, so a mis-measurement can never change behaviour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhaseTiming {
    pub phase: String,
    pub elapsed: Duration,
}

impl PhaseTiming {
    pub fn new(phase: impl Into<String>, elapsed: Duration) -> Self {
        Self {
            phase: phase.into(),
            elapsed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngressReport {
    pub copied_bytes: u64,
    /// Regular files in the prepared tree (the unit ingress cost scales with).
    pub copied_files: u64,
    pub elapsed: Duration,
    pub wip_commit: Option<git2::Oid>,
    pub receipt: IngressReceipt,
    /// Per-step ingress timings, ordered. Empty is always legal.
    pub phases: Vec<PhaseTiming>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressPlan {
    pub host_repo: PathBuf,
    pub session: SessionId,
    pub capsule: ContainerId,
    pub workspace_path: PathBuf,
    /// Exact ingress snapshot. `None` preserves legacy best-effort accounting.
    pub baseline: Option<git2::Oid>,
    pub verbose: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EgressReport {
    pub base_sha: git2::Oid,
    pub commits: u32,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

// --- capscode-net seam -------------------------------------------------------

/// Implemented in `capscode-net`.
#[async_trait]
pub trait NetworkStack: Send + Sync {
    /// Build the gateway container spec (WG + nft + broker sidecar) for this session.
    fn gateway_spec(&self, ctx: &NetContext) -> Result<ContainerSpec, NetError>;
    /// After the gateway is up, verify kill switch + DNS from inside it.
    async fn verify(
        &self,
        gateway: &ContainerId,
        rt: &dyn RuntimeAdapter,
    ) -> Result<NetVerifyReport, NetError>;
}

#[derive(Clone, Debug)]
pub struct NetContext {
    pub session: SessionId,
    pub network: NetworkConfig,
    pub broker: BrokerConfig,
    pub keys: ProviderKeys,
    pub ca_cert_pem: String,
    pub gateway_image: ImageRef,
}

/// Outcome of a single network containment probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe ran and the connection was dropped — contained.
    Dropped,
    /// The probe ran and the destination was reachable — NOT contained.
    Reachable,
    /// The probe did not run. Carries the reason, for display.
    NotTested(String),
}

impl Default for ProbeOutcome {
    fn default() -> Self {
        Self::NotTested("not run".into())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetVerifyReport {
    pub egress_via_wireguard_only: bool,
    pub resolver_in_tunnel: bool,
    pub rfc1918_blocked: bool,
    pub host_gateway_blocked: bool,
    pub ipv6_contained: ProbeOutcome,
    /// Which host-gateway probe candidates actually ran, by name.
    pub host_gateway: Vec<(String, ProbeOutcome)>,
    /// Public egress probe outcome (`PUBLIC_PROBE`), if the key was present.
    pub public_probe: Option<ProbeOutcome>,
    /// Interface ordinary egress leaves by (`wg0` in tunnel mode, else the host iface). Observed.
    pub egress_dev: String,
    /// World-facing egress IP per Cloudflare trace. Informational; None if unavailable.
    pub egress_ip: Option<String>,
    /// Two-letter country of the egress IP per Cloudflare trace. Informational; None if unavailable.
    pub egress_loc: Option<String>,
    pub details: Vec<String>,
}

// --- object-safety proof -----------------------------------------------------
//
// These never run; they just force the compiler to prove each trait is
// usable as `dyn Trait`, which lets adapters cross crate boundaries.
fn _runtime_adapter_is_object_safe(_: &dyn RuntimeAdapter) {}
fn _git_engine_is_object_safe(_: &dyn GitEngine) {}
fn _network_stack_is_object_safe(_: &dyn NetworkStack) {}

#[cfg(test)]
mod object_safety_tests {
    use super::*;

    #[test]
    fn ingress_receipt_serde_round_trips() {
        let receipts = [
            IngressReceipt::GitSnapshot {
                baseline: Some("0123456789012345678901234567890123456789".to_string()),
            },
            IngressReceipt::Shadow {
                git_dir: PathBuf::from("/tmp/session/shadow.git"),
                baseline: "9876543210987654321098765432109876543210".to_string(),
                state_dir: PathBuf::from("/tmp/session"),
            },
        ];

        for receipt in receipts {
            let encoded = serde_json::to_vec(&receipt).unwrap();
            let decoded: IngressReceipt = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, receipt);
        }
    }

    #[test]
    fn dyn_trait_objects_compile() {
        let _r: Option<Box<dyn RuntimeAdapter>> = None;
        let _g: Option<Box<dyn GitEngine>> = None;
        let _n: Option<Box<dyn NetworkStack>> = None;
    }
}
