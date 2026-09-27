use super::*;
use capscode_core::{
    AttachIo, ContainerId, ContainerSpec, ExecOutput, ImageRef, OwnedObjectKind, PublishMode,
    PullProgress, RuntimeCapabilities, RuntimeError, RuntimeInfo, RuntimeKind, VolumeId,
    VolumeSpec, LABEL_BOX_ID, LABEL_OWNER, LABEL_OWNER_VALUE, LABEL_SESSION, LABEL_WORKSPACE_MODE,
    ROLE_CACHE,
};
use clap::{CommandFactory, Parser};
use std::ffi::OsString;

#[test]
fn doctor_uses_plain_runtime_kind_labels() {
    assert_eq!(runtime_kind_label(RuntimeKind::Docker), "Docker");
    assert_eq!(runtime_kind_label(RuntimeKind::OrbStack), "OrbStack");
    assert_eq!(
        runtime_kind_label(RuntimeKind::AppleContainer),
        "Apple Container"
    );
}

#[test]
fn toolchain_build_no_smoke_flag_reaches_the_image_script() {
    let lock = Path::new("/tmp/toolchain.lock.toml");
    let args = build_script_args(lock, true);
    assert_eq!(args[0], "--lock");
    assert_eq!(args[1], lock.as_os_str());
    assert_eq!(args[2], "--no-smoke");
    assert_eq!(build_script_args(lock, false).len(), 2);
}

// DECISION: endpoint tests use a dedicated lock because existing env locks do not cover Docker CLI variables.
static DOCKER_CLI_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
use std::sync::Mutex;

struct RecordingRuntime {
    objects: Mutex<Vec<OwnedObject>>,
    events: Mutex<Vec<String>>,
    capsule_head: Mutex<Option<String>>,
    fail_next_list: Mutex<bool>,
    stop_error: Option<String>,
}

impl RecordingRuntime {
    fn new(objects: Vec<OwnedObject>) -> Self {
        Self {
            objects: Mutex::new(objects),
            events: Mutex::new(Vec::new()),
            capsule_head: Mutex::new(None),
            fail_next_list: Mutex::new(false),
            stop_error: None,
        }
    }

    fn failing_stop(objects: Vec<OwnedObject>, id: &str) -> Self {
        Self {
            objects: Mutex::new(objects),
            events: Mutex::new(Vec::new()),
            capsule_head: Mutex::new(None),
            fail_next_list: Mutex::new(false),
            stop_error: Some(id.to_owned()),
        }
    }

