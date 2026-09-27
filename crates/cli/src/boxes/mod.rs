//! Persistent per-project boxes: naming, lookup, lifecycle, and the unsynced-work guard.
//!
//! A box is a long-lived capsule with every supported tool installed. Its name is
//! `<base>-<id>` (for example `abc-k3f9x2`), where `id` is six random lowercase letters or
//! digits. Commands accept the full name, any unique prefix, or the id alone.
//!
//! This module is the contract between the command layer and the box lifecycle code.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use capscode_core::{
    OwnedObject, OwnedObjectKind, RuntimeAdapter, LABEL_BOX, LABEL_BOX_ID, LABEL_OWNER,
    LABEL_OWNER_VALUE, LABEL_ROLE, LABEL_SESSION, LABEL_WORKSPACE_MODE, ROLE_CACHE,
};

mod guard;
pub mod index;
mod lifecycle;
pub mod name;

/// The keeper's tmux socket inside a box capsule.
const KEEPER_SOCKET: &str = "/run/capscode-keeper/sock";

/// Grace period shared by box stop, capture, and teardown paths.
pub(crate) const SESSION_STOP_GRACE: Duration = Duration::from_secs(10);

/// The command that enters a box shell. It sets the configured prefix on the keeper's tmux
/// server (binding a doubled prefix to send a literal one, as tmux does for `C-b`), then
/// attaches to the `main` session or creates it.
pub(crate) fn shell_attach_command(prefix: &str) -> Result<Vec<String>> {
    let prefix = crate::config::parse_box_prefix(prefix)?;
    Ok([
        "tmux",
        "-S",
        KEEPER_SOCKET,
        "set-option",
        "-g",
        "prefix",
        &prefix,
        ";",
        "bind-key",
        &prefix,
        "send-prefix",
        ";",
        "new-session",
        "-A",
        "-s",
        "main",
        "/bin/bash",
        "-l",
    ]
    .map(String::from)
    .to_vec())
}

/// How a word typed after `capscode` resolved to a box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly one box matched.
    One(BoxRef),
    /// Several boxes matched; the caller lists them and asks for more of the name.
    Ambiguous(Vec<BoxRef>),
    /// Nothing matched.
    None,
}

/// A box as the command layer sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxRef {
    /// Full name, `<base>-<id>`.
    pub name: String,
    /// The six-character id.
    pub id: String,
    /// The session id of the box's runtime objects.
    pub session: String,
}

/// Options for `capscode create`.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// Base name; defaults to the sanitized project folder name.
    pub base: Option<String>,
    /// `--providers all|<ids>`; `None` means ask interactively (or use tool defaults when not a tty).
    pub providers: Option<String>,
    /// Bind-mount the project instead of copying it. Boxes never inherit the global
    /// `workspace.mode`; mount mode is chosen per box.
    pub mount: bool,
    /// Keep the created box on its current toolchain when the global default changes.
    pub pin: bool,
    /// Skip creating and mounting a persistent helper-cache volume.
    pub no_cache: bool,
}

/// Options for `capscode resume`.
#[derive(Debug, Clone, Default)]
pub struct ResumeOptions {
    /// `--providers all|<ids>`: replaces the routed set.
    pub providers: Option<String>,
    /// Skip the confirmation prompt shown when a running box must be restarted.
    pub yes: bool,
    /// Explicit choice: `current`, `keep`, or a 12-character digest.
    pub toolchain: Option<String>,
    /// Pin the box to whichever toolchain was selected.
    pub pin: bool,
    /// Delete and recreate this box's helper-cache volume before resuming.
    pub fresh_cache: bool,
    /// Enable a persistent cache volume for this box unless caching is disabled globally.
    pub cache: bool,
    /// Allow an explicitly selected toolchain digest that the channel withdrew.
    pub force: bool,
}

/// One row of `capscode ls` for a box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxRow {
    pub name: String,
    /// Runtime session id used by sync/rm and by `ls` to suppress duplicate rows.
    pub session: Option<String>,
    /// `running`, `stopped`, `legacy`, or `path missing`.
    pub state: String,
    pub project: PathBuf,
    /// `copy` or `mount`.
    pub mode: String,
    /// Routed provider ids.
    pub providers: Vec<String>,
    /// RFC3339, or empty if never entered.
    pub last_entered: String,
    /// 12-character toolchain digest, if this is a new-style box.
    pub toolchain: String,
    pub toolchain_current: bool,
    pub toolchain_pinned: bool,
    pub toolchain_tested: bool,
    /// The release channel or the active lock withdrew this toolchain.
    pub toolchain_yanked: bool,
    /// Cache volume name when this box has one in the runtime inventory.
    pub cache_volume: Option<String>,
}

