//! Identifiers and small value types that cross crate seams .

use serde::{Deserialize, Serialize};

/// Session identifier (UUID v4), also used as the label suffix applied to every
/// runtime object CapsCode creates for a session.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        SessionId(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        SessionId(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerId(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeId(pub String);

/// `repo:tag` or `repo@digest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef(pub String);

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    Docker,
    OrbStack,
    AppleContainer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    OpenAI,
    Moonshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tool {
    Claude,
    Codex,
    Opencode,
    Pi,
    Omp,
}

impl Tool {
    pub const ALL: [Tool; 5] = [
        Tool::Claude,
        Tool::Codex,
        Tool::Opencode,
        Tool::Pi,
        Tool::Omp,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }
}

/// Applied to every CapsCode-created runtime object so `ls`/`rm`/`doctor` can
/// enumerate and force-clean without local bookkeeping being the source of truth.
pub const LABEL_OWNER: &str = "capscode";
/// Value written under [`LABEL_OWNER`] to mark ownership.
pub const LABEL_OWNER_VALUE: &str = "1";
pub const LABEL_SESSION: &str = "capscode.session";
pub const LABEL_ROLE: &str = "capscode.role";
pub const LABEL_CONTRACT: &str = "capscode.contract";
pub const LABEL_CREATED: &str = "capscode.created_at_rfc3339";
pub const LABEL_BOX: &str = "capscode.box";
pub const LABEL_BOX_ID: &str = "capscode.box_id";
pub const LABEL_PROJECT: &str = "capscode.project";
pub const LABEL_PROJECT_HASH: &str = "capscode.project_hash";
pub const LABEL_PROVIDERS: &str = "capscode.providers";
pub const LABEL_WORKSPACE_MODE: &str = "capscode.workspace_mode";
pub const LABEL_TOOLCHAIN: &str = "capscode.toolchain";

/// Values written under [`LABEL_ROLE`].
pub const ROLE_CAPSULE: &str = "capsule";
pub const ROLE_CACHE: &str = "cache";
pub const ROLE_GATEWAY: &str = "gateway";