    fn event_list(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn set_capsule_head(&self, oid: &str) {
        *self.capsule_head.lock().unwrap() = Some(oid.to_owned());
    }

    fn fail_next_list(&self) {
        *self.fail_next_list.lock().unwrap() = true;
    }

    fn unused(&self) -> RuntimeError {
        RuntimeError::CapabilityUnsupported(self.kind(), "unused mock operation".into())
    }
}

#[async_trait::async_trait]
impl RuntimeAdapter for RecordingRuntime {
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
    async fn create_volume(&self, _: &VolumeSpec) -> std::result::Result<VolumeId, RuntimeError> {
        Err(self.unused())
    }
    async fn remove_volume(&self, id: &VolumeId) -> std::result::Result<(), RuntimeError> {
        self.events
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
        id: &ContainerId,
        _: ExecSpec,
    ) -> std::result::Result<ExecOutput, RuntimeError> {
        self.events.lock().unwrap().push(format!("exec:{}", id.0));
        let stdout = self
            .capsule_head
            .lock()
            .unwrap()
            .as_ref()
            .map(|head| format!("git\n{head}\n--ignored--\n").into_bytes())
            .unwrap_or_default();
        Ok(ExecOutput {
            stdout,
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
        id: &ContainerId,
        timeout: std::time::Duration,
    ) -> std::result::Result<(), RuntimeError> {
        self.events
            .lock()
            .unwrap()
            .push(format!("stop:{}:{}", id.0, timeout.as_secs()));
        if self.stop_error.as_deref() == Some(id.0.as_str()) {
            return Err(RuntimeError::Container(
                id.0.clone(),
                "stop timed out".into(),
            ));
        }
        for object in self.objects.lock().unwrap().iter_mut() {
            if object.id == id.0 {
                object.state = Some("exited".into());
            }
        }
        Ok(())
    }
    async fn remove(&self, id: &ContainerId, force: bool) -> std::result::Result<(), RuntimeError> {
        self.events
            .lock()
            .unwrap()
            .push(format!("remove:{}:{force}", id.0));
        Ok(())
    }
    async fn list_owned(&self) -> std::result::Result<Vec<OwnedObject>, RuntimeError> {
        self.events.lock().unwrap().push("list-owned".into());
        let mut fail_next = self.fail_next_list.lock().unwrap();
        if *fail_next {
            *fail_next = false;
            return Err(RuntimeError::Container(
                "mock-list".into(),
                "injected list failure".into(),
            ));
        }
        drop(fail_next);
        Ok(self.objects.lock().unwrap().clone())
    }
}

fn box_runtime_objects() -> Vec<OwnedObject> {
    let session = SessionId::new();
    let labels = |role: Option<&str>| {
        let mut labels = std::collections::BTreeMap::from([
            (LABEL_OWNER.into(), LABEL_OWNER_VALUE.into()),
            (LABEL_BOX.into(), "alpha-123abc".into()),
            (LABEL_BOX_ID.into(), "123abc".into()),
            (LABEL_SESSION.into(), session.to_string()),
            (LABEL_WORKSPACE_MODE.into(), "copy".into()),
        ]);
        if let Some(role) = role {
            labels.insert(capscode_core::LABEL_ROLE.into(), role.into());
        }
        labels
    };
    vec![
        OwnedObject {
            kind: OwnedObjectKind::Container,
            id: "capsule".into(),
            session: Some(session.clone()),
            role: Some(ROLE_CAPSULE.into()),
            created_at: None,
            state: Some("running".into()),
            image: Some("capscode/capsule-all:fb24882402b5".into()),
            labels: labels(Some(ROLE_CAPSULE)),
        },
        OwnedObject {
            kind: OwnedObjectKind::Container,
            id: "gateway".into(),
            session: Some(session.clone()),
            role: Some(ROLE_GATEWAY.into()),
            created_at: None,
            state: Some("running".into()),
            image: Some("capscode/capsule-all:fb24882402b5".into()),
            labels: labels(Some(ROLE_GATEWAY)),
        },
        OwnedObject {
            kind: OwnedObjectKind::Volume,
            id: "workspace-volume".into(),
            session: Some(session.clone()),
            role: None,
            created_at: None,
            state: None,
            image: None,
            labels: labels(None),
        },
        OwnedObject {
            kind: OwnedObjectKind::Volume,
            id: "cache-volume".into(),
            session: Some(session.clone()),
            role: Some(ROLE_CACHE.into()),
            created_at: None,
            state: None,
            image: None,
            labels: labels(Some(ROLE_CACHE)),
        },
    ]
}

#[test]
fn exact_commands_and_tool_aliases_precede_box_lookup() {
    use crate::args::{Cli, Command, Invocation};

    let exact = Cli::try_parse_from(["capscode", "ls"])
        .unwrap()
        .command
        .unwrap();
    assert!(matches!(exact, Command::Ls { .. }));

    let alias = Cli::try_parse_from(["capscode", "claude", "--", "hello"])
        .unwrap()
        .command
        .unwrap()
        .into_invocation()
        .unwrap();
    assert!(matches!(alias, Invocation::RunTool(_)));
}

#[test]
fn ls_keeps_one_shot_sessions_and_preserved_volumes_but_filters_box_sessions() {
    let one_shot = SessionId::from("00000000-0000-0000-0000-000000000001".to_string());
    let box_session = SessionId::from("00000000-0000-0000-0000-000000000002".to_string());
    let box_sessions = HashSet::from([box_session.clone()]);
    let object = |kind, session: SessionId, role: Option<&str>| OwnedObject {
        kind,
        id: "owned-object".to_string(),
        session: Some(session),
        role: role.map(str::to_string),
        created_at: None,
        state: None,
        image: None,
        labels: Default::default(),
    };

    assert!(session_object_is_listable(
        &object(
            OwnedObjectKind::Container,
            one_shot.clone(),
            Some(ROLE_CAPSULE)
        ),
        &box_sessions
    ));
    assert!(session_object_is_listable(
        &object(OwnedObjectKind::Volume, one_shot, None),
        &box_sessions
    ));
    assert!(!session_object_is_listable(
        &object(
            OwnedObjectKind::Container,
            box_session.clone(),
            Some(ROLE_CAPSULE)
        ),
        &box_sessions
    ));
    assert!(!session_object_is_listable(
        &object(OwnedObjectKind::Container, box_session, Some("gateway")),
        &box_sessions
    ));
}

#[test]
fn removed_commands_print_the_new_command_pointer() {
    use crate::args::{Cli, Invocation};

    for (args, expected) in [
        (vec!["capscode", "run", "claude"], "run is now"),
        (vec!["capscode", "run"], "run is now"),
        (vec!["capscode", "status"], "doctor --quick"),
        (vec!["capscode", "auth", "import"], "key set anthropic"),
        (vec!["capscode", "provider", "key", "ls"], "key ls"),
        (
            vec!["capscode", "provider", "set-default", "codex", "openai"],
            "config set defaults",
        ),
        (vec!["capscode", "vpn", "status"], "bare `vpn`"),
        (vec!["capscode", "attach"], "capscode <box>"),
        (vec!["capscode", "console"], "capscode ls"),
        (vec!["capscode", "start", "demo"], "resume <box>"),
    ] {
        let result = Cli::try_parse_from(args.clone())
            .unwrap()
            .command
            .unwrap()
            .into_invocation()
            .unwrap();
        assert!(
            matches!(&result, Invocation::Removed(pointer) if pointer.contains(expected)),
            "{args:?}: {result:?}"
        );
    }
}

#[test]
fn main_help_hides_retired_commands_and_shows_the_three_modes() {
    let help = crate::args::Cli::command().render_help().to_string();
    for retired in ["run", "attach", "console", "status", "completions"] {
        assert!(
            !help
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{retired} "))),
            "{retired} appeared in help: {help}"
        );
    }
    assert!(help.contains("capscode <tool> [args] run one tool once"));
    assert!(help.contains("capscode [<box>] enter a box (this directory's box by default)"));
    assert!(help.contains("capscode create|ls|stop|resume|sync|rm manage boxes"));
}

#[test]
fn did_you_mean_covers_tools_and_subcommands_with_distance_two() {
    assert_eq!(
        crate::args::did_you_mean("claud").as_deref(),
        Some("claude")
    );
    assert_eq!(
        crate::args::did_you_mean("creat").as_deref(),
        Some("create")
    );
    assert_eq!(crate::args::did_you_mean("zzzzzz"), None);
}

#[test]
fn unsupported_only_file_sync_handoff_keeps_volume_and_export_during_rm() {
    let temp = tempfile::tempdir().unwrap();
    let session = SessionId::from("00000000-0000-0000-0000-000000000003".to_string());
    let export = temp.path().join("export/link");
    std::fs::create_dir_all(export.parent().unwrap()).unwrap();
    std::fs::write(&export, "unsupported symlink target metadata").unwrap();

    let mut selected = vec![
        OwnedObject {
            kind: OwnedObjectKind::Container,
            id: "capsule".to_string(),
            session: Some(session.clone()),
            role: Some(ROLE_CAPSULE.to_string()),
            created_at: None,
            state: Some("exited".to_string()),
            image: None,
            labels: Default::default(),
        },
        OwnedObject {
            kind: OwnedObjectKind::Volume,
            id: "workspace-volume".to_string(),
            session: Some(session.clone()),
            role: None,
            created_at: None,
            state: None,
            image: None,
            labels: Default::default(),
        },
    ];
    // `rm` removes the capsule but excludes the volume and its export state from cleanup.
    preserve_uncaptured_workspace(&mut selected);

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].kind, OwnedObjectKind::Container);
    assert_eq!(
        std::fs::read_to_string(export).unwrap(),
        "unsupported symlink target metadata"
    );
}
use std::cell::Cell;

#[test]
fn session_path_arguments_reject_non_uuid_paths() {
    for invalid in ["/", "../x", "/tmp/x"] {
        assert!(
            validate_session_id(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(validate_session_id("12345678-abcd-4def-8123-123456789abc").is_ok());
}

#[test]
fn session_state_removal_target_must_be_a_direct_child_of_sessions() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions");
    let id = SessionId::from("12345678-abcd-4def-8123-123456789abc".to_string());
    let state = sessions.join(id.as_str());
    fs::create_dir_all(&state).unwrap();
    assert_eq!(
        canonical_session_state_dir(&id, &state).unwrap(),
        Some(state.canonicalize().unwrap())
    );
    assert!(canonical_session_state_dir(&id, &sessions).is_err());
}

#[test]
fn session_state_cleanup_lists_staged_backup_and_previous_handoff_files() {
    let state = tempfile::tempdir().unwrap();
    let staging_file = state.path().join("staging/conflicts/file with spaces.txt");
    let backup_file = state.path().join("backup/overwritten.txt");
    let previous_file = state.path().join("previous-handoff.1/export/old.txt");
    for path in [&staging_file, &backup_file, &previous_file] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"preserve me").unwrap();
    }

    let paths = session_state_handoff_data(state.path()).unwrap();
    assert_eq!(
        paths,
        vec![
            PathBuf::from("backup/overwritten.txt"),
            PathBuf::from("previous-handoff.1/export/old.txt"),
            PathBuf::from("staging/conflicts/file with spaces.txt"),
        ]
    );
}

#[test]
fn custom_provider_without_default_model_resolves_for_claude() {
    let spec = provider_spec_from_args(ProviderAddArgs {
        id: Some("acme".to_string()),
        base_url: Some("https://llm.internal.example/prod/gateway".to_string()),
        api_style: Some(ApiStyleArg::Anthropic),
        key_env: Some("ACME_TOKEN".to_string()),
        default_model: None,
        auth: Some(AuthSchemeArg::Bearer),
        experimental: false,
    })
    .unwrap();

    assert_eq!(spec.auth, AuthScheme::Bearer);
    assert_eq!(spec.default_model, None);
}

#[test]
fn provider_add_non_tty_errors_name_the_public_flags() {
    let missing_id = provider_spec_from_args(ProviderAddArgs {
        id: None,
        base_url: None,
        api_style: None,
        key_env: None,
        default_model: None,
        auth: None,
        experimental: false,
    })
    .unwrap_err();
    assert!(missing_id.to_string().contains("--id"));

    let missing_key_env = provider_spec_from_args(ProviderAddArgs {
        id: Some("acme".to_string()),
        base_url: Some("https://api.example.test/v1".to_string()),
        api_style: Some(ApiStyleArg::Openai),
        key_env: None,
        default_model: None,
        auth: None,
        experimental: false,
    })
    .unwrap_err();
    assert!(missing_key_env.to_string().contains("--key-env"));
}

#[test]
fn provider_table_aligns_long_ids_without_tabs() {
    let rows = vec![
        [
            "anthropic".to_string(),
            "anthropic".to_string(),
            "yes".to_string(),
            "https://api.anthropic.com".to_string(),
        ],
        [
            "zai-coding-cn-openai".to_string(),
            "openai".to_string(),
            "no".to_string(),
            "https://open.bigmodel.cn/api/coding/paas/v4 (experimental)".to_string(),
        ],
    ];

    let output = format_provider_table(&rows);
    assert!(!output.contains('\t'));
    let endpoint_column = output
        .lines()
        .map(|line| {
            line.find("https://")
                .unwrap_or_else(|| line.find("ENDPOINT").unwrap())
        })
        .collect::<Vec<_>>();
    assert!(endpoint_column
        .windows(2)
        .all(|window| window[0] == window[1]));
}

#[test]
fn provider_list_separates_built_ins_from_user_providers() {
    let built_in = vec![[
        "anthropic".to_string(),
        "anthropic".to_string(),
        "yes".to_string(),
        "https://api.anthropic.com".to_string(),
    ]];
    let user = vec![[
        "acme".to_string(),
        "anthropic".to_string(),
        "no".to_string(),
        "https://llm.internal.example/prod/gateway/v1".to_string(),
    ]];

    let output = format_provider_list(&built_in, &user);
    assert!(output.starts_with("BUILT-IN PROVIDERS\n"));
    assert!(output.contains("\n\nUSER-DEFINED PROVIDERS\n"));
    assert!(output.find("anthropic").unwrap() < output.find("acme").unwrap());
}

/// A saved file-sync receipt must be recognized as a non-Git workspace,
/// three values the handoff needs — which is why `sync` and `rm` could only
/// refuse. It must now carry them through intact.
#[test]
fn sync_classifies_a_non_git_session_receipt_and_keeps_its_handoff_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let oid = "0123456789012345678901234567890123456789";
    let receipt = IngressReceipt::Shadow {
        git_dir: temp.path().join("shadow.git"),
        baseline: oid.to_string(),
        state_dir: temp.path().to_path_buf(),
    };
    assert_eq!(
        classify_sync_receipt(Some(receipt)).unwrap(),
        GitSyncReceipt::Shadow {
            git_dir: temp.path().join("shadow.git"),
            baseline: oid.parse::<git2::Oid>().unwrap(),
            state_dir: temp.path().to_path_buf(),
        }
    );
}