pub(crate) fn cache_volume_matches_box(object: &OwnedObject, target: &BoxRef) -> bool {
    object.kind == OwnedObjectKind::Volume
        && object.labels.get(LABEL_OWNER).map(String::as_str) == Some(LABEL_OWNER_VALUE)
        && object.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CACHE)
        && object
            .session
            .as_ref()
            .is_some_and(|session| session.as_str() == target.session)
        && object.labels.get(LABEL_SESSION).map(String::as_str) == Some(&target.session)
        && object.labels.get(LABEL_BOX).map(String::as_str) == Some(&target.name)
        && object.labels.get(LABEL_BOX_ID).map(String::as_str) == Some(&target.id)
}

pub(crate) fn cache_volumes_for_box<'a>(
    objects: &'a [OwnedObject],
    target: &BoxRef,
) -> Vec<&'a OwnedObject> {
    objects
        .iter()
        .filter(|object| cache_volume_matches_box(object, target))
        .collect()
}

pub(crate) fn orphaned_cache_volumes(objects: &[OwnedObject]) -> Vec<String> {
    let mut orphaned = objects
        .iter()
        .filter(|object| {
            object.kind == OwnedObjectKind::Volume
                && object.labels.get(LABEL_ROLE).map(String::as_str) == Some(ROLE_CACHE)
        })
        .filter(|cache| {
            !objects.iter().any(|candidate| {
                matches!(
                    candidate.kind,
                    OwnedObjectKind::Container | OwnedObjectKind::Volume
                ) && candidate.labels.get(LABEL_ROLE).map(String::as_str) != Some(ROLE_CACHE)
                    && [LABEL_SESSION, LABEL_BOX, LABEL_BOX_ID]
                        .into_iter()
                        .all(|key| candidate.labels.get(key) == cache.labels.get(key))
            })
        })
        .map(|object| object.id.clone())
        .collect::<Vec<_>>();
    orphaned.sort();
    orphaned
}

/// Resolve a user-typed word (full name, unique prefix, or id) to a box.
pub fn resolve(word: &str) -> Result<Resolved> {
    name::resolve(word)
}

/// Boxes that belong to `project` (the canonical project directory).
pub fn for_project(project: &Path) -> Result<Vec<BoxRef>> {
    lifecycle::for_project(project)
}

/// Create a box for `project` and return it.
pub async fn create(
    project: &Path,
    options: CreateOptions,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Result<BoxRef> {
    lifecycle::create(project, options, runtime).await
}

/// Enter a box's shell, resuming it first if it is stopped. Returns the shell's exit code.
pub async fn enter(target: &BoxRef, runtime: Arc<dyn RuntimeAdapter>) -> Result<i32> {
    lifecycle::enter(target, runtime).await
}

/// Bring a stopped box back (or re-route a running one after confirmation).
pub async fn resume(
    target: &BoxRef,
    options: ResumeOptions,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Result<()> {
    lifecycle::resume(target, options, runtime).await
}

/// Stop a box; its workspace is kept.
pub async fn stop(target: &BoxRef, runtime: Arc<dyn RuntimeAdapter>) -> Result<()> {
    lifecycle::stop(target, runtime).await
}

/// Rows for `capscode ls`.
pub async fn ls_rows(runtime: Arc<dyn RuntimeAdapter>) -> Result<Vec<BoxRow>> {
    lifecycle::ls_rows(runtime).await
}

/// What removing a box's workspace would lose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkCheck {
    /// Tracked or untracked work that has not been synced to the host.
    pub unsynced: bool,
    /// Gitignored paths in the workspace (directories collapsed). Sync never exports them.
    pub ignored: Vec<String>,
}

/// What the box holds that has not reached the host. An error means the check could not run;
/// callers must treat that as "cannot verify" and require `--discard-workspace`.
pub async fn unsynced_work(target: &BoxRef, runtime: Arc<dyn RuntimeAdapter>) -> Result<WorkCheck> {
    guard::unsynced_work_with_runtime(target, runtime.as_ref()).await
}

/// A box whose workspace volume `uninstall` would delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxWorkspace {
    pub name: String,
    /// Why deleting it would lose work that is not on the host; `None` when nothing would be lost.
    pub risk: Option<String>,
}

/// Identity of the exact workspace state copied back to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyncIdentity {
    pub oid: String,
    pub kind: String,
}

/// Check every box workspace represented by runtime labels. Missing or unreadable index metadata
/// cannot establish that the workspace is safe to remove.
pub async fn workspaces_at_risk(
    objects: &[OwnedObject],
    runtime: Option<&dyn RuntimeAdapter>,
) -> Result<Vec<BoxWorkspace>> {
    workspaces_at_risk_from(objects, index::read(), runtime).await
}

