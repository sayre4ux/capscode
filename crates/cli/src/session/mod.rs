pub(crate) mod branch;
pub mod shadow;
pub mod state;
pub mod timing;

pub(crate) const GIT_OPERATION_MARKERS: &[&str] = &[
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
];

/// Shell guard shared by the capsule's exit and explicit-sync WIP commits.
/// Never stage an in-progress operation; return success so sync can export the
/// existing committed work.
pub(crate) fn git_operation_guard_shell() -> String {
    format!(
        "for marker in {}; do marker_path=$(git -C \"${{CAPSCODE_WORKSPACE_PATH:-/workspace}}\" rev-parse --path-format=absolute --git-path \"$marker\") || exit 74; if [ -e \"$marker_path\" ]; then printf 'Git operation in progress (%s); skipping the WIP commit and syncing committed work.\\n' \"$marker\"; exit 0; fi; done",
        GIT_OPERATION_MARKERS.join(" ")
    )
}

pub use state::{SessionDriver, SessionOptions};

pub(crate) use state::{
    broker_aliases, inject_gateway_secrets, provider_manifest_json, read_gateway_ca,
    set_container_env,
};