/// `can_copy_back` drives the console's confirmation wording. Both receipt
/// kinds can be copied back — by different routes — and only a missing one
/// cannot. Getting this wrong means promising a copy-back that never happens.
#[test]
fn both_receipt_kinds_can_be_copied_back_but_a_missing_one_cannot() {
    let temp = tempfile::tempdir().unwrap();
    let oid = "0123456789012345678901234567890123456789";
    assert!(GitSyncReceipt::Git { baseline: None }.can_copy_back());
    assert!(GitSyncReceipt::Shadow {
        git_dir: temp.path().join("shadow.git"),
        baseline: oid.parse::<git2::Oid>().unwrap(),
        state_dir: temp.path().to_path_buf(),
    }
    .can_copy_back());
    assert!(!GitSyncReceipt::Missing.can_copy_back());
}

/// An invalid baseline must fail loudly at classification, not surface later
/// as a confusing failure deep inside the handoff.
#[test]
fn a_shadow_receipt_with_an_unparseable_baseline_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let receipt = IngressReceipt::Shadow {
        git_dir: temp.path().join("shadow.git"),
        baseline: "not-an-oid".to_string(),
        state_dir: temp.path().to_path_buf(),
    };
    assert!(classify_sync_receipt(Some(receipt)).is_err());
}

#[test]
fn sync_classifies_a_git_snapshot_and_an_absent_receipt() {
    let oid = "0123456789012345678901234567890123456789";
    let git = classify_sync_receipt(Some(IngressReceipt::GitSnapshot {
        baseline: Some(oid.to_string()),
    }))
    .unwrap();
    assert_eq!(
        git,
        GitSyncReceipt::Git {
            baseline: Some(oid.parse::<git2::Oid>().unwrap())
        }
    );
    assert_eq!(
        classify_sync_receipt(None).unwrap(),
        GitSyncReceipt::Missing
    );
}

#[test]
fn stopped_worktree_checkpoint_is_created_without_running_git_hooks() {
    let temp = tempfile::tempdir().unwrap();
    let repo_path = temp.path().join("workspace");
    fs::create_dir(&repo_path).unwrap();
    let repository = Repository::init(&repo_path).unwrap();
    fs::write(repo_path.join("tracked.txt"), "baseline\n").unwrap();
    fs::write(repo_path.join("deleted.txt"), "remove me\n").unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    index.add_path(Path::new("deleted.txt")).unwrap();
    index.write().unwrap();
    let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("capsule", "capsule@localhost").unwrap();
    let baseline = repository
        .commit(Some("HEAD"), &signature, &signature, "baseline", &tree, &[])
        .unwrap();

    fs::write(repo_path.join("tracked.txt"), "edited\n").unwrap();
    fs::write(repo_path.join("new.txt"), "new\n").unwrap();
    fs::write(repo_path.join(".hidden-new.txt"), "hidden\n").unwrap();
    fs::remove_file(repo_path.join("deleted.txt")).unwrap();
    let commit = commit_worktree_snapshot(&repository, "capsule: sync WIP")
        .unwrap()
        .unwrap();
    assert_eq!(
        repository
            .find_commit(commit)
            .unwrap()
            .parent_id(0)
            .unwrap(),
        baseline
    );
    let tree = repository.find_commit(commit).unwrap().tree().unwrap();
    assert!(tree.get_path(Path::new("tracked.txt")).is_ok());
    assert!(tree.get_path(Path::new("new.txt")).is_ok());
    assert!(tree.get_path(Path::new(".hidden-new.txt")).is_ok());
    assert!(tree.get_path(Path::new("deleted.txt")).is_err());

    let marker = repository.path().join("MERGE_HEAD");
    fs::write(&marker, "pending").unwrap();
    assert_eq!(
        commit_worktree_snapshot(&repository, "must be skipped").unwrap(),
        None
    );
}

#[test]
fn stopped_git_snapshot_fetches_checkpoint_into_session_refs() {
    let temp = tempfile::tempdir().unwrap();
    let host_path = temp.path().join("host");
    fs::create_dir(&host_path).unwrap();
    let host = Repository::init(&host_path).unwrap();
    fs::write(host_path.join("tracked.txt"), "baseline\n").unwrap();
    let mut index = host.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    index.write().unwrap();
    let tree = host.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("host", "host@localhost").unwrap();
    let baseline = host
        .commit(Some("HEAD"), &signature, &signature, "baseline", &tree, &[])
        .unwrap();

    let source_path = temp.path().join("stopped-workspace");
    let source = Repository::clone(host_path.to_str().unwrap(), &source_path).unwrap();
    source
        .reference("refs/capscode/baseline", baseline, true, "test baseline")
        .unwrap();
    fs::write(source_path.join("tracked.txt"), "edited\n").unwrap();
    fs::write(source_path.join("new.txt"), "new\n").unwrap();
    commit_worktree_snapshot(&source, "capsule: sync WIP").unwrap();

    let session = SessionId::from("00000000-0000-0000-0000-000000000002".to_string());
    let report =
        fetch_stopped_git_snapshot(&host_path, &source, &session, Some(baseline), false).unwrap();
    assert_eq!(report.base_sha, baseline);
    assert_eq!(report.commits, 1);
    assert_eq!(report.files_changed, 2);
    assert!(host
        .find_reference(&format!("refs/capscode/{}/head", session.as_str()))
        .is_ok());
    assert_eq!(
        host.find_reference(&format!("refs/capscode/{}/baseline", session.as_str()))
            .unwrap()
            .target(),
        Some(baseline)
    );
}

#[test]
fn stopped_git_recovery_rejects_a_moved_capsule_baseline_ref() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let signature = git2::Signature::now("test", "test@example.invalid").unwrap();
    let tree = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree).unwrap();
    let expected = repo
        .commit(
            None,
            &signature,
            &signature,
            "expected baseline",
            &tree,
            &[],
        )
        .unwrap();
    let moved = repo
        .commit(None, &signature, &signature, "moved baseline", &tree, &[])
        .unwrap();
    let session = SessionId::from("00000000-0000-0000-0000-000000000004".to_string());
    repo.reference(
        &format!("refs/capscode/{}/baseline", session.as_str()),
        moved,
        true,
        "moved capsule baseline",
    )
    .unwrap();

    let error = verify_stopped_git_baseline(&repo, &session, Some(expected)).unwrap_err();
    assert!(error.to_string().contains("does not match"));
}

#[test]
fn storing_a_provider_key_enables_keychain_reads_after_write() {
    let spec = config::providers::load_registry()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == "openai")
        .unwrap();
    let written = Cell::new(false);
    let enabled = Cell::new(false);
    store_provider_key(
        &spec,
        "sk-test-provider-key",
        |id, value| {
            assert_eq!(id, "openai");
            assert_eq!(value, "sk-test-provider-key");
            written.set(true);
            Ok(())
        },
        || {
            enabled.set(true);
            Ok(())
        },
    )
    .unwrap();
    assert!(written.get());
    assert!(enabled.get());
}

#[test]
fn provider_key_bundle_rejects_anthropic() {
    let spec = config::providers::load_registry()
        .unwrap()
        .into_iter()
        .find(|provider| provider.id == "anthropic")
        .unwrap();
    let error = ensure_provider_key_supported(&spec)
        .unwrap_err()
        .to_string();
    assert!(error.contains("capscode key set anthropic"), "{error}");
    assert!(!error.contains("sk-"), "{error}");
}

#[test]
fn guarded_config_home_with_config_is_removed() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("custom-capscode-home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("config.toml"), "[network]\n").unwrap();

    assert!(looks_like_capscode_home(&home));
    assert_eq!(
        remove_config_home(&home).unwrap(),
        ConfigHomeRemoval::Removed
    );
    assert!(!home.exists());
}

#[test]
fn guarded_config_home_without_marker_is_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("unrelated-home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("notes.txt"), "keep\n").unwrap();

    assert!(!looks_like_capscode_home(&home));
    assert_eq!(
        remove_config_home(&home).unwrap(),
        ConfigHomeRemoval::NotCapscodeHome
    );
    assert_eq!(
        fs::read_to_string(home.join("notes.txt")).unwrap(),
        "keep\n"
    );
}

#[cfg(unix)]
#[test]
fn config_home_that_is_a_symlink_is_never_removed() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real-capscode");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("config.toml"), "[network]\n").unwrap();
    let link = temp.path().join("link-home");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    assert_eq!(
        remove_config_home(&link).unwrap(),
        ConfigHomeRemoval::Unsafe
    );
    assert!(real.join("config.toml").is_file());
}

