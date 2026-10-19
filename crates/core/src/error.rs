//! Error taxonomy shared across all crates .
//!
//! Library crates never call `std::process::exit` and never print to stderr for
//! user consumption; they return these typed errors and the CLI renders them.

use std::path::PathBuf;

use crate::ids::{ImageRef, Provider, RuntimeKind, SessionId};

/// `bollard::errors::Error` boxed so `RuntimeError` stays cheap to move around.
pub type BollardBoxed = Box<bollard::errors::Error>;

#[derive(thiserror::Error, Debug)]
pub enum RuntimeError {
    #[error("no supported container runtime detected")]
    NoRuntime,
    #[error("runtime {0:?} is installed but not reachable: {1}")]
    Unreachable(RuntimeKind, String),
    #[error("required capability unsupported by {0:?}: {1}")]
    CapabilityUnsupported(RuntimeKind, String),
    #[error("image {0} not present and pull failed: {1}")]
    ImagePull(ImageRef, String),
    #[error("container {0} operation failed: {1}")]
    Container(String, String),
    #[error(transparent)]
    Api(#[from] BollardBoxed),
}

#[derive(thiserror::Error, Debug)]
pub enum GitError {
    #[error("not a git repository: {0:?}")]
    NotARepo(PathBuf),
    #[error("ingress failed: {0}")]
    Ingress(String),
    #[error("egress failed: {0}")]
    Egress(String),
    #[error(
        "refusing to apply runner or protected files to your working tree; your changes are on branch {branch}. Review these held paths before applying: {paths}",
        paths = .paths.join(", ")
    )]
    RunnerFilesHeld { branch: String, paths: Vec<String> },
    #[error("refusing to overwrite existing ref {0}")]
    RefConflict(String),
    #[error(transparent)]
    Lib(#[from] git2::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum BrokerError {
    #[error("no API key configured for provider {0:?}")]
    MissingKey(Provider),
    #[error("request to non-allowlisted host: {0}")]
    HostNotAllowed(String),
    #[error("tls setup failed: {0}")]
    Tls(String),
    #[error("upstream {0} error: {1}")]
    Upstream(String, String),
    #[error("bind {0} failed: {1}")]
    Bind(String, String),
}

#[derive(thiserror::Error, Debug)]
pub enum NetError {
    #[error("invalid WireGuard config: {0}")]
    BadWgConfig(String),
    #[error("kill switch verification failed: {0}")]
    KillSwitch(String),
    #[error("dns leak detected: resolver {0} is outside the tunnel")]
    DnsLeak(String),
    #[error("gateway provisioning failed: {0}")]
    Gateway(String),
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("config error: {0}")]
    Figment(#[from] figment::Error),
    #[error("config home unavailable: {0}")]
    Home(String),
    #[error("invalid value for {field}: {reason}")]
    Invalid { field: String, reason: String },
}

#[derive(thiserror::Error, Debug)]
pub enum CapsError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Broker(#[from] BrokerError),
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("session {0} aborted during {1}")]
    Aborted(SessionId, &'static str),
}

pub type Result<T> = std::result::Result<T, CapsError>;
