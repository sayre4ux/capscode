use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use capscode_core::{
    ContainerId, ContainerSpec, ExecSpec, Mount, NetworkMode, OwnedObject, OwnedObjectKind,
    RuntimeAdapter, VolumeId, ROLE_CAPSULE,
};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::session::state::session_state_dir;

use super::{index, BoxRef, SyncIdentity, WorkCheck};

const LAST_SYNCED: &str = "last-synced.json";
const WORKSPACE: &str = "/workspace";
/// Separates `git status` output from the gitignored-path listing in the check output.
const IGNORED_MARKER: &str = "--ignored--";
const SHADOW_GIT_DIR: &str = ".capscode-autosync.git/";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SyncRecord {
    oid: String,
    kind: String,
}

pub(crate) async fn unsynced_work_with_runtime(
    target: &BoxRef,
    runtime: &dyn RuntimeAdapter,
) -> Result<WorkCheck> {
    let entry = index::entry_for_session(&target.session)?
        .ok_or_else(|| anyhow!("cannot verify box work: box metadata is missing"))?;
    if entry.mode == "mount" {
        return Ok(WorkCheck::default());
    }
    let record_path = session_state_dir(&capscode_core::SessionId::from(target.session.clone()))
        .join(LAST_SYNCED);
    let record: Option<SyncRecord> = match fs::read(&record_path) {
        Ok(bytes) => Some(
            serde_json::from_slice(&bytes)
                .context("cannot verify box work: invalid last-synced record")?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).context("cannot verify box work: reading last-synced record")
        }
    };
    let objects = runtime
        .list_owned()
        .await
        .context("cannot verify box work: listing box containers")?;
    let capsule = capsule(&objects, target)
        .ok_or_else(|| anyhow!("cannot verify box work: capsule is missing"))?;
    let output = inspect_work(runtime, target, &capsule, false)
        .await
        .context("cannot verify box work")?;
    let work = parse_work(&output)?;
    Ok(WorkCheck {
        unsynced: is_unsynced(record.as_ref(), &work.oid, &work.kind, work.dirty),
        ignored: work.ignored,
    })
}