#[derive(Clone, Debug)]
struct RuntimeBox {
    name: String,
    id: Option<String>,
    session: Option<String>,
    mode: Option<String>,
    mismatch: bool,
}

fn runtime_boxes(objects: &[OwnedObject]) -> Vec<RuntimeBox> {
    // DECISION: the session label groups each box capsule, gateway, and workspace volume into
    // one inventory row.
    let mut boxes = std::collections::BTreeMap::<String, RuntimeBox>::new();
    for object in objects.iter().filter(|object| {
        matches!(
            object.kind,
            OwnedObjectKind::Container | OwnedObjectKind::Volume
        ) && object.labels.contains_key(LABEL_BOX)
            && object.labels.get(LABEL_ROLE).map(String::as_str) != Some(ROLE_CACHE)
    }) {
        let name = object.labels.get(LABEL_BOX).cloned().unwrap_or_default();
        let session = object
            .session
            .as_ref()
            .map(|session| session.as_str().to_owned())
            .or_else(|| object.labels.get(LABEL_SESSION).cloned());
        let key = session.clone().unwrap_or_else(|| format!("box:{name}"));
        let candidate = RuntimeBox {
            name: if name.is_empty() { key.clone() } else { name },
            id: object.labels.get(LABEL_BOX_ID).cloned(),
            session,
            mode: object.labels.get(LABEL_WORKSPACE_MODE).cloned(),
            mismatch: false,
        };
        boxes
            .entry(key)
            .and_modify(|current| {
                if current.name != candidate.name
                    || matches!((&current.id, &candidate.id), (Some(left), Some(right)) if left != right)
                    || matches!((&current.mode, &candidate.mode), (Some(left), Some(right)) if left != right)
                {
                    current.mismatch = true;
                }
                if current.id.is_none() {
                    current.id.clone_from(&candidate.id);
                }
                if current.session.is_none() {
                    current.session.clone_from(&candidate.session);
                }
                if current.mode.is_none() {
                    current.mode.clone_from(&candidate.mode);
                }
            })
            .or_insert(candidate);
    }
    boxes.into_values().collect()
}

pub(crate) async fn workspaces_at_risk_from(
    objects: &[OwnedObject],
    entries: Result<Vec<index::Entry>>,
    runtime: Option<&dyn RuntimeAdapter>,
) -> Result<Vec<BoxWorkspace>> {
    let mut workspaces = Vec::new();
    let inventory = runtime_boxes(objects);
    let entries = entries.ok();
    for runtime_box in inventory {
        let entry = entries.as_ref().and_then(|entries| {
            runtime_box
                .session
                .as_deref()
                .and_then(|session| entries.iter().find(|entry| entry.session == session))
        });
        let risk = if entries.is_none() {
            Some("cannot read box metadata".to_owned())
        } else if entry.is_none() {
            // DECISION: require an index match even when runtime labels claim mount mode; a
            // mismatch between the runtime inventory and saved box metadata is unverified.
            Some("box metadata is missing".to_owned())
        } else if let Some(entry) = entry {
            if runtime_box.mismatch
                || runtime_box.name != entry.name
                || runtime_box.id.as_deref().is_some_and(|id| id != entry.id)
                || runtime_box
                    .mode
                    .as_deref()
                    .is_some_and(|mode| mode != entry.mode)
            {
                Some("box metadata does not match runtime labels".to_owned())
            } else if entry.mode == "mount" {
                None
            } else if let Some(runtime) = runtime {
                let target = BoxRef {
                    name: entry.name.clone(),
                    id: entry.id.clone(),
                    session: entry.session.clone(),
                };
                match guard::unsynced_work_with_runtime(&target, runtime).await {
                    Ok(work) if work.unsynced => Some("work not synced".to_owned()),
                    Ok(work) if !work.ignored.is_empty() => Some(format!(
                        "{} gitignored path(s) that sync never copies",
                        work.ignored.len()
                    )),
                    Ok(_) => None,
                    Err(_) => Some("cannot check for unsynced work".to_owned()),
                }
            } else {
                Some("cannot check for unsynced work".to_owned())
            }
        } else {
            Some("cannot check for unsynced work".to_owned())
        };
        workspaces.push(BoxWorkspace {
            name: runtime_box.name,
            risk,
        });
    }
    Ok(workspaces)
}

/// Read the identity of a stopped box workspace after copy-back has captured it.
pub(crate) async fn workspace_identity(
    session: &str,
    runtime: &dyn RuntimeAdapter,
) -> Result<SyncIdentity> {
    guard::workspace_identity(session, runtime).await
}

/// Record the exact workspace identity that copy-back exported to the host.
pub async fn record_synced(target: &BoxRef, identity: SyncIdentity) -> Result<()> {
    record_synced_with_home(target, identity, &capscode_core::Config::config_home()).await
}