#[test]
fn vpn_import_rejects_invalid_config_without_writing_user_config() {
    let home = tempfile::tempdir().unwrap();
    let wg_path = home.path().join("invalid.conf");
    fs::write(
        &wg_path,
        "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 1.1.1.1\n",
    )
    .unwrap();
    let config_path = home.path().join(".capscode/config.toml");

    let error = vpn_import_at(&wg_path, &config_path).unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("invalid WireGuard config"));
    assert!(message.contains("no [Peer]"));
    assert!(!config_path.exists());
}

#[test]
fn egress_intent_describes_direct_modes() {
    let direct = capscode_core::NetworkConfig::default();
    assert_eq!(
            egress_intent_line(&direct),
            "Egress: DIRECT — via your host IP (containment-only). Add a VPN: capscode vpn import <provider>.conf"
        );

    let imported = capscode_core::NetworkConfig {
        wireguard_conf: Some(PathBuf::from("provider.conf")),
        ..direct
    };
    assert_eq!(
        egress_intent_line(&imported),
        "Egress: DIRECT — VPN imported but not enforced (host IP)"
    );
}

#[tokio::test]
async fn rm_core_stops_before_copy_back_and_every_removal() {
    let objects = box_runtime_objects();
    let runtime = RecordingRuntime::new(objects.clone());
    let session = objects[0].session.clone().unwrap();
    let runtime_events = &runtime.events;
    let clean = rm_session_core(
        &runtime,
        &objects,
        &session,
        true,
        false,
        |stopped| async move {
            let capsule = stopped
                .iter()
                .find(|object| object.role.as_deref() == Some(ROLE_CAPSULE))
                .unwrap();
            assert!(!capsule_is_running(capsule));
            runtime_events
                .lock()
                .unwrap()
                .push("copy-back:stopped".into());
            Ok(RmSessionWork::Captured)
        },
        || Ok(()),
    )
    .await
    .unwrap();
    assert!(clean);

    let events = runtime.event_list();
    assert_eq!(
        events[0],
        format!(
            "stop:capsule:{}",
            crate::boxes::SESSION_STOP_GRACE.as_secs()
        )
    );
    assert_eq!(
        events[1],
        format!(
            "stop:gateway:{}",
            crate::boxes::SESSION_STOP_GRACE.as_secs()
        )
    );
    let copy = events
        .iter()
        .position(|event| event == "copy-back:stopped")
        .unwrap();
    assert!(events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.starts_with("stop:"))
        .all(|(index, _)| index < copy));
    assert!(events
        .iter()
        .any(|event| event == "remove-volume:workspace-volume"));
    assert!(events
        .iter()
        .any(|event| event == "remove-volume:cache-volume"));
    assert!(events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.starts_with("remove:") || event.starts_with("remove-volume:"))
        .all(|(index, _)| index > copy));
}

#[test]
fn rm_selects_only_the_exact_boxes_cache_volume_and_never_a_workspace_by_prefix() {
    let objects = box_runtime_objects();
    let session = objects[0].session.as_ref().unwrap().to_string();
    let target = crate::boxes::BoxRef {
        name: "alpha-123abc".into(),
        id: "123abc".into(),
        session: session.clone(),
    };
    let mut similar_cache = objects
        .iter()
        .find(|object| object.role.as_deref() == Some(ROLE_CACHE))
        .unwrap()
        .clone();
    similar_cache.id = "capscode-similar-cache".into();
    similar_cache
        .labels
        .insert(LABEL_BOX.into(), "alpha-123abc2".into());
    similar_cache
        .labels
        .insert(LABEL_BOX_ID.into(), "123ab2".into());
    let mut workspace = objects
        .iter()
        .find(|object| object.id == "workspace-volume")
        .unwrap()
        .clone();
    workspace.id = format!("capscode-{session}-workspace");

    let mut inventory = objects;
    inventory.push(similar_cache);
    inventory.push(workspace);
    let selected = select_rm_objects(&inventory, Some(&session), false, Some(&target));
    let ids = selected
        .iter()
        .map(|object| object.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"cache-volume"));
    assert!(!ids.contains(&"capscode-similar-cache"));
    assert!(ids.contains(&format!("capscode-{session}-workspace").as_str()));
}

#[test]
fn uncaptured_rm_keeps_workspace_out_of_teardown_but_keeps_cache_selected() {
    let mut selected = box_runtime_objects();
    preserve_uncaptured_workspace(&mut selected);
    assert!(!selected
        .iter()
        .any(|object| object.id == "workspace-volume"));
    assert!(selected.iter().any(|object| object.id == "cache-volume"));
}

#[tokio::test]
async fn rm_force_still_stops_gracefully() {
    let objects = box_runtime_objects();
    let runtime = RecordingRuntime::new(objects.clone());
    let session = objects[0].session.clone().unwrap();
    rm_session_core(
        &runtime,
        &objects,
        &session,
        true,
        true,
        |_| async { Ok(RmSessionWork::Discarded) },
        || Ok(()),
    )
    .await
    .unwrap();

    let events = runtime.event_list();
    assert_eq!(
        events[0],
        format!(
            "stop:capsule:{}",
            crate::boxes::SESSION_STOP_GRACE.as_secs()
        )
    );
    assert_eq!(
        events[1],
        format!(
            "stop:gateway:{}",
            crate::boxes::SESSION_STOP_GRACE.as_secs()
        )
    );
    let capsule_stop = events
        .iter()
        .position(|event| event.starts_with("stop:capsule:"))
        .unwrap();
    let capsule_remove = events
        .iter()
        .position(|event| event == "remove:capsule:true")
        .unwrap();
    assert!(capsule_stop < capsule_remove);
}

#[tokio::test]
async fn rm_keeps_volume_and_containers_when_stop_fails() {
    let objects = box_runtime_objects();
    let runtime = RecordingRuntime::failing_stop(objects.clone(), "capsule");
    let session = objects[0].session.clone().unwrap();
    let error = rm_session_core(
        &runtime,
        &objects,
        &session,
        true,
        false,
        |_| async { Ok(RmSessionWork::Captured) },
        || Ok(()),
    )
    .await
    .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("nothing was removed"));
    assert!(message.contains("capscode rm"));
    assert!(message.contains("--discard-workspace"));
    assert!(message.contains("stop timed out"));
    let events = runtime.event_list();
    assert!(events.iter().all(|event| event.starts_with("stop:")));
    assert!(!events.iter().any(|event| event.starts_with("remove:")));
    assert!(!events
        .iter()
        .any(|event| event.starts_with("remove-volume:")));
}

#[tokio::test]
async fn rm_discarding_the_workspace_removes_even_when_stop_fails() {
    let objects = box_runtime_objects();
    let runtime = RecordingRuntime::failing_stop(objects.clone(), "capsule");
    let session = objects[0].session.clone().unwrap();
    rm_session_core(
        &runtime,
        &objects,
        &session,
        true,
        true,
        |_| async { Ok(RmSessionWork::Discarded) },
        || Ok(()),
    )
    .await
    .unwrap();

    let events = runtime.event_list();
    assert!(events.iter().any(|event| event == "remove:capsule:true"));
    assert!(events
        .iter()
        .any(|event| event.starts_with("remove-volume:")));
}

#[tokio::test]
async fn uninstall_relists_stops_rechecks_and_refuses_before_removal() {
    let objects = box_runtime_objects();
    let runtime = RecordingRuntime::new(objects.clone());
    let temp = tempfile::tempdir().unwrap();
    let index_path = temp.path().join("boxes.json");

    // This is the first listing uninstall performs for its plan; inject its failure, then drive
    // the same runtime core used after confirmation.
    runtime.fail_next_list();
    assert!(runtime.list_owned().await.is_err());
    let error = uninstall_runtime_core(&runtime, &index_path, false)
        .await
        .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("uninstall stopped 1 box(es)"));
    assert!(message.contains("alpha-123abc"));
    assert!(message.contains("capscode sync <box>"));
    assert!(message.contains("works on a stopped box"));
    assert!(message.contains("boxes were stopped, not removed"));
    assert!(message.contains("capscode resume <box>"));
    let events = runtime.event_list();
    assert_eq!(events[1], "list-owned");
    assert_eq!(
        events[2],
        format!(
            "stop:capsule:{}",
            crate::boxes::SESSION_STOP_GRACE.as_secs()
        )
    );
    assert_eq!(
        events[3],
        format!(
            "stop:gateway:{}",
            crate::boxes::SESSION_STOP_GRACE.as_secs()
        )
    );
    assert_eq!(events[4], "list-owned");
    assert!(!events.iter().any(|event| event.starts_with("remove:")));
    assert!(!events
        .iter()
        .any(|event| event.starts_with("remove-volume:")));
}