// DECISION: inject the config-home path into the persistence core so tests never depend on or
// mutate the user's home directory or process environment.
pub(crate) async fn record_synced_with_home(
    target: &BoxRef,
    identity: SyncIdentity,
    config_home: &Path,
) -> Result<()> {
    let index_path = config_home.join("boxes.json");
    let entry = index::entry_for_session_from(&index_path, &target.session)?
        .ok_or_else(|| anyhow!("cannot record sync: box metadata is missing"))?;
    if entry.mode == "mount" {
        return Ok(());
    }

    let state_dir = config_home
        .join("sessions")
        .join(capscode_core::SessionId::from(target.session.clone()).as_str());
    fs::create_dir_all(&state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    write_sync_record(&state_dir, identity)?;
    let mut updated = entry;
    updated.last_synced = index::now();
    index::upsert_at(&index_path, updated)?;
    Ok(())
}

fn write_sync_record(state_dir: &std::path::Path, identity: SyncIdentity) -> Result<()> {
    let record = SyncRecord {
        oid: identity.oid,
        kind: identity.kind,
    };
    let mut temp =
        tempfile::NamedTempFile::new_in(state_dir).context("creating last-synced record")?;
    serde_json::to_writer(&mut temp, &record).context("serializing last-synced record")?;
    temp.write_all(b"\n")
        .context("writing last-synced record")?;
    temp.as_file()
        .sync_all()
        .context("syncing last-synced record")?;
    temp.persist(state_dir.join(LAST_SYNCED))
        .context("replacing last-synced record")?;
    Ok(())
}

pub(crate) async fn workspace_identity(
    session: &str,
    runtime: &dyn RuntimeAdapter,
) -> Result<SyncIdentity> {
    let entry = index::entry_for_session(session)?
        .ok_or_else(|| anyhow!("cannot record sync: box metadata is missing"))?;
    if entry.mode == "mount" {
        bail!("mount-mode boxes do not have a workspace sync identity");
    }
    let target = BoxRef {
        name: entry.name,
        id: entry.id,
        session: entry.session,
    };
    let objects = runtime
        .list_owned()
        .await
        .context("cannot inspect the stopped box workspace")?;
    let capsule = capsule(&objects, &target)
        .ok_or_else(|| anyhow!("cannot inspect the stopped box workspace: capsule is missing"))?;
    if capsule.state.as_deref() == Some("running") {
        bail!("cannot inspect the workspace identity while the box is running");
    }
    stopped_workspace_identity(runtime, &target, &capsule).await
}

async fn stopped_workspace_identity(
    runtime: &dyn RuntimeAdapter,
    target: &BoxRef,
    capsule: &OwnedObject,
) -> Result<SyncIdentity> {
    // Callers read the identity only after a sync brought back everything the stopped workspace
    // holds, so committing that tree here records it as synced. Without the commit, edits newer
    // than the keeper's last autosync tick would leave the tree dirty, and a stopped box (no
    // keeper running) would then read as unsynced forever.
    let output = inspect_work(runtime, target, capsule, true)
        .await
        .context("inspecting the stopped box workspace")?;
    let work = parse_work(&output)?;
    Ok(SyncIdentity {
        oid: work.oid,
        kind: work.kind,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedWork {
    oid: String,
    kind: String,
    dirty: bool,
    ignored: Vec<String>,
}

fn parse_work(bytes: &[u8]) -> Result<ParsedWork> {
    let text =
        std::str::from_utf8(bytes).context("cannot verify box work: invalid check output")?;
    let mut lines = text.lines();
    let kind = lines
        .next()
        .filter(|kind| *kind == "git" || *kind == "shadow")
        .ok_or_else(|| anyhow!("cannot verify box work: repository kind is missing"))?;
    let oid = lines
        .next()
        .filter(|oid| !oid.is_empty())
        .ok_or_else(|| anyhow!("cannot verify box work: repository has no HEAD"))?;
    let mut dirty = false;
    let mut saw_marker = false;
    let mut ignored = Vec::new();
    for line in lines {
        if saw_marker {
            if !line.is_empty() && line != SHADOW_GIT_DIR {
                ignored.push(line.to_owned());
            }
        } else if line == IGNORED_MARKER {
            saw_marker = true;
        } else if !line.is_empty() {
            dirty = true;
        }
    }
    if !saw_marker {
        bail!("cannot verify box work: gitignored-file listing is missing");
    }
    Ok(ParsedWork {
        oid: oid.to_owned(),
        kind: kind.to_owned(),
        dirty,
        ignored,
    })
}

fn is_unsynced(
    record: Option<&SyncRecord>,
    current_oid: &str,
    current_kind: &str,
    dirty: bool,
) -> bool {
    record.is_none_or(|record| record.oid != current_oid || record.kind != current_kind) || dirty
}

async fn inspect_work(
    runtime: &dyn RuntimeAdapter,
    target: &BoxRef,
    capsule: &OwnedObject,
    commit_synced_tree: bool,
) -> Result<Vec<u8>> {
    let shadow_baseline = if commit_synced_tree {
        shadow_commit_script(WORKSPACE)
    } else {
        String::new()
    };
    let command = format!(
        "git -C /workspace rev-parse --is-inside-work-tree >/dev/null 2>&1 && {{ printf 'git\\n'; git -C /workspace rev-parse HEAD && git -C /workspace status --porcelain --untracked-files=all && printf '%s\\n' '{IGNORED_MARKER}' && git -C /workspace ls-files --others --ignored --exclude-standard --directory; }} || {{ {shadow_baseline}printf 'shadow\\n'; if git --git-dir=/workspace/.capscode-autosync.git rev-parse HEAD 2>/dev/null; then :; else printf 'unborn\\n'; fi; git --git-dir=/workspace/.capscode-autosync.git --work-tree=/workspace status --porcelain --untracked-files=all && printf '%s\\n' '{IGNORED_MARKER}' && git --git-dir=/workspace/.capscode-autosync.git --work-tree=/workspace ls-files --others --ignored --exclude-standard --directory; }}"
    );
    let spec = ExecSpec {
        cmd: vec!["sh".into(), "-lc".into(), command],
        env: vec![("GIT_OPTIONAL_LOCKS".into(), "0".into())],
        workdir: Some(PathBuf::from(WORKSPACE)),
        user: Some("agent".into()),
        ..Default::default()
    };
    if capsule.state.as_deref() == Some("running") {
        let output = runtime
            .exec(&ContainerId(capsule.id.clone()), spec)
            .await
            .context("checking running box")?;
        if output.exit_code != 0 {
            bail!(
                "cannot verify: repository check failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(output.stdout);
    }
    stopped_helper(runtime, target, spec, !commit_synced_tree).await
}

/// Shell that commits the whole workspace into the private autosync repository: a baseline
/// commit when the repository is unborn, otherwise a commit only when the tree changed.
/// The repository hides itself through its `info/exclude`, written by the keeper; an exclude
/// pathspec for an ignored path would make `git add` fail.
fn shadow_commit_script(workspace: &str) -> String {
    let git = format!("git --git-dir={workspace}/.capscode-autosync.git --work-tree={workspace}");
    let commit = format!(
        "{git} -c user.name=capsule -c user.email=capsule@localhost -c commit.gpgsign=false -c core.hooksPath=/dev/null commit -q"
    );
    format!(
        "{git} add -A || exit 1; if ! {git} rev-parse -q --verify HEAD >/dev/null; then {commit} --allow-empty -m 'capsule: synced baseline' || exit 1; elif ! {git} diff --cached --quiet; then {commit} -m 'capsule: synced' || exit 1; fi; "
    )
}

async fn stopped_helper(
    runtime: &dyn RuntimeAdapter,
    target: &BoxRef,
    spec: ExecSpec,
    read_only: bool,
) -> Result<Vec<u8>> {
    let config = config::load(None)?;
    let image = config::capsule_all_image(&config)?;
    let volume = VolumeId(format!("capscode-{}-workspace", target.session));
    let helper = runtime
        .create_container(&checker_spec(image, volume, read_only))
        .await
        .context("creating workspace check helper")?;
    let result = async {
        runtime
            .start(&helper)
            .await
            .context("starting workspace check helper")?;
        let output = runtime
            .exec(&helper, spec)
            .await
            .context("checking stopped box workspace")?;
        if output.exit_code != 0 {
            bail!(
                "repository check failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok::<_, anyhow::Error>(output.stdout)
    }
    .await;
    let _ = runtime.stop(&helper, Duration::from_secs(1)).await;
    let remove = runtime.remove(&helper, true).await;
    if let Err(error) = remove {
        return Err(error).context("removing workspace check helper");
    }
    result
}

fn checker_spec(
    image: capscode_core::ImageRef,
    volume: VolumeId,
    read_only: bool,
) -> ContainerSpec {
    ContainerSpec {
        image,
        hostname: "capscode-check".into(),
        env: Vec::new(),
        cmd: Some(vec!["sleep 60".into()]),
        // The image entrypoint performs writable-root setup for a real capsule.
        // A stopped-box checker is read-only and only needs a shell process.
        entrypoint: Some(vec!["/bin/sh".into(), "-c".into()]),
        mounts: vec![Mount::Volume {
            id: volume,
            target: PathBuf::from(WORKSPACE),
            read_only,
        }],
        network: NetworkMode::None,
        published_ports: Vec::new(),
        cap_add: Vec::new(),
        cap_drop: vec!["ALL".into()],
        no_new_privileges: true,
        devices: Vec::new(),
        sysctls: Vec::new(),
        labels: Vec::new(),
        tmpfs_home: false,
        read_only_rootfs: true,
        init: true,
        pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
        memory_bytes: None,
    }
}

fn capsule(objects: &[OwnedObject], target: &BoxRef) -> Option<OwnedObject> {
    objects
        .iter()
        .find(|object| {
            object.kind == OwnedObjectKind::Container
                && object.role.as_deref() == Some(ROLE_CAPSULE)
                && object
                    .session
                    .as_ref()
                    .is_some_and(|session| session.as_str() == target.session)
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use capscode_core::{
        AttachIo, ContainerSpec, ExecOutput, ImageRef, PublishMode, PullProgress,
        RuntimeCapabilities, RuntimeError, RuntimeInfo, RuntimeKind, VolumeSpec,
    };
    use std::sync::Mutex;

    struct MockRuntime {
        stdout: Vec<u8>,
        exec_specs: Mutex<Vec<ExecSpec>>,
        helper_entrypoints: Mutex<Vec<Option<Vec<String>>>>,
    }

    impl MockRuntime {
        fn new(stdout: &[u8]) -> Self {
            Self {
                stdout: stdout.to_vec(),
                exec_specs: Mutex::new(Vec::new()),
                helper_entrypoints: Mutex::new(Vec::new()),
            }
        }

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
        async fn remove_volume(&self, _: &VolumeId) -> std::result::Result<(), RuntimeError> {
            Err(self.unused())
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
            spec: &ContainerSpec,
        ) -> std::result::Result<ContainerId, RuntimeError> {
            self.helper_entrypoints
                .lock()
                .unwrap()
                .push(spec.entrypoint.clone());
            Ok(ContainerId("helper".into()))
        }
        async fn start(&self, _: &ContainerId) -> std::result::Result<(), RuntimeError> {
            Ok(())
        }
        async fn exec(
            &self,
            _: &ContainerId,
            spec: ExecSpec,
        ) -> std::result::Result<ExecOutput, RuntimeError> {
            self.exec_specs.lock().unwrap().push(spec);
            Ok(ExecOutput {
                stdout: self.stdout.clone(),
                stderr: Vec::new(),
                exit_code: 0,
            })
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
            _: &ContainerId,
            _: Duration,
        ) -> std::result::Result<(), RuntimeError> {
            Ok(())
        }
        async fn remove(&self, _: &ContainerId, _: bool) -> std::result::Result<(), RuntimeError> {
            Ok(())
        }
        async fn list_owned(&self) -> std::result::Result<Vec<OwnedObject>, RuntimeError> {
            Ok(Vec::new())
        }
    }

    fn target() -> BoxRef {
        BoxRef {
            name: "alpha-123abc".into(),
            id: "123abc".into(),
            session: "s1".into(),
        }
    }

    fn running_capsule() -> OwnedObject {
        OwnedObject {
            kind: OwnedObjectKind::Container,
            id: "capsule".into(),
            session: Some(capscode_core::SessionId::from("s1".to_owned())),
            role: Some(ROLE_CAPSULE.into()),
            created_at: None,
            state: Some("running".into()),
            image: None,
            labels: Default::default(),
        }
    }

    #[test]
    fn guard_treats_head_change_or_dirty_status_as_unsynced() {
        let clean = parse_work(b"git\na\n--ignored--\n").unwrap();
        assert_eq!((clean.oid.as_str(), clean.kind.as_str()), ("a", "git"));
        assert!(!clean.dirty);
        assert!(clean.ignored.is_empty());
        assert!(
            parse_work(b"shadow\na\n?? file\n--ignored--\n")
                .unwrap()
                .dirty
        );
        let unborn = parse_work(b"shadow\nunborn\n?? file\n--ignored--\n").unwrap();
        assert_eq!(unborn.oid, "unborn");
        assert!(unborn.dirty);
        assert!(parse_work(b"").is_err());
        assert!(
            parse_work(b"git\na\n").is_err(),
            "a missing ignored listing is not clean"
        );
        let record = SyncRecord {
            oid: "a".into(),
            kind: "git".into(),
        };
        assert!(!is_unsynced(Some(&record), "a", "git", false));
        assert!(is_unsynced(Some(&record), "b", "git", false));
        assert!(is_unsynced(Some(&record), "a", "shadow", false));
        assert!(is_unsynced(Some(&record), "a", "git", true));
        assert!(is_unsynced(None, "a", "git", false));
    }

    #[test]
    fn guard_lists_gitignored_files_separately_from_dirty_status() {
        let work =
            parse_work(b"git\na\n--ignored--\n.env\nnode_modules/\n.capscode-autosync.git/\n")
                .unwrap();
        assert!(
            !work.dirty,
            "ignored files alone do not make tracked work unsynced"
        );
        assert_eq!(
            work.ignored,
            vec![".env".to_owned(), "node_modules/".to_owned()]
        );
    }

    #[test]
    fn sync_record_kind_distinguishes_git_and_shadow() {
        let record: SyncRecord = serde_json::from_str(r#"{"oid":"abc","kind":"shadow"}"#).unwrap();
        assert_eq!(record.oid, "abc");
        assert_eq!(record.kind, "shadow");
    }

    #[tokio::test]
    async fn record_synced_core_stores_exported_oid_after_mock_capsule_head_moves() {
        let config_home = tempfile::tempdir().unwrap();
        let index_path = config_home.path().join("boxes.json");
        index::write_to(
            &index_path,
            &[index::Entry {
                name: "alpha-123abc".into(),
                id: "123abc".into(),
                session: "s1".into(),
                mode: "copy".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        let runtime = MockRuntime::new(b"git\ncommit-b\n--ignored--\n");
        let current = inspect_work(&runtime, &target(), &running_capsule(), false)
            .await
            .unwrap();
        assert_eq!(parse_work(&current).unwrap().oid, "commit-b");

        for kind in ["git", "shadow"] {
            record_synced_with_home(
                &target(),
                SyncIdentity {
                    oid: "commit-a".into(),
                    kind: kind.into(),
                },
                config_home.path(),
            )
            .await
            .unwrap();
            let bytes = fs::read(config_home.path().join("sessions/s1").join(LAST_SYNCED)).unwrap();
            let record: SyncRecord = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(record.oid, "commit-a");
            assert_eq!(record.kind, kind);
            assert_ne!(record.oid, parse_work(&current).unwrap().oid);
        }
    }

    #[tokio::test]
    async fn stopped_shadow_identity_commits_the_synced_tree() {
        let runtime = MockRuntime::new(b"shadow\ncommit-baseline\n--ignored--\n");
        // Exercise the generated command in the mock exec path without loading image config from
        // the user's home; workspace_identity calls this helper only after confirming stopped state.
        let identity = stopped_workspace_identity(&runtime, &target(), &running_capsule())
            .await
            .unwrap();

        assert_eq!(identity.kind, "shadow");
        assert_eq!(identity.oid, "commit-baseline");
        let record = SyncRecord {
            oid: identity.oid.clone(),
            kind: identity.kind.clone(),
        };
        assert!(!is_unsynced(
            Some(&record),
            &identity.oid,
            &identity.kind,
            false
        ));
        let command = runtime.exec_specs.lock().unwrap()[0].cmd[2].clone();
        assert!(command.contains(&shadow_commit_script(WORKSPACE)));
        let commit_at = command.find("commit -q").unwrap();
        assert!(
            commit_at < command.find("printf 'shadow\\n'").unwrap(),
            "the identity is read after the commit"
        );
    }

    fn git(workspace: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg(format!(
                "--git-dir={}/.capscode-autosync.git",
                workspace.display()
            ))
            .arg(format!("--work-tree={}", workspace.display()))
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn run_commit_script(workspace: &Path) {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(shadow_commit_script(&workspace.display().to_string()))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn the_commit_script_leaves_a_dirty_autosync_tree_clean() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        git(workspace, &["init", "-q"]);
        // The keeper's setup (images/keeper.sh) hides the repository from its own worktree.
        fs::write(
            workspace.join(".capscode-autosync.git/info/exclude"),
            "/.capscode-autosync.git/\n",
        )
        .unwrap();
        fs::write(workspace.join("a.txt"), "one").unwrap();

        run_commit_script(workspace);
        let baseline = git(workspace, &["rev-parse", "HEAD"]);
        assert_eq!(
            git(
                workspace,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            ""
        );

        // Edits newer than the keeper's last tick: a change, an addition, a deletion.
        fs::write(workspace.join("a.txt"), "two").unwrap();
        fs::write(workspace.join("b.txt"), "new").unwrap();
        run_commit_script(workspace);
        let synced = git(workspace, &["rev-parse", "HEAD"]);
        assert_ne!(synced, baseline);
        assert_eq!(
            git(
                workspace,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            ""
        );

        fs::remove_file(workspace.join("b.txt")).unwrap();
        run_commit_script(workspace);
        assert_eq!(
            git(workspace, &["ls-tree", "-r", "--name-only", "HEAD"]),
            "a.txt\n"
        );

        let clean = git(workspace, &["rev-parse", "HEAD"]);
        run_commit_script(workspace);
        assert_eq!(
            git(workspace, &["rev-parse", "HEAD"]),
            clean,
            "a clean tree gets no new commit"
        );
    }

    #[tokio::test]
    async fn guard_decision_uses_mocked_runtime_output_and_records_shadow_baseline() {
        let runtime = MockRuntime::new(b"shadow\nabc\n--ignored--\n");
        let output = inspect_work(&runtime, &target(), &running_capsule(), true)
            .await
            .unwrap();
        let work = parse_work(&output).unwrap();
        let record = SyncRecord {
            oid: "abc".into(),
            kind: "shadow".into(),
        };
        assert!(!is_unsynced(
            Some(&record),
            &work.oid,
            &work.kind,
            work.dirty
        ));
        let command = runtime.exec_specs.lock().unwrap()[0].cmd[2].clone();
        assert!(command.contains("--work-tree=/workspace add -A"));
        assert!(command.contains("--allow-empty"));
        assert!(command.contains("ls-files --others --ignored --exclude-standard --directory"));
    }

    #[test]
    fn stopped_helper_bypasses_the_image_entrypoint() {
        let spec = checker_spec(
            ImageRef("capsule:all".into()),
            VolumeId("box-workspace".into()),
            true,
        );
        assert_eq!(spec.entrypoint, Some(vec!["/bin/sh".into(), "-c".into()]));
        assert_eq!(spec.cmd, Some(vec!["sleep 60".into()]));
        assert!(spec.read_only_rootfs);
        let writable = checker_spec(
            ImageRef("capsule:all".into()),
            VolumeId("box-workspace".into()),
            false,
        );
        assert!(matches!(
            &writable.mounts[0],
            Mount::Volume {
                read_only: false,
                ..
            }
        ));
    }
}
