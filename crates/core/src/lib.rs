//! `capscode-core` — domain types, config, errors, and cross-seam traits.
//!
//! This crate has no intra-workspace dependencies; every other crate in the
//! workspace compiles against it alone .

mod config;
mod error;
mod ids;
mod providers;
mod traits;

pub use config::{
    AuthConfig, AuthKind, BoxConfig, BrokerConfig, CapsuleConfig, Config, GitConfig, HistoryDial,
    ImageConfig, MemoryLimit, NetworkConfig, PidsLimit, ProfileConfig, ProviderCredential,
    ProviderDefaults, ProviderKeys, PullPolicy, RuntimePref, UpdateConfig, WorkspaceConfig,
    WorkspaceMode, DEFAULT_BOX_PREFIX, DEFAULT_PIDS_LIMIT, DEFAULT_UPDATE_CHANNEL,
};
pub use error::{
    BollardBoxed, BrokerError, CapsError, ConfigError, GitError, NetError, Result, RuntimeError,
};
pub use ids::{
    ContainerId, ImageRef, Provider, RuntimeKind, SessionId, Tool, VolumeId, LABEL_BOX,
    LABEL_BOX_ID, LABEL_CONTRACT, LABEL_CREATED, LABEL_OWNER, LABEL_OWNER_VALUE, LABEL_PROJECT,
    LABEL_PROJECT_HASH, LABEL_PROVIDERS, LABEL_ROLE, LABEL_SESSION, LABEL_TOOLCHAIN,
    LABEL_WORKSPACE_MODE, ROLE_CACHE, ROLE_CAPSULE, ROLE_GATEWAY,
};
pub use providers::{
    load_bundled_provider_registry, merge_provider_registries, ApiStyle, AuthScheme,
    ProviderRegistryError, ProviderSpec,
};
pub use traits::{
    AttachIo, ContainerSpec, EgressPlan, EgressReport, ExecOutput, ExecSpec, GitEngine,
    IngressPlan, IngressProgress, IngressReceipt, IngressReport, LocalImageInfo, Mount, NetContext,
    NetVerifyReport, NetworkMode, NetworkStack, OwnedObject, OwnedObjectKind, PhaseTiming,
    PortProtocol, PortPublish, ProbeOutcome, PublishMode, PullProgress, RuntimeAdapter,
    RuntimeCapabilities, RuntimeInfo, VolumeSpec,
};