#[tokio::test]
async fn uninstall_stops_before_teardown() {
    let objects = box_runtime_objects();
    let runtime = RecordingRuntime::new(objects.clone());
    let temp = tempfile::tempdir().unwrap();
    let index_path = temp.path().join("boxes.json");
    uninstall_runtime_core(&runtime, &index_path, true)
        .await
        .unwrap();

    let events = runtime.event_list();
    let capsule_stop = events
        .iter()
        .position(|event| event.starts_with("stop:capsule:"))
        .unwrap();
    let gateway_stop = events
        .iter()
        .position(|event| event.starts_with("stop:gateway:"))
        .unwrap();
    let first_remove = events
        .iter()
        .position(|event| event.starts_with("remove:"))
        .unwrap();
    assert!(capsule_stop < gateway_stop);
    assert!(gateway_stop < first_remove);
    assert!(events
        .iter()
        .any(|event| event.starts_with("remove-volume:")));
}

#[tokio::test]
async fn uninstall_removes_cache_inventory_without_running_the_workspace_guard() {
    let cache = box_runtime_objects()
        .into_iter()
        .find(|object| object.role.as_deref() == Some(ROLE_CACHE))
        .unwrap();
    let runtime = RecordingRuntime::new(vec![cache]);
    let temp = tempfile::tempdir().unwrap();
    let index_path = temp.path().join("boxes.json");

    uninstall_runtime_core(&runtime, &index_path, false)
        .await
        .unwrap();

    let events = runtime.event_list();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.as_str() == "list-owned")
            .count(),
        2
    );
    assert!(events
        .iter()
        .any(|event| event == "remove-volume:cache-volume"));
    assert!(!events.iter().any(|event| event.starts_with("stop:")));
}