pub(crate) async fn record_synced_with_home(
    target: &BoxRef,
    identity: SyncIdentity,
    config_home: &Path,
) -> Result<()> {
    guard::record_synced_with_home(target, identity, config_home).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn box_workspace_volume() -> OwnedObject {
        let session = "00000000-0000-0000-0000-000000000091";
        OwnedObject {
            kind: OwnedObjectKind::Volume,
            id: "workspace-volume".into(),
            session: Some(session.to_owned().into()),
            role: None,
            created_at: None,
            state: None,
            image: None,
            labels: BTreeMap::from([
                (LABEL_BOX.into(), "alpha-123abc".into()),
                (LABEL_BOX_ID.into(), "123abc".into()),
                (LABEL_SESSION.into(), session.into()),
                (LABEL_WORKSPACE_MODE.into(), "copy".into()),
            ]),
        }
    }

    fn cache_volume(name: &str, id: &str, session: &str) -> OwnedObject {
        OwnedObject {
            kind: OwnedObjectKind::Volume,
            id: format!("capscode-{session}-cache"),
            session: Some(session.to_owned().into()),
            role: Some(ROLE_CACHE.into()),
            created_at: None,
            state: None,
            image: None,
            labels: BTreeMap::from([
                (LABEL_OWNER.into(), LABEL_OWNER_VALUE.into()),
                (LABEL_ROLE.into(), ROLE_CACHE.into()),
                (LABEL_BOX.into(), name.into()),
                (LABEL_BOX_ID.into(), id.into()),
                (LABEL_SESSION.into(), session.into()),
            ]),
        }
    }

    #[test]
    fn cache_volume_selection_uses_exact_box_labels_and_never_a_workspace_prefix() {
        let target = BoxRef {
            name: "alpha-123abc".into(),
            id: "123abc".into(),
            session: "session-one".into(),
        };
        let selected = cache_volume("alpha-123abc", "123abc", "session-one");
        let similar_name = cache_volume("alpha-123abc2", "123abc2", "session-one2");
        let mut workspace = cache_volume("alpha-123abc", "123abc", "session-one");
        workspace.id = "capscode-session-one-workspace".into();
        workspace.role = Some("capsule".into());
        workspace.labels.insert(LABEL_ROLE.into(), "capsule".into());

        let objects = vec![selected, similar_name, workspace];
        let matches = cache_volumes_for_box(&objects, &target);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "capscode-session-one-cache");
    }

    #[tokio::test]
    async fn cache_only_inventory_does_not_trigger_the_unsynced_workspace_guard() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("boxes.json");
        let boxes = workspaces_at_risk_from(
            &[cache_volume("alpha-123abc", "123abc", "session-one")],
            index::read_from(&path),
            None,
        )
        .await
        .unwrap();
        assert!(boxes.is_empty());
    }

    #[test]
    fn orphaned_cache_listing_matches_the_full_box_identity() {
        let cache = cache_volume("alpha-123abc", "123abc", "session-one");
        let similar = cache_volume("alpha-123abc2", "123abc2", "session-one2");
        assert_eq!(
            orphaned_cache_volumes(&[cache.clone(), similar.clone()]),
            [cache.id.clone(), similar.id.clone()]
        );

        let mut capsule = cache.clone();
        capsule.kind = OwnedObjectKind::Container;
        capsule.id = "capsule-one".into();
        capsule.role = Some("capsule".into());
        capsule.labels.insert(LABEL_ROLE.into(), "capsule".into());
        assert_eq!(
            orphaned_cache_volumes(&[cache.clone(), similar, capsule]),
            ["capscode-session-one2-cache"]
        );
    }

    #[tokio::test]
    async fn uninstall_counts_unindexed_box_volume_as_at_risk_when_index_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("boxes.json");
        let entries = index::read_from(&path).unwrap();
        let boxes = workspaces_at_risk_from(&[box_workspace_volume()], Ok(entries), None)
            .await
            .unwrap();

        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].name, "alpha-123abc");
        assert_eq!(boxes[0].risk.as_deref(), Some("box metadata is missing"));
    }

    #[tokio::test]
    async fn uninstall_counts_unindexed_box_volume_as_at_risk_when_index_is_corrupt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("boxes.json");
        std::fs::write(&path, b"{").unwrap();
        let entries = index::read_from(&path);
        assert!(entries.is_err());
        let boxes = workspaces_at_risk_from(&[box_workspace_volume()], entries, None)
            .await
            .unwrap();

        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].name, "alpha-123abc");
        assert_eq!(boxes[0].risk.as_deref(), Some("cannot read box metadata"));
    }
}