#[tokio::test]
async fn sync_flow_records_the_exported_identity_after_capsule_head_moves() {
    let objects = box_runtime_objects();
    let session = objects[0].session.as_ref().unwrap().to_string();
    let runtime = RecordingRuntime::new(objects);
    let target = crate::boxes::BoxRef {
        name: "alpha-123abc".into(),
        id: "123abc".into(),
        session: session.clone(),
    };
    let config_home = tempfile::tempdir().unwrap();
    let index_path = config_home.path().join("boxes.json");
    crate::boxes::index::write_to(
        &index_path,
        &[crate::boxes::index::Entry {
            name: target.name.clone(),
            id: target.id.clone(),
            session: target.session.clone(),
            mode: "copy".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    let exported = crate::boxes::SyncIdentity {
        oid: "commit-a".into(),
        kind: "git".into(),
    };

    sync_then_record(
        || async {
            runtime.set_capsule_head("commit-b");
            Ok(Some(exported.clone()))
        },
        |identity| async {
            crate::boxes::record_synced_with_home(&target, identity, config_home.path()).await
        },
    )
    .await
    .unwrap();

    assert_eq!(
        runtime.capsule_head.lock().unwrap().as_deref(),
        Some("commit-b")
    );
    let bytes = fs::read(
        config_home
            .path()
            .join("sessions")
            .join(session)
            .join("last-synced.json"),
    )
    .unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(receipt["oid"], "commit-a");
    assert_eq!(receipt["kind"], "git");
}

#[tokio::test]
async fn shadow_handoff_is_applied_before_identity_read_and_identity_failure_is_nonfatal() {
    let events = std::sync::Mutex::new(Vec::new());
    let (outcome, identity) = apply_shadow_handoff_then_read_identity(
        true,
        || {
            events.lock().unwrap().push("apply".to_owned());
            Ok(ShadowOutcome::Applied)
        },
        || async {
            events.lock().unwrap().push("identity".to_owned());
            Err(anyhow::anyhow!("mock identity read failed"))
        },
    )
    .await
    .unwrap();

    assert_eq!(outcome, ShadowOutcome::Applied);
    assert!(identity.is_none());
    assert_eq!(*events.lock().unwrap(), ["apply", "identity"]);
}

#[test]
fn rm_messages_name_classic_sessions_and_only_offer_box_resume() {
    let objects = box_runtime_objects();
    let session = objects[0].session.clone().unwrap();
    assert_eq!(rm_target_label(&objects, &session), "box alpha-123abc");
    assert!(rm_resume_hint(&objects)
        .unwrap()
        .contains("capscode resume alpha-123abc"));

    let classic = objects
        .into_iter()
        .map(|mut object| {
            object.labels.remove(LABEL_BOX);
            object
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rm_target_label(&classic, &session),
        format!("session {session}")
    );
    assert!(rm_resume_hint(&classic).is_none());
}

#[test]
fn stopped_git_copy_tolerates_stale_index_lock() {
    let temp = tempfile::tempdir().unwrap();
    let host_repo = temp.path().join("host");
    let exported_copy = temp.path().join("exported-copy");
    let host = Repository::init(&host_repo).unwrap();
    let exported = Repository::init(&exported_copy).unwrap();
    let host_lock = host_repo.join(".git/index.lock");
    let exported_lock = exported_copy.join(".git/index.lock");
    fs::write(&host_lock, "host lock").unwrap();
    fs::write(&exported_lock, "stale lock from stopped capsule").unwrap();

    remove_stale_exported_index_lock(&exported_copy).unwrap();
    exported.index().unwrap().write().unwrap();

    assert!(!exported_lock.exists());
    assert!(host_lock.exists());
    drop(host);
}

#[cfg(unix)]
#[test]
fn stale_index_lock_removal_refuses_a_symlinked_git_dir() {
    let temp = tempfile::tempdir().unwrap();
    let user_repo = temp.path().join("user-repo");
    let exported_copy = temp.path().join("exported-copy");
    Repository::init(&user_repo).unwrap();
    fs::create_dir(&exported_copy).unwrap();
    std::os::unix::fs::symlink(user_repo.join(".git"), exported_copy.join(".git")).unwrap();
    let user_lock = user_repo.join(".git/index.lock");
    fs::write(&user_lock, "live lock held by the user's git").unwrap();

    assert!(remove_stale_exported_index_lock(&exported_copy).is_err());
    assert!(user_lock.exists());
}

#[test]
fn sync_identity_uses_the_git_head_ref_fetched_into_the_host() {
    let temp = tempfile::tempdir().unwrap();
    let repository = Repository::init(temp.path()).unwrap();
    let session = SessionId::new();
    let tree_id = repository.treebuilder(None).unwrap().write().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("capsule", "capsule@localhost").unwrap();
    let head_ref = format!("refs/capscode/{}/head", session.as_str());
    let exported = repository
        .commit(
            Some(&head_ref),
            &signature,
            &signature,
            "exported",
            &tree,
            &[],
        )
        .unwrap();

    assert_eq!(fetched_git_head(temp.path(), &session).unwrap(), exported);
}

#[test]
fn egress_intent_names_the_wireguard_exit_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let configured = temp.path().join("provider.conf");
    fs::write(
            &configured,
            "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 1.1.1.1\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = 1.1.1.1:51820\nAllowedIPs = 0.0.0.0/0\n",
        )
        .unwrap();
    let network = capscode_core::NetworkConfig {
        wireguard_conf: Some(configured),
        enforce_vpn: true,
        ..Default::default()
    };

    assert_eq!(
        egress_intent_line(&network),
        "Egress: WireGuard VPN — exit 1.1.1.1:51820"
    );
}

#[test]
fn egress_posture_describes_containment_only_and_tunnel_modes() {
    let containment = egress_posture(&capscode_core::NetworkConfig::default());
    assert_eq!(containment.mode, "containment-only");
    assert_eq!(
            containment.line(),
            "Egress posture: containment-only — public egress permitted by design (no data-exfiltration filtering); host/LAN/metadata blocked. Import a VPN for origin privacy: capscode vpn import <provider>.conf"
        );

    let tunnel = egress_posture(&capscode_core::NetworkConfig {
        wireguard_conf: Some(PathBuf::from("provider.conf")),
        enforce_vpn: true,
        ..Default::default()
    });
    assert_eq!(tunnel.mode, "tunnel");
    assert_eq!(
            tunnel.line(),
            "Egress posture: tunnel — all egress forced through WireGuard; DNS pinned to the tunnel resolver; host/LAN/metadata blocked. No destination filtering beyond this (by design)."
        );
}

#[test]
fn experimental_gate_prompts_only_for_unapproved_interactive_runs() {
    assert_eq!(
        experimental_gate(true, false, true),
        ExperimentalGate::Prompt
    );
    assert_eq!(
        experimental_gate(true, false, false),
        ExperimentalGate::Refuse
    );
    assert_eq!(
        experimental_gate(true, true, false),
        ExperimentalGate::Allow
    );
    assert_eq!(
        experimental_gate(false, false, false),
        ExperimentalGate::Allow
    );
}

#[test]
fn inline_store_offer_requires_non_anthropic_tty_and_healthy_keystore() {
    for provider_is_anthropic in [false, true] {
        for stdin_is_terminal in [false, true] {
            for keystore_problem_present in [false, true] {
                let expected =
                    !provider_is_anthropic && stdin_is_terminal && !keystore_problem_present;
                assert_eq!(
                        should_offer_inline_store(
                            provider_is_anthropic,
                            stdin_is_terminal,
                            keystore_problem_present,
                        ),
                        expected,
                        "anthropic={provider_is_anthropic}, tty={stdin_is_terminal}, problem={keystore_problem_present}"
                    );
            }
        }
    }
}

#[test]
fn status_json_has_expected_sections_without_credential_values() {
    let payload = status_json_payload(
        &DashboardRuntime {
            engine: Some("docker".to_string()),
            error: None,
        },
        &DashboardCredential {
            source: "env:ANTHROPIC_API_KEY".to_string(),
            kind: Some("API key".to_string()),
        },
        &DashboardCredential {
            source: "codex-auth:chatgpt".to_string(),
            kind: Some("ChatGPT access token".to_string()),
        },
        &ProviderDefaults::default(),
        &config::ProviderKeystoreStatus::Loaded(vec!["openai".to_string()]),
        &VpnStatus::NotConfigured,
        egress_posture(&capscode_core::NetworkConfig::default()),
    );
    let object = payload.as_object().expect("dashboard object");
    for key in [
        "runtime",
        "anthropic",
        "codex",
        "provider_defaults",
        "provider_keys",
        "vpn",
        "egress_posture",
        "egress_posture_detail",
    ] {
        assert!(object.contains_key(key), "missing dashboard key {key}");
    }
    assert_eq!(object["egress_posture"], "containment-only");
    assert_eq!(
            object["egress_posture_detail"],
            "public egress permitted by design (no data-exfiltration filtering); host/LAN/metadata blocked. Import a VPN for origin privacy: capscode vpn import <provider>.conf"
        );
    let serialized = serde_json::to_string(&payload).unwrap();
    assert!(!serialized.contains("sk-fake-dashboard-secret"));
}

#[test]
fn check_status_maps_bool_to_pass_fail() {
    assert_eq!(check_status(true), (Status::Ok, "PASS"));
    assert_eq!(check_status(false), (Status::Fail, "FAIL"));
}

#[test]
fn status_kv_plain_uses_ascii_glyph() {
    assert_eq!(
        status_kv("runtime", "docker", Status::Ok, Caps::PLAIN),
        "[ok] runtime       docker"
    );
    assert_eq!(
        status_kv(
            "credential",
            "sk-live-secret-value",
            Status::Fail,
            Caps::PLAIN
        ),
        "[x] credential    [REDACTED]"
    );
}

#[test]
fn format_vpn_doctor_covers_all_states() {
    let (st, value, fix) = format_vpn_doctor(&VpnStatus::NotConfigured);
    assert_eq!(st, Status::Dim);
    assert!(value.contains("not configured"));
    assert_eq!(fix, Some("capscode vpn import <provider>.conf"));

    let (st, value, fix) = format_vpn_doctor(&VpnStatus::Ready {
        path: PathBuf::from("/tmp/wg.conf"),
        endpoint: "1.2.3.4:51820".to_string(),
    });
    assert_eq!(st, Status::Ok);
    assert_eq!(value, "ready (endpoint 1.2.3.4:51820)");
    assert!(fix.is_none());

    let (st, value, fix) = format_vpn_doctor(&VpnStatus::Misconfigured {
        path: PathBuf::from("/tmp/wg.conf"),
        reason: "bad key".to_string(),
    });
    assert_eq!(st, Status::Fail);
    assert!(value.contains("misconfigured"));
    assert!(value.contains("bad key"));
    assert!(fix.is_none());
}

#[test]
fn format_dashboard_credential_plain() {
    let configured = DashboardCredential {
        source: "env:ANTHROPIC_API_KEY".to_string(),
        kind: Some("API key".to_string()),
    };
    assert_eq!(
        format_dashboard_credential(&configured),
        (Status::Ok, "env:ANTHROPIC_API_KEY (API key)".to_string())
    );
    let none = DashboardCredential {
        source: "none".to_string(),
        kind: None,
    };
    assert_eq!(
        format_dashboard_credential(&none),
        (Status::Dim, "none".to_string())
    );
}

#[test]
fn format_provider_keys_status_loaded_and_empty() {
    let loaded =
        config::ProviderKeystoreStatus::Loaded(vec!["openai".to_string(), "grok".to_string()]);
    let (st, value) = format_provider_keys_status(&loaded);
    assert_eq!(st, Status::Ok);
    assert!(value.contains("openai"));
    assert!(value.contains("grok"));

    let empty_loaded = config::ProviderKeystoreStatus::Loaded(vec![]);
    assert_eq!(
        format_provider_keys_status(&empty_loaded),
        (Status::Dim, "no stored provider key ids".to_string())
    );

    assert_eq!(
        format_provider_keys_status(&config::ProviderKeystoreStatus::Empty),
        (Status::Dim, "no keychain bundle".to_string())
    );
}

#[test]
fn format_tool_provider_kv_lines_plain() {
    let statuses = vec![
        ToolProviderStatus {
            tool: "claude",
            provider: Some("anthropic".to_string()),
            configured: true,
            error: None,
        },
        ToolProviderStatus {
            tool: "codex",
            provider: Some("openai".to_string()),
            configured: false,
            error: None,
        },
        ToolProviderStatus {
            tool: "pi",
            provider: None,
            configured: false,
            error: Some("no default".to_string()),
        },
    ];
    let lines = format_tool_provider_kv_lines(&statuses, Caps::PLAIN);
    assert!(lines
        .iter()
        .any(|l| l.contains("claude") && l.contains("[ok]")));
    assert!(lines
        .iter()
        .any(|l| l.contains("codex") && l.contains("[x]") && l.contains("credential missing")));
    assert!(lines.iter().any(|l| l.contains("capscode key set openai")));
    assert!(lines
        .iter()
        .any(|l| l.contains("pi") && l.contains("unset") && l.contains("no default")));
}

#[test]
fn format_status_lines_has_three_sections_plain() {
    let runtime = DashboardRuntime {
        engine: Some("docker".to_string()),
        error: None,
    };
    let anthropic = DashboardCredential {
        source: "env:ANTHROPIC_API_KEY".to_string(),
        kind: Some("API key".to_string()),
    };
    let codex = DashboardCredential {
        source: "none".to_string(),
        kind: None,
    };
    let defaults = ProviderDefaults::default();
    let keys = config::ProviderKeystoreStatus::Loaded(vec!["openai".to_string()]);
    let vpn = VpnStatus::NotConfigured;
    let posture = egress_posture(&capscode_core::NetworkConfig::default());
    let lines = format_status_lines(
        &StatusDashboardView {
            runtime: &runtime,
            anthropic: &anthropic,
            codex: &codex,
            defaults: &defaults,
            provider_keys: &keys,
            vpn: &vpn,
            posture,
        },
        Caps::PLAIN,
    );
    let joined = lines.join("\n");
    assert!(joined.contains("-- Runtime "));
    assert!(joined.contains("-- Providers & Credentials "));
    assert!(joined.contains("-- Network "));
    assert!(joined.contains("[ok] runtime"));
    assert!(joined.contains("docker"));
    assert!(joined.contains("anthropic"));
    assert!(joined.contains("containment-only"));
    assert!(!joined.contains("sk-"));
}

#[test]
fn format_vpn_status_dashboard_enforced_flag() {
    let ready = VpnStatus::Ready {
        path: PathBuf::from("/tmp/wg.conf"),
        endpoint: "1.1.1.1:51820".to_string(),
    };
    let (st, value) = format_vpn_status_dashboard(&ready, true);
    assert_eq!(st, Status::Ok);
    assert!(value.contains("validated: yes"));
    assert!(value.contains("enforced: yes"));

    let (st, value) = format_vpn_status_dashboard(&VpnStatus::NotConfigured, false);
    assert_eq!(st, Status::Dim);
    assert_eq!(value, "path: none; validated: no; enforced: no");
}

#[test]
fn format_egress_value_variants() {
    let mut report = capscode_core::NetVerifyReport {
        egress_via_wireguard_only: true,
        resolver_in_tunnel: true,
        rfc1918_blocked: true,
        host_gateway_blocked: true,
        ipv6_contained: capscode_core::ProbeOutcome::Dropped,
        host_gateway: vec![],
        public_probe: None,
        egress_dev: "wg0".to_string(),
        egress_ip: Some("1.2.3.4".to_string()),
        egress_loc: Some("AMS".to_string()),
        details: vec![],
    };
    assert_eq!(format_egress_value(&report), "wg0 — 1.2.3.4 (AMS)");
    report.egress_loc = None;
    assert_eq!(format_egress_value(&report), "wg0 — 1.2.3.4");
    report.egress_ip = None;
    assert_eq!(format_egress_value(&report), "wg0 — public IP unavailable");
}

#[test]
fn dashboard_vpn_validation_does_not_resolve_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("vpn.conf");
    fs::write(
            &path,
            "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nDNS = 10.99.0.1\n[Peer]\nPublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nEndpoint = definitely-not-resolvable.invalid:51820\nAllowedIPs = 0.0.0.0/0\n",
        )
        .unwrap();

    assert!(matches!(
        vpn_status_for_without_network(Some(&path)),
        VpnStatus::Ready { .. }
    ));
}

#[test]
fn docker_cli_endpoint_uses_the_selected_runtime_socket() {
    assert_eq!(
        docker_cli_endpoint(
            Some("unix:///run/orbstack.sock".into()),
            Some("unix:///var/run/docker.sock".into()),
        )
        .unwrap(),
        "unix:///run/orbstack.sock"
    );
    assert_eq!(
        docker_cli_endpoint(None, Some("unix:///var/run/docker.sock".into())).unwrap(),
        "unix:///var/run/docker.sock"
    );
    assert!(docker_cli_endpoint(None, None).is_err());
}

struct EndpointRuntime {
    socket_path: String,
}

#[async_trait::async_trait]
impl capscode_core::RuntimeAdapter for EndpointRuntime {
    fn kind(&self) -> capscode_core::RuntimeKind {
        capscode_core::RuntimeKind::Docker
    }

    async fn preflight(
        &self,
    ) -> std::result::Result<capscode_core::RuntimeInfo, capscode_core::RuntimeError> {
        Ok(capscode_core::RuntimeInfo {
            kind: capscode_core::RuntimeKind::Docker,
            version: "test".to_string(),
            socket_path: Some(self.socket_path.clone()),
            capabilities: self.capabilities(),
        })
    }

    fn capabilities(&self) -> capscode_core::RuntimeCapabilities {
        capscode_core::RuntimeCapabilities {
            localhost_publish: capscode_core::PublishMode::Native,
            shared_netns: true,
            tmpfs_mounts: true,
            cap_add_net_admin: true,
            dev_net_tun: true,
        }
    }

    async fn create_volume(
        &self,
        _: &capscode_core::VolumeSpec,
    ) -> std::result::Result<capscode_core::VolumeId, capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn remove_volume(
        &self,
        _: &capscode_core::VolumeId,
    ) -> std::result::Result<(), capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn image_present(
        &self,
        _: &capscode_core::ImageRef,
    ) -> std::result::Result<bool, capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn pull_image(
        &self,
        _: &capscode_core::ImageRef,
        _: &mut (dyn FnMut(capscode_core::PullProgress) + Send),
    ) -> std::result::Result<(), capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn create_container(
        &self,
        _: &capscode_core::ContainerSpec,
    ) -> std::result::Result<capscode_core::ContainerId, capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn start(
        &self,
        _: &capscode_core::ContainerId,
    ) -> std::result::Result<(), capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn exec(
        &self,
        _: &capscode_core::ContainerId,
        _: capscode_core::ExecSpec,
    ) -> std::result::Result<capscode_core::ExecOutput, capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn run_interactive(
        &self,
        _: &capscode_core::ContainerId,
        _: capscode_core::ExecSpec,
        _: capscode_core::AttachIo,
    ) -> std::result::Result<i32, capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn stop(
        &self,
        _: &capscode_core::ContainerId,
        _: std::time::Duration,
    ) -> std::result::Result<(), capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn remove(
        &self,
        _: &capscode_core::ContainerId,
        _: bool,
    ) -> std::result::Result<(), capscode_core::RuntimeError> {
        unreachable!()
    }

    async fn list_owned(
        &self,
    ) -> std::result::Result<Vec<capscode_core::OwnedObject>, capscode_core::RuntimeError> {
        unreachable!()
    }
}

struct RestoreDockerCliEnv {
    docker_host: Option<OsString>,
    capscode_docker_host: Option<OsString>,
}

impl RestoreDockerCliEnv {
    fn capture() -> Self {
        Self {
            docker_host: std::env::var_os("DOCKER_HOST"),
            capscode_docker_host: std::env::var_os("CAPSCODE_DOCKER_HOST"),
        }
    }
}

impl Drop for RestoreDockerCliEnv {
    fn drop(&mut self) {
        unsafe {
            match self.docker_host.take() {
                Some(value) => std::env::set_var("DOCKER_HOST", value),
                None => std::env::remove_var("DOCKER_HOST"),
            }
            match self.capscode_docker_host.take() {
                Some(value) => std::env::set_var("CAPSCODE_DOCKER_HOST", value),
                None => std::env::remove_var("CAPSCODE_DOCKER_HOST"),
            }
        }
    }
}

#[tokio::test]
async fn selected_runtime_preparation_sets_both_docker_cli_endpoints() {
    let _env_lock = DOCKER_CLI_ENV_LOCK.lock().await;
    let _restore = RestoreDockerCliEnv::capture();
    // DECISION: seed conflicting values so a missing sync cannot pass due to the host environment.
    unsafe {
        std::env::set_var("CAPSCODE_DOCKER_HOST", "unix:///run/old-capscode.sock");
        std::env::set_var("DOCKER_HOST", "unix:///run/old-docker.sock");
    }
    let runtime: Arc<dyn capscode_core::RuntimeAdapter> = Arc::new(EndpointRuntime {
        socket_path: "unix:///run/selected-runtime.sock".to_string(),
    });

    let (_, endpoint_error) = prepare_selected_runtime(runtime).await;

    assert!(endpoint_error.is_none());
    assert_eq!(
        std::env::var("CAPSCODE_DOCKER_HOST").unwrap(),
        "unix:///run/selected-runtime.sock"
    );
    assert_eq!(
        std::env::var("DOCKER_HOST").unwrap(),
        "unix:///run/selected-runtime.sock"
    );
}

#[cfg(not(windows))]
#[test]
fn docker_cli_endpoint_adds_unix_scheme_to_socket_paths() {
    assert_eq!(
        docker_cli_endpoint(Some("/run/orbstack.sock".into()), None).unwrap(),
        "unix:///run/orbstack.sock"
    );
}

fn exit_code_of(checks: Vec<DoctorCheckResult>) -> i32 {
    doctor_status_result(&checks).unwrap()
}

fn provider_status(configured: bool) -> ToolProviderStatus {
    ToolProviderStatus {
        tool: "codex",
        provider: configured.then(|| "openai".to_string()),
        configured,
        error: None,
    }
}

#[test]
fn quick_doctor_fails_only_when_the_runtime_is_unavailable() {
    let down = DashboardRuntime {
        engine: None,
        error: Some("cannot reach docker".to_string()),
    };
    let up = DashboardRuntime {
        engine: Some("orbstack".to_string()),
        error: None,
    };
    assert_eq!(
        exit_code_of(quick_doctor_checks(&down, true, true, true)),
        1
    );
    assert_eq!(
        exit_code_of(quick_doctor_checks(&up, false, false, false)),
        0,
        "missing provider keys, credentials, and VPN must not fail quick doctor"
    );
}

#[test]
fn full_doctor_fails_when_the_gateway_or_containment_fails() {
    let providers = [provider_status(true)];
    assert_eq!(
        exit_code_of(full_doctor_checks(None, Some(true), &providers, true, true)),
        0
    );
    assert_eq!(
        exit_code_of(full_doctor_checks(
            Some("required image capscode/gateway:0.1.0 is not present"),
            Some(true),
            &providers,
            true,
            true,
        )),
        1,
        "a missing or failed gateway must fail doctor"
    );
    assert_eq!(
        exit_code_of(full_doctor_checks(
            None,
            Some(false),
            &providers,
            true,
            true
        )),
        1,
        "failed containment checks must fail doctor"
    );
    assert_eq!(
        exit_code_of(full_doctor_checks(None, None, &providers, true, true)),
        1,
        "a gateway run without a containment report must fail doctor"
    );
    assert_eq!(
        exit_code_of(runtime_failure_doctor_checks("runtime unavailable")),
        1
    );
}

#[test]
fn full_doctor_ignores_missing_optional_provider_credentials_and_vpn() {
    assert_eq!(
        exit_code_of(full_doctor_checks(
            None,
            Some(true),
            &[provider_status(false)],
            false,
            false,
        )),
        0
    );
}

#[test]
fn doctor_json_keeps_gateway_execution_separate_from_containment() {
    let gateway = doctor_gateway_json(true, None, Some(false));
    assert_eq!(gateway["ok"], true);
    assert_eq!(gateway["error"], serde_json::Value::Null);
    assert_eq!(gateway["containment_ok"], false);
}

#[test]
fn uninstall_refuses_unlisted_boxes_when_the_runtime_cannot_be_listed() {
    let error = uninstall_unlisted_boxes_guard(false, false, Ok(2)).unwrap_err();
    assert!(format!("{error:#}").contains("2 box(es) in boxes.json"));
    assert!(format!("{error:#}").contains("--discard-workspace"));
    assert!(uninstall_unlisted_boxes_guard(false, false, Err(anyhow::anyhow!("corrupt"))).is_err());
    uninstall_unlisted_boxes_guard(false, false, Ok(0)).unwrap();
    uninstall_unlisted_boxes_guard(false, true, Ok(2)).unwrap();
    uninstall_unlisted_boxes_guard(true, false, Ok(2)).unwrap();
}

#[test]
fn toolchain_pin_requires_untested_acknowledgement() {
    let error = require_untested_pin(false).unwrap_err();
    assert!(format!("{error:#}").contains("--untested"));
    require_untested_pin(true).unwrap();
}

#[test]
fn toolchain_pin_refreshes_dependency_cutoff_and_commits_only_after_build_success() {
    let mut lock = crate::toolchain::builtin_lock().unwrap();
    let timestamp = current_utc_timestamp().unwrap();
    assert!(crate::toolchain::is_utc_timestamp(&timestamp));
    let tool = lock.tools.get_mut("codex").unwrap();
    let integrity = tool.integrity.clone();
    apply_toolchain_pin(tool, "0.159.4", &integrity, timestamp.clone());
    assert_eq!(tool.resolved_before, timestamp);
    assert_eq!(tool.smoke, crate::toolchain::SmokeGrade::None);

    let root = tempfile::tempdir().unwrap();
    let runtime_lock = root.path().join("toolchain.lock.toml");
    let old = crate::toolchain::builtin_lock().unwrap();
    crate::toolchain::write_runtime_lock_at(&runtime_lock, &old).unwrap();
    assert!(activate_runtime_lock_after_build(
        Err(anyhow::anyhow!("image build failed")),
        &lock,
        &runtime_lock,
    )
    .is_err());
    assert_eq!(
        crate::toolchain::read_validated(&runtime_lock)
            .unwrap()
            .tools["codex"]
            .version,
        old.tools["codex"].version
    );

    activate_runtime_lock_after_build(Ok(()), &lock, &runtime_lock).unwrap();
    assert_eq!(
        crate::toolchain::read_validated(&runtime_lock)
            .unwrap()
            .tools["codex"]
            .version,
        "0.159.4"
    );
}

#[test]
fn image_pruning_uses_ownership_role_and_contract_labels() {
    let image = |reference: &str, role: Option<&str>, contract: Option<&str>| {
        let mut labels = std::collections::BTreeMap::from([(
            LABEL_OWNER.to_owned(),
            LABEL_OWNER_VALUE.to_owned(),
        )]);
        if let Some(role) = role {
            labels.insert(capscode_core::LABEL_ROLE.to_owned(), role.to_owned());
        }
        if let Some(contract) = contract {
            labels.insert(LABEL_CONTRACT.to_owned(), contract.to_owned());
        }
        LocalImageInfo {
            reference: reference.to_owned(),
            size_bytes: None,
            created_unix: None,
            labels,
        }
    };

    let owned_capsule = image(
        "capscode/capsule-all:abcdef123456",
        Some(ROLE_CAPSULE),
        Some("2"),
    );
    let old_capsule = image("capscode/capsule-codex:0.1.0", Some(ROLE_CAPSULE), None);
    let gateway = image("capscode/gateway:0.1.0", Some(ROLE_GATEWAY), Some("2"));
    let unlabelled = LocalImageInfo {
        reference: "capscode/capsule-experiment:dev".into(),
        size_bytes: None,
        created_unix: None,
        labels: Default::default(),
    };

    assert!(image_created_by_capscode(&owned_capsule));
    assert!(is_capsule_toolchain_image(&owned_capsule));
    assert!(image_contract_is_current(&owned_capsule));
    assert!(!image_contract_is_current(&old_capsule));
    assert!(!is_capsule_toolchain_image(&gateway));
    assert!(!image_created_by_capscode(&unlabelled));
    assert!(!is_capsule_toolchain_image(&unlabelled));
}

#[test]
fn ls_json_keeps_existing_box_fields_and_adds_toolchain_fields() {
    let row = crate::boxes::BoxRow {
        name: "alpha-123abc".into(),
        session: Some("session-id".into()),
        state: "stopped".into(),
        project: std::path::PathBuf::from("/tmp/alpha"),
        mode: "copy".into(),
        providers: vec!["openai".into()],
        last_entered: "2026-10-18T00:00:00Z".into(),
        toolchain: "fb24882402b5".into(),
        toolchain_current: true,
        toolchain_pinned: false,
        toolchain_tested: true,
        toolchain_yanked: false,
        cache_volume: Some("capscode-session-id-cache".into()),
    };
    let value = box_row_json(&row);
    for field in [
        "kind",
        "name",
        "session",
        "state",
        "project",
        "mode",
        "providers",
        "last_entered",
        "toolchain",
        "toolchain_current",
        "toolchain_pinned",
        "toolchain_tested",
        "toolchain_yanked",
        "cache_volume",
    ] {
        assert!(value.get(field).is_some(), "missing JSON field {field}");
    }
    assert_eq!(value["toolchain"], "fb24882402b5");
    assert_eq!(value["toolchain_current"], true);
    assert_eq!(value["cache_volume"], "capscode-session-id-cache");
}

#[test]
fn doctor_orphan_cache_guidance_prints_a_manual_docker_remove_command() {
    assert_eq!(
        orphaned_cache_volume_line("capscode-session-cache"),
        "orphaned cache volume capscode-session-cache; remove with `docker volume rm capscode-session-cache`"
    );
}

#[test]
fn prune_keeps_current_running_stopped_pinned_and_newest_snapshots() {
    let box_object = |name: &str, state: &str, digest: &str| OwnedObject {
        kind: OwnedObjectKind::Container,
        id: name.into(),
        session: None,
        role: Some(ROLE_CAPSULE.into()),
        created_at: None,
        state: Some(state.into()),
        image: Some(format!("capscode/capsule-all:{digest}")),
        labels: std::collections::BTreeMap::from([
            (LABEL_BOX.into(), name.into()),
            (capscode_core::LABEL_TOOLCHAIN.into(), digest.into()),
        ]),
    };
    let snapshots = [
        ("111111111111", 5), // current
        ("222222222222", 4), // box running
        ("333333333333", 3), // box stopped
        ("444444444444", 2), // newest other
        ("555555555555", 1), // removable
        ("666666666666", 0), // pinned by a box whose container is gone
    ]
    .map(|(digest, seconds)| crate::toolchain::Snapshot {
        digest: digest.into(),
        path: std::path::PathBuf::new(),
        lock: crate::toolchain::ToolchainLock::default(),
        modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
    });
    let objects = [
        box_object("running-box", "running", "222222222222"),
        box_object("stopped-box", "exited", "333333333333"),
    ];
    let pinned_box = crate::boxes::index::Entry {
        toolchain: "666666666666".into(),
        ..Default::default()
    };
    let referenced = referenced_toolchains(&[pinned_box], &objects);
    let protected =
        crate::toolchain::protected_snapshots(&snapshots, "111111111111", &referenced, 1);
    let removable: Vec<_> = snapshots
        .iter()
        .filter(|snapshot| !protected.contains(&snapshot.digest))
        .map(|snapshot| snapshot.digest.as_str())
        .collect();
    assert_eq!(removable, ["555555555555"]);
    assert!(protected.contains("666666666666"));
}

#[test]
fn ls_marks_a_box_on_a_withdrawn_toolchain_as_yanked() {
    let mut row = crate::boxes::BoxRow {
        name: "alpha-123abc".into(),
        session: Some("session-id".into()),
        state: "stopped".into(),
        project: std::path::PathBuf::from("/tmp/alpha"),
        mode: "copy".into(),
        providers: Vec::new(),
        last_entered: String::new(),
        toolchain: "fb24882402b5".into(),
        toolchain_current: true,
        toolchain_pinned: true,
        toolchain_tested: true,
        toolchain_yanked: true,
        cache_volume: None,
    };
    assert_eq!(toolchain_row_label(&row), "fb24882 (yanked)");
    assert_eq!(
        box_row_json(&row)["toolchain_yanked"],
        serde_json::json!(true)
    );
    row.toolchain_yanked = false;
    assert_eq!(toolchain_row_label(&row), "fb24882 (pinned)");
}
