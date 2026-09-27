//! Sanitized git ingress/egress engine implementing `capscode_core::GitEngine`.
//! Git workspace ingress and egress operations.

use std::collections::{BTreeMap, HashSet};
#[cfg(unix)]
use std::ffi::CString;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use capscode_core::{
    ContainerId, ContainerSpec, EgressPlan, EgressReport, ExecSpec, GitEngine, GitError,
    HistoryDial, IngressPlan, IngressProgress, IngressReceipt, IngressReport, Mount, NetworkMode,
    PhaseTiming, RuntimeAdapter, RuntimeKind, VolumeId, VolumeSpec, LABEL_CREATED, LABEL_OWNER,
    LABEL_OWNER_VALUE, LABEL_ROLE, LABEL_SESSION, ROLE_CAPSULE,
};
use git2::{Oid, Repository, ResetType, Status, StatusOptions, Tree};
use tokio::process::Command as TokioCommand;
use tokio::task;

/// Concrete GitEngine.
pub struct DefaultGitEngine;

const SHADOW_MAX_FILES: u64 = 100_000;
const SHADOW_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const SHADOW_FREE_SPACE_MARGIN: u64 = 64 * 1024 * 1024;
const RECEIPT_FILE: &str = "receipt.json";
const MANIFEST_FILE: &str = "manifest.json";
const DIFF_OUTPUT_LIMIT: usize = 256 * 1024;
const PRIVATE_SHADOW_ROOTS: &[&str] = &[".capscode-autosync.git"];
const RUNNER_DIRECTORIES: &[&str] = &[".husky", ".githooks", ".lefthook", ".git-hooks"];
const RUNNER_CONFIG_FILES: &[&str] = &[
    ".envrc",
    ".pre-commit-config.yaml",
    ".pre-commit-config.yml",
    ".lefthook.yml",
    ".lefthook.yaml",
    ".lefthook.toml",
    ".lefthook.json",
    ".lefthook.jsonc",
    "lefthook.yml",
    "lefthook.yaml",
    "lefthook.toml",
    "lefthook.json",
    "lefthook.jsonc",
    ".lefthook-local.yml",
    ".lefthook-local.yaml",
    ".lefthook-local.toml",
    ".lefthook-local.json",
    ".lefthook-local.jsonc",
    "lefthook-local.yml",
    "lefthook-local.yaml",
    "lefthook-local.toml",
    "lefthook-local.json",
    "lefthook-local.jsonc",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ShadowChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowFileChange {
    pub kind: ShadowChangeKind,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowIssue {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug)]
struct ShadowApplyCandidate {
    path: PathBuf,
    kind: ShadowChangeKind,
    capsule_executable: bool,
    capsule_blob: Oid,
    baseline_blob: Option<Oid>,
    baseline_executable: Option<bool>,
}

#[derive(Debug)]
pub struct ShadowHandoff {
    pub state_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub backup_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub changes: Vec<ShadowFileChange>,
    pub conflicts: Vec<ShadowIssue>,
    pub unsupported: Vec<ShadowIssue>,
    pub diff: String,
    pub diff_truncated: bool,
    git_dir: PathBuf,
    candidates: Vec<ShadowApplyCandidate>,
}

impl ShadowHandoff {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.unsupported.is_empty()
    }

    pub fn clean_count(&self) -> usize {
        self.candidates.len()
    }

    pub fn deletion_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|change| change.kind == ShadowChangeKind::Deleted)
            .count()
    }

    pub fn staged_change_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|change| change.kind != ShadowChangeKind::Deleted)
            .count()
    }
}

#[derive(Debug)]
pub struct ShadowApplyReport {
    pub applied: Vec<PathBuf>,
    /// Paths refused outright: nothing under `.git/` is ever imported.
    pub refused: Vec<PathBuf>,
    /// Paths the host would execute from repo content, skipped because the run
    /// did not explicitly allow them. Reported by name so the user sees what did
    /// not come back.
    pub skipped_implicit_runners: Vec<PathBuf>,
    /// Paths whose apply-time revalidation failed. Each one stays in the staging
    /// directory; a single failure does not prevent processing the rest of the round.
    pub failed: Vec<ShadowIssue>,
}

/// What class of thing a changed path is, from the HOST's point of view.
///
/// The existing symlink / `..` / type-change refusals protect PATHS. This
/// protects CONTENT CLASS: an agent that ran unattended for days can author a
/// file the host executes the next time the user runs an ordinary command
/// there, which is host code execution by way of a "file copy".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportClass {
    /// Ordinary content. Imported normally.
    Plain,
    /// The host executes this from repo content (git hooks, `.husky/`, direnv).
    /// Requires an explicit decision; never imported silently.
    ImplicitRunner,
    /// Never imported, no prompt, no override: everything under `.git/`, plus a
    /// `.git` gitdir pointer file. History is reconstructed host-side, so agent
    /// authored git metadata has no legitimate route in.
    Refused,
}

/// Classify a workspace-relative path. Pure so it is testable without a
/// filesystem, and deliberately CAUTIOUS: anything unrecognised that looks like
/// tooling configuration is treated as an implicit runner rather than as plain
/// content.
pub fn classify_import_path(relative: &Path) -> ImportClass {
    let components = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part),
            _ => None,
        });
    let normal_components = components.collect::<Vec<_>>();

    // Git metadata is refused at any depth, including `.git` pointer files.
    if normal_components.iter().any(|part| {
        part.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
    }) {
        return ImportClass::Refused;
    }

    // The keeper's private checkpoint repo for non-git workspaces (see
    // images/keeper.sh). It exists only inside the capsule's volume as crash
    // insurance and is meaningless — and confusing — on the host.
    if is_private_shadow_path(relative) {
        return ImportClass::Refused;
    }

    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    // Directories whose contents the host runs.
    if normal_components.iter().any(|part| {
        part.to_str().is_some_and(|name| {
            RUNNER_DIRECTORIES
                .iter()
                .any(|runner_dir| name.eq_ignore_ascii_case(runner_dir))
        })
    }) {
        return ImportClass::ImplicitRunner;
    }

    if RUNNER_CONFIG_FILES
        .iter()
        .any(|runner_file| name.eq_ignore_ascii_case(runner_file))
    {
        ImportClass::ImplicitRunner
    } else {
        ImportClass::Plain
    }
}

fn is_private_shadow_path(relative: &Path) -> bool {
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|first| {
            PRIVATE_SHADOW_ROOTS
                .iter()
                .any(|private_root| first.eq_ignore_ascii_case(private_root))
        })
}

fn remove_private_shadow_roots(root: &Path) -> Result<(), GitError> {
    let entries = std::fs::read_dir(root)
        .map_err(|_| shadow_egress_error("could not inspect the file sync export root"))?;
    for entry in entries {
        let entry = entry
            .map_err(|_| shadow_egress_error("could not inspect the file sync export root"))?;
        let is_private_root = entry.file_name().to_str().is_some_and(|name| {
            PRIVATE_SHADOW_ROOTS
                .iter()
                .any(|private| name.eq_ignore_ascii_case(private))
        });
        if !is_private_root {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
            shadow_egress_error("could not inspect the private checkpoint repository")
        })?;
        let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        result.map_err(|_| {
            shadow_egress_error("could not exclude the private checkpoint repository")
        })?;
    }
    Ok(())
}

fn classify_git_import_path(relative: &Path, hooks_path: Option<&Path>) -> ImportClass {
    let class = classify_import_path(relative);
    if class != ImportClass::Plain {
        return class;
    }
    if hooks_path.is_some_and(|hooks_path| path_starts_with_case_insensitive(relative, hooks_path))
    {
        ImportClass::ImplicitRunner
    } else {
        ImportClass::Plain
    }
}

fn path_starts_with_case_insensitive(path: &Path, prefix: &Path) -> bool {
    let path_components = path.components().collect::<Vec<_>>();
    let prefix_components = prefix.components().collect::<Vec<_>>();
    prefix_components.len() <= path_components.len()
        && prefix_components
            .iter()
            .zip(path_components.iter())
            .all(|(prefix, path)| {
                let prefix = prefix.as_os_str().to_string_lossy();
                let path = path.as_os_str().to_string_lossy();
                prefix
                    .chars()
                    .flat_map(char::to_lowercase)
                    .eq(path.chars().flat_map(char::to_lowercase))
            })
}

fn repo_hooks_path(repo: &Repository, host_repo: &Path) -> Result<Option<PathBuf>, GitError> {
    let config = repo.config()?;
    let configured = match config.get_path("core.hooksPath") {
        Ok(path) => path,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let workdir = repo.workdir().unwrap_or(host_repo);
    let relative = if configured.is_absolute() {
        let Ok(relative) = configured.strip_prefix(workdir) else {
            return Ok(None);
        };
        relative
    } else {
        configured.as_path()
    };

    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => normalized.push(name),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if normalized.pop() => {}
            std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => return Ok(None),
        }
    }
    Ok(Some(normalized))
}

fn capsule_changed_paths(
    repo: &Repository,
    baseline: Oid,
    head: Oid,
) -> Result<Vec<PathBuf>, GitError> {
    let baseline_tree = repo.find_commit(baseline)?.tree()?;
    let head_tree = repo.find_commit(head)?.tree()?;
    let mut diff = repo.diff_tree_to_tree(Some(&baseline_tree), Some(&head_tree), None)?;
    let mut find_options = git2::DiffFindOptions::new();
    find_options.renames(true);
    diff.find_similar(Some(&mut find_options))?;

    let mut paths = Vec::new();
    for delta in diff.deltas() {
        if let Some(path) = delta.old_file().path() {
            paths.push(path.to_path_buf());
        }
        if let Some(path) = delta.new_file().path() {
            paths.push(path.to_path_buf());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn create_runner_files_branch(
    host_repo: &Path,
    session: &capscode_core::SessionId,
    head: Oid,
) -> Result<String, GitError> {
    let session_token = session
        .as_str()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(8)
        .collect::<String>();
    let head_token = head.to_string();
    let base = format!("runner-files-held-{session_token}-{}", &head_token[..12]);
    for suffix in 0_u32.. {
        let name = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        match DefaultGitEngine::new().apply_as_branch(host_repo, session, &name) {
            Ok(branch) => return Ok(branch),
            Err(GitError::RefConflict(_)) => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("an unbounded range always yields a branch name")
}

impl Default for DefaultGitEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultGitEngine {
    pub fn new() -> Self {
        Self
    }
}

fn workspace_volume_name(session: &capscode_core::SessionId) -> String {
    format!("capscode-{}-workspace", session.as_str())
}

fn session_ref_prefix(session: &capscode_core::SessionId) -> String {
    format!("refs/capscode/{}", session.as_str())
}

fn egress_refspecs(session: &capscode_core::SessionId) -> Vec<String> {
    let prefix = session_ref_prefix(session);
    vec![
        format!("+HEAD:{prefix}/head"),
        format!("+refs/heads/*:{prefix}/heads/*"),
        format!("+refs/capscode/baseline:{prefix}/baseline"),
    ]
}

fn verify_fetched_baseline(plan: &EgressPlan) -> Result<(), GitError> {
    let Some(expected) = plan.baseline else {
        return Ok(());
    };
    let repo = Repository::open(&plan.host_repo)
        .map_err(|_| GitError::NotARepo(plan.host_repo.to_path_buf()))?;
    let name = format!("{}/baseline", session_ref_prefix(&plan.session));
    let actual = repo
        .find_reference(&name)
        .map_err(|_| GitError::Egress("capsule baseline was not fetched".into()))?
        .target()
        .ok_or_else(|| GitError::Egress("fetched capsule baseline ref is symbolic".into()))?;
    if actual != expected {
        return Err(GitError::Egress(
            "capsule baseline ref does not match the ingress snapshot".into(),
        ));
    }
    repo.find_commit(expected)
        .map_err(|_| GitError::Egress("fetched capsule baseline commit is missing".into()))?;
    Ok(())
}

fn ext_upload_pack_cmd(container: &ContainerId, ws: &Path, kind: RuntimeKind) -> String {
    let cli = match kind {
        RuntimeKind::Docker | RuntimeKind::OrbStack => "docker",
        RuntimeKind::AppleContainer => "container",
    };
    format!(
        "{} exec -i {} git-upload-pack {}",
        cli,
        container.0,
        ws.display()
    )
}

fn time_label() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn docker_host_override() -> Option<OsString> {
    std::env::var_os("CAPSCODE_DOCKER_HOST")
}

fn configure_docker_command(command: &mut Command) {
    configure_docker_host(command, docker_host_override());
}

fn configure_docker_host(command: &mut Command, host: Option<OsString>) {
    if let Some(host) = host {
        command.env("DOCKER_HOST", host);
    }
}

fn configure_tokio_docker_command(command: &mut TokioCommand) {
    if let Some(host) = docker_host_override() {
        command.env("DOCKER_HOST", host);
    }
}

/// Size and file count of a prepared tree. Both numbers are reported to the user
/// and drive the ingress cost model, so the walk must see every entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TreeStats {
    bytes: u64,
    files: u64,
}

fn tree_stats(path: &Path) -> TreeStats {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| std::fs::metadata(e.path()).ok())
        .fold(TreeStats::default(), |mut stats, meta| {
            stats.bytes += meta.len();
            stats.files += 1;
            stats
        })
}

/// Record a step's duration into `log`, evaluating `body` between the marks.
macro_rules! timed {
    ($log:expr, $phase:expr, $($body:tt)*) => {{
        let started = Instant::now();
        let value = { $($body)* };
        $log.record($phase, started.elapsed());
        value
    }};
}

/// Ordered per-step ingress timings. Observational only.
#[derive(Debug, Default)]
struct PhaseLog {
    phases: Vec<PhaseTiming>,
}

impl PhaseLog {
    fn record(&mut self, phase: &str, elapsed: Duration) {
        self.phases.push(PhaseTiming::new(phase, elapsed));
    }
}

mod walkdir {
    // Minimal walkdir replacement to avoid new dep in lib.
    use std::fs;
    use std::path::{Path, PathBuf};

    pub struct WalkDir {
        root: PathBuf,
    }

    impl WalkDir {
        pub fn new(p: &Path) -> Self {
            Self {
                root: p.to_path_buf(),
            }
        }
        pub fn into_iter(self) -> WalkIter {
            WalkIter {
                stack: vec![self.root.clone()],
                pending: Vec::new(),
            }
        }
    }

    pub struct DirEntry {
        path: PathBuf,
        ft: fs::FileType,
    }

    impl DirEntry {
        pub fn path(&self) -> &Path {
            &self.path
        }
        pub fn file_type(&self) -> fs::FileType {
            self.ft
        }
    }

    pub struct WalkIter {
        stack: Vec<PathBuf>,
        pending: Vec<DirEntry>,
    }

    impl Iterator for WalkIter {
        type Item = Result<DirEntry, std::io::Error>;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                // Drain what the last read_dir produced before touching the stack:
                // returning early from a fresh `read_dir` would drop the remaining
                // entries of that directory on the floor.
                if let Some(entry) = self.pending.pop() {
                    return Some(Ok(entry));
                }
                let dir = self.stack.pop()?;
                if let Ok(rd) = fs::read_dir(&dir) {
                    for ent in rd.flatten() {
                        let p = ent.path();
                        if let Ok(ft) = ent.file_type() {
                            if ft.is_dir() {
                                self.stack.push(p.clone());
                            }
                            self.pending.push(DirEntry { path: p, ft });
                        }
                    }
                }
            }
        }
    }
}

fn isolated_git_command() -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");

    const REMOVE: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
        "GIT_TEMPLATE_DIR",
        "GIT_ATTR_SOURCE",
    ];
    for name in REMOVE {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("GIT_AUTHOR_")
            || name.starts_with("GIT_COMMITTER_")
            || name.starts_with("GIT_CONFIG_KEY_")
            || name.starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(name.as_ref());
        }
    }
    command
}

fn empty_git_template_dir() -> Result<tempfile::TempDir, GitError> {
    tempfile::Builder::new()
        .prefix("capscode-git-template-")
        .tempdir()
        .map_err(|error| {
            GitError::Ingress(format!("creating empty Git template directory: {error}"))
        })
}

fn add_git_template_option(command: &mut Command, template_dir: &Path) {
    let mut option = OsString::from("--template=");
    option.push(template_dir.as_os_str());
    command.arg(option);
}

fn run_git_blocking(dir: &Path, args: &[&str], verbose: bool) -> Result<(), GitError> {
    let mut command = isolated_git_command();
    command.current_dir(dir).args(args);
    if verbose {
        let status = command
            .status()
            .map_err(|e| GitError::Ingress(format!("git spawn {}: {}", args.join(" "), e)))?;
        if !status.success() {
            return Err(GitError::Ingress(format!("git {} failed", args.join(" "))));
        }
    } else {
        let output = command
            .output()
            .map_err(|e| GitError::Ingress(format!("git spawn {}: {}", args.join(" "), e)))?;
        if !output.status.success() {
            return Err(GitError::Ingress(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(())
}

fn run_git_capture(dir: &Path, args: &[&str]) -> Result<Output, GitError> {
    isolated_git_command()
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|error| GitError::Ingress(format!("starting git {}: {error}", args.join(" "))))
}

fn run_git_add_path(dir: &Path, relative: &Path, verbose: bool) -> Result<(), GitError> {
    let mut command = isolated_git_command();
    command.current_dir(dir).args(["add", "-f", "--"]);
    if relative.as_os_str().is_empty() {
        command.arg(".");
    } else {
        command.arg(relative);
    }
    if verbose {
        let status = command.status().map_err(|error| {
            GitError::Ingress(format!(
                "starting git add for {}: {error}",
                relative.display()
            ))
        })?;
        if status.success() {
            return Ok(());
        }
        return Err(GitError::Ingress(format!(
            "git add failed for {}",
            relative.display()
        )));
    }
    let output = command.output().map_err(|error| {
        GitError::Ingress(format!(
            "starting git add for {}: {error}",
            relative.display()
        ))
    })?;
    if !output.status.success() {
        return Err(GitError::Ingress(format!(
            "git add failed for {}: {}",
            relative.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn has_staged_changes(dir: &Path) -> Result<bool, GitError> {
    let output = run_git_capture(dir, &["diff", "--cached", "--quiet"])?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(GitError::Ingress(format!(
            "checking staged ingress changes failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

fn remove_destination_entry(path: &Path) -> Result<(), GitError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            std::fs::remove_dir_all(path).map_err(|error| {
                GitError::Ingress(format!("removing destination {}: {error}", path.display()))
            })
        }
        Ok(_) => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(file_error) => std::fs::remove_dir(path).map_err(|dir_error| {
                GitError::Ingress(format!(
                    "removing destination {}: {file_error}; {dir_error}",
                    path.display()
                ))
            }),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(GitError::Ingress(format!(
            "inspecting destination {}: {error}",
            path.display()
        ))),
    }
}

fn canonical_entry_path(path: &Path) -> Option<PathBuf> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Some(parent.canonicalize().ok()?.join(name)),
        _ => path.canonicalize().ok(),
    }
}

fn is_same_filesystem_entry(source: &Path, destination: &Path) -> bool {
    if source == destination {
        return true;
    }

    #[cfg(unix)]
    if let (Ok(source_metadata), Ok(destination_metadata)) = (
        std::fs::symlink_metadata(source),
        std::fs::symlink_metadata(destination),
    ) {
        if source_metadata.dev() == destination_metadata.dev()
            && source_metadata.ino() == destination_metadata.ino()
        {
            return true;
        }
    }

    canonical_entry_path(source)
        .zip(canonical_entry_path(destination))
        .is_some_and(|(source, destination)| source == destination)
}

fn ensure_destination_directory(root: &Path, relative: &Path) -> Result<PathBuf, GitError> {
    let root_metadata = std::fs::symlink_metadata(root).map_err(|error| {
        GitError::Ingress(format!(
            "inspecting prepared workspace {}: {error}",
            root.display()
        ))
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(GitError::Ingress(format!(
            "prepared workspace {} is not a real directory",
            root.display()
        )));
    }

    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(GitError::Ingress(format!(
                "unsafe destination path {}",
                relative.display()
            )));
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                remove_destination_entry(&current)?;
                std::fs::create_dir(&current).map_err(|error| {
                    GitError::Ingress(format!(
                        "creating destination {}: {error}",
                        current.display()
                    ))
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|error| {
                    GitError::Ingress(format!(
                        "creating destination {}: {error}",
                        current.display()
                    ))
                })?;
            }
            Err(error) => {
                return Err(GitError::Ingress(format!(
                    "inspecting destination {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(current)
}

fn create_symlink(source: &Path, destination: &Path) -> Result<(), GitError> {
    let target = std::fs::read_link(source).map_err(|error| {
        GitError::Ingress(format!("reading symlink {}: {error}", source.display()))
    })?;
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, destination);
    #[cfg(windows)]
    let result = if std::fs::metadata(source)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    };
    #[cfg(not(any(unix, windows)))]
    let result: Result<(), std::io::Error> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symbolic links are not supported on this platform",
    ));
    result.map_err(|error| {
        GitError::Ingress(format!(
            "creating symlink {}: {error}",
            destination.display()
        ))
    })
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<(), GitError> {
    let mut input = open_nofollow(source).map_err(|error| {
        GitError::Ingress(format!("opening source file {}: {error}", source.display()))
    })?;
    let permissions = input
        .metadata()
        .map_err(|error| {
            GitError::Ingress(format!(
                "inspecting source file {}: {error}",
                source.display()
            ))
        })?
        .permissions();
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| {
            GitError::Ingress(format!(
                "creating destination file {}: {error}",
                destination.display()
            ))
        })?;
    std::io::copy(&mut input, &mut output).map_err(|error| {
        GitError::Ingress(format!(
            "copying {} to {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    output.set_permissions(permissions).map_err(|error| {
        GitError::Ingress(format!(
            "setting permissions on {}: {error}",
            destination.display()
        ))
    })
}

fn copy_dir_all(src: &Path, dst_root: &Path, relative: &Path) -> Result<(), GitError> {
    copy_entry(src, dst_root, relative)
}

fn copy_entry(src: &Path, dst_root: &Path, relative: &Path) -> Result<(), GitError> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(GitError::Ingress(format!(
            "unsafe source path {}",
            relative.display()
        )));
    }

    let dst = dst_root.join(relative);
    let metadata = std::fs::symlink_metadata(src).map_err(|error| {
        GitError::Ingress(format!("inspecting source {}: {error}", src.display()))
    })?;
    if is_same_filesystem_entry(src, &dst) {
        return Err(GitError::Ingress(format!(
            "refusing to copy {} onto itself",
            src.display()
        )));
    }

    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let parent = ensure_destination_directory(dst_root, parent_relative)?;
    let dst = if relative.as_os_str().is_empty() {
        dst_root.to_path_buf()
    } else {
        parent.join(relative.file_name().ok_or_else(|| {
            GitError::Ingress(format!("invalid destination path {}", relative.display()))
        })?)
    };
    if metadata.file_type().is_symlink() {
        if dst == dst_root {
            return Err(GitError::Ingress(
                "refusing to replace the prepared workspace root with a symlink".into(),
            ));
        }
        remove_destination_entry(&dst)?;
        return create_symlink(src, &dst);
    }

    if metadata.is_dir() {
        if relative.as_os_str().is_empty() {
            for entry in std::fs::read_dir(src).map_err(|error| {
                GitError::Ingress(format!(
                    "reading source directory {}: {error}",
                    src.display()
                ))
            })? {
                let entry = entry.map_err(|error| {
                    GitError::Ingress(format!(
                        "reading source directory {}: {error}",
                        src.display()
                    ))
                })?;
                if entry.file_name() == ".git" {
                    continue;
                }
                copy_entry(&entry.path(), dst_root, Path::new(&entry.file_name()))?;
            }
            return Ok(());
        }

        ensure_destination_directory(dst_root, relative)?;
        for entry in std::fs::read_dir(src).map_err(|error| {
            GitError::Ingress(format!(
                "reading source directory {}: {error}",
                src.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                GitError::Ingress(format!(
                    "reading source directory {}: {error}",
                    src.display()
                ))
            })?;
            if entry.file_name() == ".git" {
                continue;
            }
            copy_entry(&entry.path(), dst_root, &relative.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if !metadata.is_file() {
        return Err(GitError::Ingress(format!(
            "unsupported source entry {}",
            src.display()
        )));
    }
    if dst == dst_root {
        return Err(GitError::Ingress(
            "refusing to replace the prepared workspace root with a file".into(),
        ));
    }
    remove_destination_entry(&dst)?;
    copy_regular_file(src, &dst)
}

fn remove_prepared_entry(root: &Path, relative: &Path) -> Result<(), GitError> {
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let parent = ensure_destination_directory(root, parent_relative)?;
    let name = relative.file_name().ok_or_else(|| {
        GitError::Ingress(format!("invalid prepared path {}", relative.display()))
    })?;
    remove_destination_entry(&parent.join(name))
}

fn resolve_include(host_root: &Path, include: &Path) -> Result<(PathBuf, PathBuf), GitError> {
    let requested = if include.is_absolute() {
        include.to_path_buf()
    } else {
        host_root.join(include)
    };
    let resolved = requested.canonicalize().map_err(|error| {
        GitError::Ingress(format!(
            "cannot resolve include {}: {error}",
            include.display()
        ))
    })?;
    if !resolved.starts_with(host_root) {
        return Err(GitError::Ingress(format!(
            "include {} resolves outside the project directory",
            include.display()
        )));
    }

    let metadata = std::fs::symlink_metadata(&requested).map_err(|error| {
        GitError::Ingress(format!("inspecting include {}: {error}", include.display()))
    })?;
    let source = if metadata.file_type().is_symlink() {
        requested
            .parent()
            .ok_or_else(|| GitError::Ingress(format!("invalid include {}", include.display())))?
            .canonicalize()
            .map_err(|error| {
                GitError::Ingress(format!(
                    "resolving include parent {}: {error}",
                    include.display()
                ))
            })?
            .join(requested.file_name().ok_or_else(|| {
                GitError::Ingress(format!("invalid include {}", include.display()))
            })?)
    } else {
        resolved
    };
    let relative = source
        .strip_prefix(host_root)
        .map_err(|_| {
            GitError::Ingress(format!(
                "include {} resolves outside the project directory",
                include.display()
            ))
        })?
        .to_path_buf();
    if relative
        .components()
        .any(|component| component.as_os_str() == ".git")
    {
        return Err(GitError::Ingress(format!(
            "include {} names Git metadata, which is not copied",
            include.display()
        )));
    }
    Ok((source, relative))
}

fn copy_tree_objects(
    source: &Repository,
    destination: &Repository,
    tree: &Tree<'_>,
) -> Result<Oid, GitError> {
    let mut builder = destination.treebuilder(None)?;
    for entry in tree.iter() {
        let oid = match entry.filemode() {
            0o040000 => {
                let subtree = source.find_tree(entry.id())?;
                copy_tree_objects(source, destination, &subtree)?
            }
            0o160000 => entry.id(),
            _ => {
                let blob = source.find_blob(entry.id())?;
                destination.blob(blob.content())?
            }
        };
        builder.insert(entry.name_bytes(), oid, entry.filemode())?;
    }
    Ok(builder.write()?)
}

fn initialize_prepared_repo(
    host: &Repository,
    host_head: Option<Oid>,
    destination: &Path,
    verbose: bool,
) -> Result<(Repository, Oid), GitError> {
    let template_dir = empty_git_template_dir()?;
    let mut init = isolated_git_command();
    init.args(["init", "--quiet", "-b", "main"]);
    add_git_template_option(&mut init, template_dir.path());
    init.arg(destination);
    if verbose {
        let status = init
            .status()
            .map_err(|error| GitError::Ingress(format!("starting git init: {error}")))?;
        if !status.success() {
            return Err(GitError::Ingress(
                "git init for prepared workspace failed".into(),
            ));
        }
    } else {
        let output = init
            .output()
            .map_err(|error| GitError::Ingress(format!("starting git init: {error}")))?;
        if !output.status.success() {
            return Err(GitError::Ingress(format!(
                "git init for prepared workspace failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }

    let destination_repo = Repository::open(destination).map_err(|error| {
        GitError::Ingress(format!(
            "opening prepared repository {}: {error}",
            destination.display()
        ))
    })?;
    let tree_oid = if let Some(host_head) = host_head {
        let host_commit = host.find_commit(host_head)?;
        let host_tree = host_commit.tree()?;
        copy_tree_objects(host, &destination_repo, &host_tree)?
    } else {
        destination_repo.treebuilder(None)?.write()?
    };
    let tree = destination_repo.find_tree(tree_oid)?;
    let signature = git2::Signature::now("capsule", "capsule@localhost")?;
    let baseline = destination_repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "capsule: redacted workspace baseline",
        &tree,
        &[],
    )?;
    let baseline_commit = destination_repo.find_commit(baseline)?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    destination_repo.reset(
        baseline_commit.as_object(),
        ResetType::Hard,
        Some(&mut checkout),
    )?;
    drop(tree);
    drop(baseline_commit);
    Ok((destination_repo, baseline))
}

#[derive(Clone, Copy, Debug)]
struct HistoryMapping {
    capsule_baseline: Oid,
    host_baseline: Option<Oid>,
}

struct PreparedGitSource {
    directory: tempfile::TempDir,
    wip_oid: Option<Oid>,
    snapshot_head: Oid,
    history_mapping: Option<HistoryMapping>,
}

fn history_mapping_path(
    host_repo: &Path,
    session: &capscode_core::SessionId,
) -> Result<PathBuf, GitError> {
    let repo =
        Repository::open(host_repo).map_err(|_| GitError::NotARepo(host_repo.to_path_buf()))?;
    Ok(repo
        .path()
        .join("capscode")
        .join(format!("{}-history.json", session.as_str())))
}

fn persist_history_mapping(
    host_repo: &Path,
    session: &capscode_core::SessionId,
    mapping: HistoryMapping,
) -> Result<(), GitError> {
    let path = history_mapping_path(host_repo, session)?;
    let parent = path
        .parent()
        .ok_or_else(|| GitError::Ingress("history metadata has no parent directory".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        GitError::Ingress(format!(
            "creating local history metadata directory: {error}"
        ))
    })?;
    let value = serde_json::json!({
        "capsule_baseline": mapping.capsule_baseline.to_string(),
        "host_baseline": mapping.host_baseline.map(|oid| oid.to_string()),
    });
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| GitError::Ingress(format!("creating local history metadata: {error}")))?;
    serde_json::to_writer(temporary.as_file_mut(), &value)
        .map_err(|error| GitError::Ingress(format!("writing local history metadata: {error}")))?;
    temporary
        .persist(&path)
        .map_err(|error| GitError::Ingress(format!("saving local history metadata: {error}")))?;
    Ok(())
}

fn read_history_mapping(
    host_repo: &Path,
    session: &capscode_core::SessionId,
) -> Result<Option<HistoryMapping>, GitError> {
    let path = history_mapping_path(host_repo, session)?;
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(GitError::Egress(format!(
                "reading local history metadata {}: {error}",
                path.display()
            )));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| GitError::Egress(format!("invalid local history metadata: {error}")))?;
    let parse_oid = |key: &str| -> Result<Oid, GitError> {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| GitError::Egress(format!("local history metadata is missing {key}")))?
            .parse()
            .map_err(|error| {
                GitError::Egress(format!("invalid {key} in local history metadata: {error}"))
            })
    };
    let capsule_baseline = parse_oid("capsule_baseline")?;
    let host_baseline = match value.get("host_baseline") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => Some(value.parse().map_err(|error| {
            GitError::Egress(format!(
                "invalid host_baseline in local history metadata: {error}"
            ))
        })?),
        _ => {
            return Err(GitError::Egress(
                "local history metadata is missing host_baseline".into(),
            ));
        }
    };
    Ok(Some(HistoryMapping {
        capsule_baseline,
        host_baseline,
    }))
}

fn remove_history_mapping(
    host_repo: &Path,
    session: &capscode_core::SessionId,
) -> Result<(), GitError> {
    let path = history_mapping_path(host_repo, session)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(GitError::Egress(format!(
            "removing local history metadata {}: {error}",
            path.display()
        ))),
    }
}

fn collect_commits_after_baseline(
    repo: &Repository,
    oid: Oid,
    baseline: Oid,
    visited: &mut HashSet<Oid>,
    ordered: &mut Vec<Oid>,
) -> Result<(), GitError> {
    if oid == baseline || !visited.insert(oid) {
        return Ok(());
    }
    let commit = repo
        .find_commit(oid)
        .map_err(|error| GitError::Egress(format!("reading capsule commit {oid}: {error}")))?;
    for parent in commit.parent_ids() {
        collect_commits_after_baseline(repo, parent, baseline, visited, ordered)?;
    }
    ordered.push(oid);
    Ok(())
}

fn has_synthetic_history_root(repo: &Repository, head: Oid) -> Result<bool, GitError> {
    let mut commit = repo
        .find_commit(head)
        .map_err(|error| GitError::Egress(format!("reading capsule head {head}: {error}")))?;
    while commit.parent_count() > 0 {
        commit = commit.parent(0).map_err(|error| {
            GitError::Egress(format!("reading capsule history before {head}: {error}"))
        })?;
    }
    let author = commit.author();
    let committer = commit.committer();
    Ok(author.name().ok() == Some("capsule")
        && author.email().ok() == Some("capsule@localhost")
        && committer.name().ok() == Some("capsule")
        && committer.email().ok() == Some("capsule@localhost")
        && commit.message().ok() == Some("capsule: redacted workspace baseline"))
}

fn recreate_redacted_history(
    repo: &Repository,
    source_head: Oid,
    mapping: HistoryMapping,
) -> Result<Oid, GitError> {
    let capsule_baseline = repo
        .find_commit(mapping.capsule_baseline)
        .map_err(|error| {
            GitError::Egress(format!(
                "capsule history is missing its synthetic baseline: {error}"
            ))
        })?;
    let host_baseline = mapping
        .host_baseline
        .map(|oid| {
            repo.find_commit(oid).map_err(|error| {
                GitError::Egress(format!("host baseline {oid} is unavailable: {error}"))
            })
        })
        .transpose()?;
    let mut ordered = Vec::new();
    collect_commits_after_baseline(
        repo,
        source_head,
        capsule_baseline.id(),
        &mut HashSet::new(),
        &mut ordered,
    )?;
    if ordered.is_empty() {
        return Ok(host_baseline.map_or(capsule_baseline.id(), |commit| commit.id()));
    }

    let mut remapped = BTreeMap::new();
    for oid in ordered {
        let source = repo
            .find_commit(oid)
            .map_err(|error| GitError::Egress(format!("reading capsule commit {oid}: {error}")))?;
        let mut parents = Vec::new();
        if source.parent_count() == 0 {
            // If the agent amended the synthetic root, its replacement has no
            // parent to map. Attach that commit to the real host baseline so
            // the resulting branch still joins the user's history.
            if let Some(host_baseline_oid) = mapping.host_baseline {
                parents.push(repo.find_commit(host_baseline_oid).map_err(|error| {
                    GitError::Egress(format!(
                        "reading remapped host baseline {host_baseline_oid}: {error}"
                    ))
                })?);
            }
        }
        for parent_oid in source.parent_ids() {
            let mapped_oid = if parent_oid == mapping.capsule_baseline {
                mapping.host_baseline
            } else {
                Some(*remapped.get(&parent_oid).ok_or_else(|| {
                    GitError::Egress(format!(
                        "capsule commit {oid} has an unmapped parent {parent_oid}"
                    ))
                })?)
            };
            if let Some(mapped_oid) = mapped_oid {
                parents.push(repo.find_commit(mapped_oid).map_err(|error| {
                    GitError::Egress(format!("reading remapped parent {mapped_oid}: {error}"))
                })?);
            }
        }
        let parent_refs = parents.iter().collect::<Vec<_>>();
        let author = source.author();
        let committer = source.committer();
        let message = std::str::from_utf8(source.message_bytes()).map_err(|error| {
            GitError::Egress(format!(
                "capsule commit {oid} has a non-UTF-8 message: {error}"
            ))
        })?;
        let tree = source.tree()?;
        let new_oid = repo
            .commit(None, &author, &committer, message, &tree, &parent_refs)
            .map_err(|error| {
                GitError::Egress(format!("recreating capsule commit {oid}: {error}"))
            })?;
        remapped.insert(oid, new_oid);
    }
    remapped.get(&source_head).copied().ok_or_else(|| {
        GitError::Egress("capsule head was not included in the recreated history".into())
    })
}

/// Prepare a sanitized source tree (on host temp dir) for a git project.
fn prepare_git_source(
    host_repo: &Path,
    history: HistoryDial,
    include: &[PathBuf],
    verbose: bool,
) -> Result<PreparedGitSource, GitError> {
    let canonical_host = host_repo
        .canonicalize()
        .map_err(|error| GitError::Ingress(format!("resolving project directory: {error}")))?;
    let host = Repository::open(&canonical_host)
        .map_err(|_| GitError::NotARepo(canonical_host.to_path_buf()))?;
    let host_head = match host.head() {
        Ok(head) => Some(head.peel_to_commit()?.id()),
        Err(error) if error.code() == git2::ErrorCode::UnbornBranch => None,
        Err(error) => return Err(GitError::from(error)),
    };

    let td = tempfile::tempdir().map_err(|error| GitError::Ingress(error.to_string()))?;
    let dest = td.path().to_path_buf();

    let history_mapping = if history == HistoryDial::RedactAuthors || host_head.is_none() {
        let (_prepared_repo, capsule_baseline) =
            initialize_prepared_repo(&host, host_head, &dest, verbose)?;
        if history == HistoryDial::Shallow {
            std::fs::write(
                dest.join(".git").join("shallow"),
                format!("{capsule_baseline}\n"),
            )
            .map_err(|error| {
                GitError::Ingress(format!("marking empty history shallow: {error}"))
            })?;
        }
        Some(HistoryMapping {
            capsule_baseline,
            host_baseline: host_head,
        })
    } else {
        let template_dir = empty_git_template_dir()?;
        let mut command = isolated_git_command();
        command.arg("clone").arg("--no-local").arg("--no-hardlinks");
        if history == HistoryDial::Shallow {
            command.args(["--depth", "1"]);
        }
        add_git_template_option(&mut command, template_dir.path());
        command.arg(&canonical_host).arg(&dest);
        if verbose {
            let status = command
                .status()
                .map_err(|error| GitError::Ingress(format!("starting git clone: {error}")))?;
            if !status.success() {
                return Err(GitError::Ingress("git clone prep failed".into()));
            }
        } else {
            let output = command
                .output()
                .map_err(|error| GitError::Ingress(format!("starting git clone: {error}")))?;
            if !output.status.success() {
                return Err(GitError::Ingress(format!(
                    "git clone prep failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        None
    };

    if history == HistoryDial::Shallow {
        let output = run_git_capture(&dest, &["rev-parse", "--is-shallow-repository"])?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            return Err(GitError::Ingress(format!(
                "could not verify requested shallow history: git rev-parse failed: {}",
                if error.trim().is_empty() {
                    "no error output"
                } else {
                    error.trim()
                }
            )));
        }
        if value != "true" {
            return Err(GitError::Ingress(format!(
                "could not verify requested shallow history: git reported `{value}`"
            )));
        }
    }

    // Analyze dirty state on the original project.
    let mut dirty_tracked: Vec<PathBuf> = vec![];
    {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true);
        opts.recurse_untracked_dirs(true);
        let statuses = host.statuses(Some(&mut opts)).map_err(GitError::from)?;
        for entry in statuses.iter() {
            let path = PathBuf::from(entry.path().unwrap_or_default());
            let status = entry.status();
            if status.intersects(
                Status::WT_MODIFIED
                    | Status::WT_DELETED
                    | Status::WT_TYPECHANGE
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_TYPECHANGE,
            ) {
                dirty_tracked.push(path);
            }
        }
    }

    for relative in &dirty_tracked {
        let source = canonical_host.join(relative);
        match std::fs::symlink_metadata(&source) {
            Ok(_) => copy_entry(&source, &dest, relative)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remove_prepared_entry(&dest, relative)?;
            }
            Err(error) => {
                return Err(GitError::Ingress(format!(
                    "inspecting dirty project file {}: {error}",
                    relative.display()
                )));
            }
        }
    }

    let mut included_paths = Vec::new();
    for include in include {
        let (source, relative) = resolve_include(&canonical_host, include)?;
        if relative.as_os_str().is_empty() {
            copy_dir_all(&source, &dest, &relative)?;
        } else {
            copy_entry(&source, &dest, &relative)?;
        }
        included_paths.push(relative);
    }

    run_git_blocking(&dest, &["add", "-u"], verbose)?;
    for relative in &included_paths {
        run_git_add_path(&dest, relative, verbose)?;
    }
    let wip_oid = if has_staged_changes(&dest)? {
        run_git_blocking(
            &dest,
            &[
                "-c",
                "user.name=capsule",
                "-c",
                "user.email=capsule@localhost",
                "commit",
                "-m",
                "capsule: uncommitted changes",
            ],
            verbose,
        )?;
        Some(
            Repository::open(&dest)
                .map_err(GitError::from)?
                .head()?
                .peel_to_commit()?
                .id(),
        )
    } else {
        None
    };

    let remotes = run_git_capture(&dest, &["remote"])?;
    if !remotes.status.success() {
        return Err(GitError::Ingress(format!(
            "checking prepared repository remotes failed: {}",
            String::from_utf8_lossy(&remotes.stderr).trim()
        )));
    }
    if String::from_utf8_lossy(&remotes.stdout)
        .lines()
        .any(|remote| remote == "origin")
    {
        run_git_blocking(&dest, &["remote", "remove", "origin"], verbose)?;
    }
    let prepared_repo = Repository::open(&dest).map_err(GitError::from)?;
    let snapshot_head = prepared_repo.head()?.peel_to_commit()?.id();
    prepared_repo.reference(
        "refs/capscode/baseline",
        snapshot_head,
        true,
        "capscode: pin capsule baseline",
    )?;

    Ok(PreparedGitSource {
        directory: td,
        wip_oid,
        snapshot_head,
        history_mapping,
    })
}

fn prepare_rsync_source(host_repo: &Path) -> Result<tempfile::TempDir, GitError> {
    let td = tempfile::tempdir().map_err(|error| GitError::Ingress(error.to_string()))?;
    // rsync -a (portable; --no-xattrs not universal). We scrub xattrs inside seeder when non-git.
    let src_arg = format!("{}/", host_repo.display());
    let dst_arg = format!("{}/", td.path().display());
    let status = std::process::Command::new("rsync")
        .args(["-a", "--delete"])
        .arg("--exclude")
        .arg(".git")
        .arg(&src_arg)
        .arg(&dst_arg)
        .status()
        .map_err(|error| GitError::Ingress(format!("starting rsync: {error}")))?;
    if !status.success() {
        return Err(GitError::Ingress("rsync fallback prepare failed".into()));
    }
    Ok(td)
}

fn shadow_tree_usage(root: &Path) -> Result<(u64, u64), GitError> {
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            GitError::Ingress(format!(
                "reading file sync ingress tree {}: {error}",
                directory.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| GitError::Ingress(error.to_string()))?;
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| GitError::Ingress(error.to_string()))?;
            if metadata.file_type().is_dir() {
                pending.push(entry.path());
            } else {
                files = files.saturating_add(1);
                bytes = bytes.saturating_add(metadata.len());
            }
            if files > SHADOW_MAX_FILES || bytes > SHADOW_MAX_BYTES {
                return Err(GitError::Ingress(format!(
                    "non-git workspace is too large for safe file sync capture ({files} files, {bytes} bytes; limits are {SHADOW_MAX_FILES} files and {SHADOW_MAX_BYTES} bytes)"
                )));
            }
        }
    }
    Ok((files, bytes))
}

#[cfg(unix)]
fn available_space(path: &Path) -> Result<Option<u64>, GitError> {
    let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        GitError::Ingress(format!(
            "session state path contains a NUL byte: {}",
            path.display()
        ))
    })?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `encoded` is NUL-terminated and `stats` points to writable storage
    // for a `statvfs` value that is read only after libc reports success.
    let status = unsafe { libc::statvfs(encoded.as_ptr(), stats.as_mut_ptr()) };
    if status != 0 {
        return Err(GitError::Ingress(format!(
            "checking free space for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `statvfs` returned success and initialized the output value.
    let stats = unsafe { stats.assume_init() };
    let available = (stats.f_bavail as u128).saturating_mul(stats.f_frsize as u128);
    Ok(Some(available.min(u128::from(u64::MAX)) as u64))
}

#[cfg(not(unix))]
fn available_space(_path: &Path) -> Result<Option<u64>, GitError> {
    Ok(None)
}

fn check_shadow_capacity(prep_dir: &Path, state_dir: &Path) -> Result<(), GitError> {
    let (_, bytes) = shadow_tree_usage(prep_dir)?;
    let required = bytes
        .saturating_add(bytes / 10)
        .saturating_add(SHADOW_FREE_SPACE_MARGIN);
    if let Some(available) = available_space(state_dir)? {
        if available < required {
            return Err(GitError::Ingress(format!(
                "insufficient free space for non-git file sync capture: need approximately {required} bytes, but only {available} bytes are available in {}",
                state_dir.display()
            )));
        }
    }
    Ok(())
}

fn shadow_git_command(git_dir: &Path, work_tree: &Path) -> Command {
    let mut git_dir_arg = OsString::from("--git-dir=");
    git_dir_arg.push(git_dir);
    let mut work_tree_arg = OsString::from("--work-tree=");
    work_tree_arg.push(work_tree);

    let mut command = Command::new("git");
    command
        .arg(git_dir_arg)
        .arg(work_tree_arg)
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");

    const REMOVE: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
        "GIT_TEMPLATE_DIR",
        "GIT_ATTR_SOURCE",
    ];
    for name in REMOVE {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let name_text = name.to_string_lossy();
        if name_text.starts_with("GIT_AUTHOR_")
            || name_text.starts_with("GIT_COMMITTER_")
            || name_text.starts_with("GIT_CONFIG_KEY_")
            || name_text.starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(name);
        }
    }
    command
}

fn require_shadow_git_success(operation: &str, output: Output) -> Result<Output, GitError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(GitError::Ingress(format!(
            "file sync git {operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn build_shadow_repo(prep_dir: &Path, state_dir: &Path) -> Result<(PathBuf, Oid), GitError> {
    check_shadow_capacity(prep_dir, state_dir)?;
    let git_dir = state_dir.join("shadow.git");
    if git_dir.exists() {
        return Err(GitError::Ingress(format!(
            "refusing to overwrite existing file sync repository {}",
            git_dir.display()
        )));
    }

    // `git init --git-dir=...` initializes the repository contents but does not
    // create a missing parent directory for the git dir itself.
    std::fs::create_dir_all(&git_dir)
        .map_err(|error| GitError::Ingress(format!("creating file sync Git directory: {error}")))?;

    let template_dir = empty_git_template_dir()?;
    let mut init = shadow_git_command(&git_dir, prep_dir);
    init.args(["init", "-q", "-b", "main"]);
    add_git_template_option(&mut init, template_dir.path());
    require_shadow_git_success(
        "init",
        init.output()
            .map_err(|error| GitError::Ingress(format!("starting file sync git init: {error}")))?,
    )?;

    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir).map_err(|error| {
        GitError::Ingress(format!("creating file sync Git info directory: {error}"))
    })?;
    std::fs::write(
        info_dir.join("attributes"),
        "* -text -eol -filter -ident -working-tree-encoding\n",
    )
    .map_err(|error| GitError::Ingress(format!("neutralizing file sync attributes: {error}")))?;

    let mut add = shadow_git_command(&git_dir, prep_dir);
    add.args(["add", "-A", "-f"]);
    require_shadow_git_success(
        "add",
        add.output()
            .map_err(|error| GitError::Ingress(format!("starting file sync git add: {error}")))?,
    )?;

    let mut commit = shadow_git_command(&git_dir, prep_dir);
    commit.args([
        "-c",
        "user.name=capscode",
        "-c",
        "user.email=capscode@localhost",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "capscode: ingress baseline",
    ]);
    require_shadow_git_success(
        "commit",
        commit.output().map_err(|error| {
            GitError::Ingress(format!("starting file sync git commit: {error}"))
        })?,
    )?;

    let mut rev_parse = shadow_git_command(&git_dir, prep_dir);
    rev_parse.args(["rev-parse", "--verify", "HEAD"]);
    let output = require_shadow_git_success(
        "rev-parse",
        rev_parse.output().map_err(|error| {
            GitError::Ingress(format!("starting file sync git rev-parse: {error}"))
        })?,
    )?;
    let baseline = std::str::from_utf8(&output.stdout)
        .map_err(|error| GitError::Ingress(format!("file sync baseline is not UTF-8: {error}")))?
        .trim()
        .parse::<Oid>()
        .map_err(|error| GitError::Ingress(format!("invalid file sync baseline OID: {error}")))?;
    Ok((git_dir, baseline))
}

fn persist_receipt(state_dir: &Path, receipt: &IngressReceipt) -> Result<(), GitError> {
    let mut temporary = tempfile::NamedTempFile::new_in(state_dir)
        .map_err(|error| GitError::Ingress(format!("creating receipt temp file: {error}")))?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), receipt)
        .map_err(|error| GitError::Ingress(format!("serializing ingress receipt: {error}")))?;
    temporary
        .as_file_mut()
        .write_all(b"\n")
        .map_err(|error| GitError::Ingress(format!("writing ingress receipt: {error}")))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| GitError::Ingress(format!("syncing ingress receipt: {error}")))?;
    temporary
        .persist(state_dir.join(RECEIPT_FILE))
        .map_err(|error| GitError::Ingress(format!("persisting ingress receipt: {error}")))?;
    #[cfg(unix)]
    File::open(state_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| GitError::Ingress(format!("syncing session state directory: {error}")))?;
    Ok(())
}

fn shadow_egress_error(message: impl Into<String>) -> GitError {
    GitError::Egress(message.into())
}

fn require_shadow_egress_success(operation: &str, output: Output) -> Result<Output, GitError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(shadow_egress_error(format!(
            "file sync git {operation} failed: {}",
            escape_command_output(&output.stderr)
        )))
    }
}

fn shadow_export_tar_args(workspace: &Path) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--exclude=./.capscode-autosync.git"),
        OsString::from("-C"),
        workspace.as_os_str().to_os_string(),
    ];
    args.extend(["-cf", "-", "."].map(OsString::from));
    args
}

fn shadow_exporter_args(source: &CapsuleWorkspaceSource) -> Vec<OsString> {
    let mut args = vec![
        "run".into(),
        "--rm".into(),
        "--network".into(),
        "none".into(),
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--cap-add".into(),
        "DAC_OVERRIDE".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        // Mount only the workspace volume. Copying every capsule mount would
        // also pull in per-session mounts that may already be gone.
        "--mount".into(),
        format!(
            "type=volume,src={},dst=/workspace,readonly",
            source.workspace_volume
        )
        .into(),
        "--entrypoint".into(),
        "/usr/bin/tar".into(),
        source.image.as_str().into(),
    ];
    args.extend(shadow_export_tar_args(Path::new("/workspace")));
    args
}

/// What the file sync exporter needs from the stopped capsule: its image and
/// the named volume mounted at /workspace.
struct CapsuleWorkspaceSource {
    image: String,
    workspace_volume: String,
}

async fn inspect_capsule_workspace(
    capsule: &ContainerId,
) -> Result<CapsuleWorkspaceSource, GitError> {
    let mut inspect = TokioCommand::new("docker");
    configure_tokio_docker_command(&mut inspect);
    let output = inspect
        .args([
            "inspect",
            "--format={{.Image}} {{range .Mounts}}{{if eq .Destination \"/workspace\"}}{{.Type}}:{{.Name}}{{end}}{{end}}",
            &capsule.0,
        ])
        .output()
        .await
        .map_err(|error| shadow_egress_error(format!("inspecting capsule: {error}")))?;
    if !output.status.success() {
        return Err(shadow_egress_error(format!(
            "inspecting capsule failed: {}",
            escape_command_output(&output.stderr)
        )));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|error| {
        shadow_egress_error(format!("capsule inspect output is not UTF-8: {error}"))
    })?;
    parse_capsule_workspace_source(text)
}

fn parse_capsule_workspace_source(text: &str) -> Result<CapsuleWorkspaceSource, GitError> {
    let mut fields = text.split_whitespace();
    let image = fields.next().unwrap_or_default();
    let mount = fields.next().unwrap_or_default();
    let valid_image_id = image.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if !valid_image_id {
        return Err(shadow_egress_error(
            "Docker returned an invalid capsule image id",
        ));
    }
    let volume = mount.strip_prefix("volume:").unwrap_or_default();
    let valid_volume = !volume.is_empty()
        && volume
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if !valid_volume {
        return Err(shadow_egress_error(
            "the capsule's /workspace is not a named Docker volume, so it cannot be exported",
        ));
    }
    Ok(CapsuleWorkspaceSource {
        image: image.to_owned(),
        workspace_volume: volume.to_owned(),
    })
}

async fn capture_shadow_tree(
    source: &CapsuleWorkspaceSource,
    destination: &Path,
    verbose: bool,
) -> Result<(), GitError> {
    // `docker exec` cannot run after quiescence stops the capsule. Use a fresh,
    // networkless helper based on the same local image and mount its volumes
    // read-only. The archive filter runs in that container, before transfer.
    let mut archive_command = TokioCommand::new("docker");
    configure_tokio_docker_command(&mut archive_command);
    archive_command
        .args(shadow_exporter_args(source))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut archive = archive_command
        .spawn()
        .map_err(|error| shadow_egress_error(format!("starting workspace archive: {error}")))?;
    let mut archive_stdout = archive.stdout.take().ok_or_else(|| {
        shadow_egress_error("workspace archive did not provide its output stream")
    })?;
    let mut archive_stderr = archive
        .stderr
        .take()
        .ok_or_else(|| shadow_egress_error("workspace archive did not provide its error stream"))?;
    let archive_stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut output = Vec::new();
        archive_stderr.read_to_end(&mut output).await?;
        Ok::<_, std::io::Error>(output)
    });

    let mut extract_command = TokioCommand::new("tar");
    extract_command
        .args(["-x", "-f", "-", "-C"])
        .arg(destination)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut extract = match extract_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = archive.kill().await;
            let _ = archive.wait().await;
            let _ = archive_stderr_task.await;
            return Err(shadow_egress_error(format!(
                "starting workspace archive extraction: {error}"
            )));
        }
    };
    let mut extract_stdin = extract.stdin.take().ok_or_else(|| {
        shadow_egress_error("workspace extractor did not provide its input stream")
    })?;
    let mut extract_stderr = extract.stderr.take().ok_or_else(|| {
        shadow_egress_error("workspace extractor did not provide its error stream")
    })?;
    let extract_stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut output = Vec::new();
        extract_stderr.read_to_end(&mut output).await?;
        Ok::<_, std::io::Error>(output)
    });

    let transfer = tokio::io::copy(&mut archive_stdout, &mut extract_stdin).await;
    drop(extract_stdin);
    if transfer.is_err() {
        let _ = archive.kill().await;
        let _ = extract.kill().await;
    }
    let (archive_status, extract_status) = tokio::join!(archive.wait(), extract.wait());
    let archive_status = archive_status
        .map_err(|error| shadow_egress_error(format!("waiting for workspace archive: {error}")))?;
    let extract_status = extract_status.map_err(|error| {
        shadow_egress_error(format!("waiting for workspace archive extraction: {error}"))
    })?;
    let archive_stderr = archive_stderr_task
        .await
        .map_err(|error| shadow_egress_error(format!("reading workspace archive errors: {error}")))?
        .map_err(|error| {
            shadow_egress_error(format!("reading workspace archive errors: {error}"))
        })?;
    let extract_stderr = extract_stderr_task
        .await
        .map_err(|error| {
            shadow_egress_error(format!("reading workspace extraction errors: {error}"))
        })?
        .map_err(|error| {
            shadow_egress_error(format!("reading workspace extraction errors: {error}"))
        })?;

    if !archive_status.success() {
        return Err(shadow_egress_error(format!(
            "creating capsule workspace archive failed: {}",
            escape_command_output(&archive_stderr)
        )));
    }
    if !extract_status.success() {
        return Err(shadow_egress_error(format!(
            "extracting capsule workspace archive failed: {}",
            escape_command_output(&extract_stderr)
        )));
    }
    transfer.map_err(|error| {
        shadow_egress_error(format!("copying capsule workspace archive: {error}"))
    })?;

    if verbose {
        if !archive_stderr.is_empty() {
            eprint!("{}", escape_command_output(&archive_stderr));
        }
        if !extract_stderr.is_empty() {
            eprint!("{}", escape_command_output(&extract_stderr));
        }
    }
    Ok(())
}

pub async fn capture_shadow_export(
    capsule: &ContainerId,
    state_dir: &Path,
    verbose: bool,
) -> Result<PathBuf, GitError> {
    let export = state_dir.join("export");
    let partial = state_dir.join("export.partial");
    if export.exists() || partial.exists() {
        return Err(shadow_egress_error(format!(
            "refusing to overwrite an existing file sync export in {}",
            state_dir.display()
        )));
    }
    std::fs::create_dir(&partial).map_err(|error| {
        shadow_egress_error(format!(
            "creating file sync export directory {}: {error}",
            partial.display()
        ))
    })?;

    let source = inspect_capsule_workspace(capsule).await?;
    capture_shadow_tree(&source, &partial, verbose).await?;
    remove_private_shadow_roots(&partial)?;
    std::fs::rename(&partial, &export).map_err(|error| {
        shadow_egress_error(format!(
            "finalizing file sync export {}: {error}",
            export.display()
        ))
    })?;
    Ok(export)
}

/// Capture a FRESH export, superseding any earlier one.
///
/// `capture_shadow_export` refuses to overwrite and `shadow_egress` reuses
/// whatever it finds. That is right for the exit path, which captures once in
/// `Draining` and hands off in `Egressing`. It is wrong for an on-demand
/// `sync` or `rm`, which would otherwise hand off the snapshot taken by an
/// earlier call and silently report stale work as current.
///
/// The previous export is moved aside, never deleted up front: if the new
/// capture fails, it is put back. A re-capture must not be able to destroy the
/// only host-side copy of the work.
pub async fn recapture_shadow_export(
    capsule: &ContainerId,
    state_dir: &Path,
    verbose: bool,
) -> Result<PathBuf, GitError> {
    let had_previous = stage_previous_export(state_dir)?;
    match capture_shadow_export(capsule, state_dir, verbose).await {
        Ok(fresh) => {
            commit_recapture(state_dir, had_previous);
            Ok(fresh)
        }
        Err(error) => Err(rollback_recapture(state_dir, had_previous, error)),
    }
}

/// Everything one handoff round writes into the state dir, and therefore
/// everything a second round would collide with.
const HANDOFF_ROUND_ARTIFACTS: [&str; 3] = ["staging", MANIFEST_FILE, "backup"];

/// Move a previous handoff round aside so a fresh one can run in its place.
///
/// The shadow egress refuses to overwrite its staging directory or its manifest.
/// That refusal is correct — staging holds what was NOT applied (conflicts and
/// deletions) and backup holds host files that WERE overwritten, so either can
/// be the only copy of something — but it is also what makes a second on-demand
/// `sync` fail outright. So the round is retired whole, into one numbered
/// directory, and never deleted. Returns where it went, for the caller to report.
pub fn retire_previous_handoff(state_dir: &Path) -> Result<Option<PathBuf>, GitError> {
    let present: Vec<&str> = HANDOFF_ROUND_ARTIFACTS
        .into_iter()
        .filter(|name| state_dir.join(name).exists())
        .collect();
    if present.is_empty() {
        return Ok(None);
    }

    // Count up rather than reusing one name: two retired rounds must not
    // collide, since the older one may hold the only copy of a conflict.
    let retired = (1..)
        .map(|index| state_dir.join(format!("previous-handoff.{index}")))
        .find(|candidate| !candidate.exists())
        .expect("an unbounded range always yields a free name");
    std::fs::create_dir(&retired).map_err(|error| {
        shadow_egress_error(format!(
            "creating {} for the previous handoff: {error}",
            retired.display()
        ))
    })?;
    for name in present {
        std::fs::rename(state_dir.join(name), retired.join(name)).map_err(|error| {
            shadow_egress_error(format!(
                "retiring the previous handoff's {name} to {}: {error}",
                retired.display()
            ))
        })?;
    }
    Ok(Some(retired))
}

fn superseded_export(state_dir: &Path) -> PathBuf {
    state_dir.join("export.superseded")
}

/// Clear crash debris and move any live export aside. Returns whether there was
/// one to restore.
fn stage_previous_export(state_dir: &Path) -> Result<bool, GitError> {
    let export = state_dir.join("export");
    let superseded = superseded_export(state_dir);

    // Both are debris from a crashed run: an interrupted capture leaves
    // `export.partial`, an interrupted re-capture leaves `export.superseded`.
    // Neither is the live copy, and either would block the capture.
    for stale in [&superseded, &state_dir.join("export.partial")] {
        if stale.exists() {
            std::fs::remove_dir_all(stale).map_err(|error| {
                shadow_egress_error(format!(
                    "clearing stale file sync export {}: {error}",
                    stale.display()
                ))
            })?;
        }
    }

    if !export.exists() {
        return Ok(false);
    }
    std::fs::rename(&export, &superseded)
        .map_err(|error| shadow_egress_error(format!("setting the previous export aside: {error}")))
        .map(|()| true)
}

/// Reap the superseded copy. Best effort by design: the fresh capture is already
/// in place, so a failure here costs disk, not correctness.
fn commit_recapture(state_dir: &Path, had_previous: bool) {
    if had_previous {
        let _ = std::fs::remove_dir_all(superseded_export(state_dir));
    }
}

/// Put the previous export back after a failed capture. If even that fails, the
/// error must name where the copy actually is — the user's work is in it.
fn rollback_recapture(state_dir: &Path, had_previous: bool, error: GitError) -> GitError {
    if !had_previous {
        return error;
    }
    let superseded = superseded_export(state_dir);
    match std::fs::rename(&superseded, state_dir.join("export")) {
        Ok(()) => error,
        Err(restore) => shadow_egress_error(format!(
            "file sync re-capture failed ({error}), and restoring the previous export also failed ({restore}); the previous copy is at {}",
            superseded.display()
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawShadowChange {
    status: u8,
    path: Vec<u8>,
}

fn parse_name_status_z(bytes: &[u8]) -> Result<Vec<RawShadowChange>, GitError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&0) {
        return Err(shadow_egress_error(
            "file sync git name-status output was not NUL terminated",
        ));
    }
    let mut fields = bytes[..bytes.len() - 1].split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(status_field) = fields.next() {
        let path = fields.next().ok_or_else(|| {
            shadow_egress_error("file sync git name-status output had an incomplete record")
        })?;
        let status = match status_field {
            [status @ (b'A' | b'M' | b'D' | b'T')] => *status,
            _ => {
                return Err(shadow_egress_error(format!(
                    "unrecognized file sync git name-status record: {}",
                    escape_command_output(status_field)
                )))
            }
        };
        if path.is_empty() {
            return Err(shadow_egress_error("file sync git reported an empty path"));
        }
        changes.push(RawShadowChange {
            status,
            path: path.to_vec(),
        });
    }
    Ok(changes)
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn escape_shadow_path(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = path.as_os_str().as_bytes();
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().as_bytes();

    if let Ok(text) = std::str::from_utf8(bytes) {
        return text
            .chars()
            .flat_map(|character| character.escape_default())
            .collect();
    }
    let mut escaped = String::new();
    for byte in bytes {
        if byte.is_ascii_graphic() && *byte != b'\\' {
            escaped.push(char::from(*byte));
        } else if *byte == b' ' {
            escaped.push(' ');
        } else {
            escaped.push_str(&format!("\\x{byte:02x}"));
        }
    }
    escaped
}

fn escape_command_output(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .flat_map(char::escape_default)
        .collect()
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    use std::path::Component;

    if path.as_os_str().is_empty() || path.is_absolute() || path.to_str().is_none() {
        return Err("path must be non-empty, relative, and valid UTF-8".to_string());
    }
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| "path contains non-UTF-8 data".to_string())?;
                if name.is_empty() || name.eq_ignore_ascii_case(".git") {
                    return Err("path contains a forbidden .git component".to_string());
                }
            }
            Component::CurDir | Component::ParentDir => {
                return Err("path contains a . or .. component".to_string())
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err("path is not relative".to_string())
            }
        }
    }
    Ok(())
}

fn validate_path_set(root: &Path, paths: &[PathBuf]) -> Result<(), GitError> {
    let mut exact = HashSet::new();
    let mut folded = HashSet::new();
    let mut component_sets = Vec::with_capacity(paths.len());

    for path in paths {
        validate_relative_path(path).map_err(|reason| {
            shadow_egress_error(format!(
                "unsafe apply path {}: {reason}",
                escape_shadow_path(path)
            ))
        })?;
        let text = path
            .to_str()
            .expect("validated relative path must be valid UTF-8");
        if !exact.insert(text.to_string()) {
            return Err(shadow_egress_error(format!(
                "duplicate apply path {}",
                escape_shadow_path(path)
            )));
        }
        let case_folded: String = text.chars().flat_map(char::to_lowercase).collect();
        if !folded.insert(case_folded) {
            return Err(shadow_egress_error(format!(
                "case-insensitive apply-path collision at {}",
                escape_shadow_path(path)
            )));
        }
        component_sets.push(
            path.components()
                .map(|component| component.as_os_str().to_os_string())
                .collect::<Vec<_>>(),
        );
    }

    for left in 0..component_sets.len() {
        for right in left + 1..component_sets.len() {
            let a = &component_sets[left];
            let b = &component_sets[right];
            if (a.len() < b.len() && b.starts_with(a)) || (b.len() < a.len() && a.starts_with(b)) {
                return Err(shadow_egress_error(format!(
                    "ancestor/descendant apply-path collision between {} and {}",
                    escape_shadow_path(&paths[left]),
                    escape_shadow_path(&paths[right])
                )));
            }
        }
    }

    let root_metadata = std::fs::symlink_metadata(root).map_err(|error| {
        shadow_egress_error(format!("inspecting apply root {}: {error}", root.display()))
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(shadow_egress_error(format!(
            "apply root {} is not a real directory",
            root.display()
        )));
    }
    for path in paths {
        let components: Vec<_> = path.components().collect();
        let mut current = root.to_path_buf();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            current.push(component.as_os_str());
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(shadow_egress_error(format!(
                        "refusing symlink component {}",
                        escape_shadow_path(path)
                    )))
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(shadow_egress_error(format!(
                        "non-directory apply component {}",
                        escape_shadow_path(path)
                    )))
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(shadow_egress_error(format!(
                        "inspecting apply component {}: {error}",
                        escape_shadow_path(path)
                    )))
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct GitFileState {
    mode: u32,
    blob: Oid,
}

fn parse_tree_records(bytes: &[u8]) -> Result<BTreeMap<Vec<u8>, GitFileState>, GitError> {
    let mut records = BTreeMap::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| {
                shadow_egress_error("file sync git ls-tree returned a record without a path")
            })?;
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|error| shadow_egress_error(format!("invalid ls-tree header: {error}")))?;
        let mut fields = header.split_ascii_whitespace();
        let mode = u32::from_str_radix(
            fields
                .next()
                .ok_or_else(|| shadow_egress_error("ls-tree mode missing"))?,
            8,
        )
        .map_err(|error| shadow_egress_error(format!("invalid ls-tree mode: {error}")))?;
        let kind = fields
            .next()
            .ok_or_else(|| shadow_egress_error("ls-tree object type missing"))?;
        let blob = fields
            .next()
            .ok_or_else(|| shadow_egress_error("ls-tree object id missing"))?
            .parse::<Oid>()
            .map_err(|error| shadow_egress_error(format!("invalid ls-tree object id: {error}")))?;
        if fields.next().is_some() || kind != "blob" {
            return Err(shadow_egress_error(format!(
                "unsupported baseline tree entry type {kind}"
            )));
        }
        if records
            .insert(record[tab + 1..].to_vec(), GitFileState { mode, blob })
            .is_some()
        {
            return Err(shadow_egress_error(
                "duplicate path in file sync baseline tree",
            ));
        }
    }
    Ok(records)
}

fn parse_index_records(bytes: &[u8]) -> Result<BTreeMap<Vec<u8>, GitFileState>, GitError> {
    let mut records = BTreeMap::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| {
                shadow_egress_error("file sync git ls-files returned a record without a path")
            })?;
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|error| shadow_egress_error(format!("invalid ls-files header: {error}")))?;
        let mut fields = header.split_ascii_whitespace();
        let mode = u32::from_str_radix(
            fields
                .next()
                .ok_or_else(|| shadow_egress_error("ls-files mode missing"))?,
            8,
        )
        .map_err(|error| shadow_egress_error(format!("invalid ls-files mode: {error}")))?;
        let blob = fields
            .next()
            .ok_or_else(|| shadow_egress_error("ls-files object id missing"))?
            .parse::<Oid>()
            .map_err(|error| shadow_egress_error(format!("invalid ls-files object id: {error}")))?;
        let stage = fields
            .next()
            .ok_or_else(|| shadow_egress_error("ls-files stage missing"))?;
        if fields.next().is_some() || stage != "0" {
            return Err(shadow_egress_error(format!(
                "unsupported file sync index stage {stage}"
            )));
        }
        if records
            .insert(record[tab + 1..].to_vec(), GitFileState { mode, blob })
            .is_some()
        {
            return Err(shadow_egress_error("duplicate path in file sync index"));
        }
    }
    Ok(records)
}

fn is_regular_git_mode(mode: u32) -> bool {
    mode == 0o100644 || mode == 0o100755
}

fn git_mode_is_executable(mode: u32) -> bool {
    mode == 0o100755
}

fn open_nofollow(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    options.open(path)
}

fn open_regular_from_root(
    root: &Path,
    relative: &Path,
) -> Result<(File, std::fs::Metadata), GitError> {
    validate_relative_path(relative).map_err(|reason| {
        shadow_egress_error(format!(
            "unsafe file path {}: {reason}",
            escape_shadow_path(relative)
        ))
    })?;
    let root_metadata = std::fs::symlink_metadata(root).map_err(|error| {
        shadow_egress_error(format!("inspecting root {}: {error}", root.display()))
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(shadow_egress_error(format!(
            "root {} is not a real directory",
            root.display()
        )));
    }

    let components: Vec<_> = relative.components().collect();
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            shadow_egress_error(format!(
                "inspecting {}: {error}",
                escape_shadow_path(relative)
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(shadow_egress_error(format!(
                "refusing to follow symlink {}",
                escape_shadow_path(relative)
            )));
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            return Err(shadow_egress_error(format!(
                "path component {} is not a directory",
                escape_shadow_path(relative)
            )));
        }
        if index + 1 == components.len() && !metadata.is_file() {
            return Err(shadow_egress_error(format!(
                "path {} is not a regular file",
                escape_shadow_path(relative)
            )));
        }
    }

    let file = open_nofollow(&current).map_err(|error| {
        shadow_egress_error(format!("opening {}: {error}", escape_shadow_path(relative)))
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| shadow_egress_error(format!("inspecting open file: {error}")))?;
    if !metadata.is_file() {
        return Err(shadow_egress_error(format!(
            "opened path {} is not a regular file",
            escape_shadow_path(relative)
        )));
    }
    Ok((file, metadata))
}

#[cfg(unix)]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    metadata.mode()
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

fn hash_open_file(git_dir: &Path, work_tree: &Path, file: File) -> Result<Oid, GitError> {
    let mut hash = shadow_git_command(git_dir, work_tree);
    hash.args(["hash-object", "--stdin"])
        .stdin(Stdio::from(file));
    let output = require_shadow_egress_success(
        "hash-object",
        hash.output()
            .map_err(|error| shadow_egress_error(format!("starting hash-object: {error}")))?,
    )?;
    std::str::from_utf8(&output.stdout)
        .map_err(|error| shadow_egress_error(format!("hash-object output is not UTF-8: {error}")))?
        .trim()
        .parse::<Oid>()
        .map_err(|error| shadow_egress_error(format!("invalid hash-object output: {error}")))
}

#[derive(Clone, Debug)]
enum HostFileState {
    Missing,
    Regular { blob: Oid, mode: u32 },
    NonFile(String),
}

fn inspect_host_file(
    host_root: &Path,
    relative: &Path,
    git_dir: &Path,
) -> Result<HostFileState, GitError> {
    validate_relative_path(relative).map_err(|reason| {
        shadow_egress_error(format!(
            "unsafe host path {}: {reason}",
            escape_shadow_path(relative)
        ))
    })?;
    let components: Vec<_> = relative.components().collect();
    let mut current = host_root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HostFileState::Missing)
            }
            Err(error) => {
                return Err(shadow_egress_error(format!(
                    "inspecting host path {}: {error}",
                    escape_shadow_path(relative)
                )))
            }
        };
        if metadata.file_type().is_symlink() {
            return Ok(HostFileState::NonFile(
                "host path has a symlink component".to_string(),
            ));
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            return Ok(HostFileState::NonFile(
                "host path has a non-directory component".to_string(),
            ));
        }
        if index + 1 == components.len() && !metadata.is_file() {
            return Ok(HostFileState::NonFile(
                "host path is not a regular file".to_string(),
            ));
        }
    }

    let (file, metadata) = open_regular_from_root(host_root, relative)?;
    let blob = hash_open_file(git_dir, host_root, file)?;
    Ok(HostFileState::Regular {
        blob,
        mode: metadata_mode(&metadata),
    })
}

fn classify_conflict(
    kind: ShadowChangeKind,
    baseline_blob: Option<Oid>,
    baseline_executable: Option<bool>,
    host: &HostFileState,
) -> Option<String> {
    match (kind, host) {
        (ShadowChangeKind::Added, HostFileState::Missing) => None,
        (ShadowChangeKind::Added, HostFileState::Regular { .. }) => {
            Some("host path was created during the session".to_string())
        }
        (ShadowChangeKind::Added, HostFileState::NonFile(reason)) => Some(reason.clone()),
        (ShadowChangeKind::Modified, HostFileState::Missing) => {
            Some("host file was deleted during the session".to_string())
        }
        (ShadowChangeKind::Modified, HostFileState::NonFile(reason)) => Some(reason.clone()),
        (ShadowChangeKind::Modified, HostFileState::Regular { blob, mode }) => {
            let Some(expected_blob) = baseline_blob else {
                return Some("baseline blob is missing".to_string());
            };
            let Some(expected_executable) = baseline_executable else {
                return Some("baseline mode is missing".to_string());
            };
            if blob != &expected_blob {
                Some("host content changed during the session".to_string())
            } else if (*mode & 0o111 != 0) != expected_executable {
                Some("host executable mode changed during the session".to_string())
            } else {
                None
            }
        }
        (ShadowChangeKind::Deleted, _) => {
            Some("deletions are not applied automatically".to_string())
        }
    }
}

fn stage_regular_file(export: &Path, staging: &Path, relative: &Path) -> Result<u32, GitError> {
    let (mut source, metadata) = open_regular_from_root(export, relative)?;
    let destination = staging.join(relative);
    let parent = destination
        .parent()
        .ok_or_else(|| shadow_egress_error("staging destination has no parent"))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        shadow_egress_error(format!(
            "creating staging directory for {}: {error}",
            escape_shadow_path(relative)
        ))
    })?;
    validate_path_set(staging, &[relative.to_path_buf()])?;
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .map_err(|error| {
            shadow_egress_error(format!(
                "creating staged file for {}: {error}",
                escape_shadow_path(relative)
            ))
        })?;
    std::io::copy(&mut source, &mut destination_file).map_err(|error| {
        shadow_egress_error(format!("staging {}: {error}", escape_shadow_path(relative)))
    })?;
    let source_mode = metadata_mode(&metadata);
    #[cfg(unix)]
    std::fs::set_permissions(
        &destination,
        std::fs::Permissions::from_mode(if source_mode & 0o111 != 0 {
            0o700
        } else {
            0o600
        }),
    )
    .map_err(|error| shadow_egress_error(format!("setting staged file permissions: {error}")))?;
    destination_file
        .sync_all()
        .map_err(|error| shadow_egress_error(format!("syncing staged file: {error}")))?;
    Ok(source_mode)
}

fn collect_shadow_unsupported(export: &Path) -> Result<Vec<ShadowIssue>, GitError> {
    let mut pending = vec![export.to_path_buf()];
    let mut issues = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        let relative_directory = directory.strip_prefix(export).unwrap_or(Path::new(""));
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            if relative_directory.as_os_str().is_empty() {
                shadow_egress_error(format!("reading file sync export root: {error}"))
            } else {
                shadow_egress_error(format!(
                    "reading file sync export directory {}: {error}",
                    escape_shadow_path(relative_directory)
                ))
            }
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                shadow_egress_error(format!("reading file sync export entry: {error}"))
            })?;
            let path = entry.path();
            let relative = path.strip_prefix(export).map_err(|error| {
                let _ = error;
                shadow_egress_error("could not resolve a file sync export path")
            })?;
            if is_private_shadow_path(relative) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                shadow_egress_error(format!(
                    "inspecting file sync export {}: {error}",
                    escape_shadow_path(relative)
                ))
            })?;
            let has_git_component = relative.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
            });
            if has_git_component {
                issues
                    .entry(relative.to_path_buf())
                    .or_insert_with(|| "nested .git content is not applied".to_string());
                continue;
            }
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                issues
                    .entry(relative.to_path_buf())
                    .or_insert_with(|| "symbolic links are not applied".to_string());
            } else if file_type.is_dir() {
                pending.push(path);
            } else if !file_type.is_file() {
                issues
                    .entry(relative.to_path_buf())
                    .or_insert_with(|| "non-regular filesystem entry is not applied".to_string());
            }
        }
    }
    Ok(issues
        .into_iter()
        .map(|(path, reason)| ShadowIssue { path, reason })
        .collect())
}

fn shadow_export_is_unchanged(changes: &[RawShadowChange], unsupported: &[ShadowIssue]) -> bool {
    changes.is_empty() && unsupported.is_empty()
}

fn empty_shadow_handoff(state_dir: &Path, git_dir: &Path) -> ShadowHandoff {
    ShadowHandoff {
        state_dir: state_dir.to_path_buf(),
        staging_dir: state_dir.join("staging"),
        backup_dir: state_dir.join("backup"),
        manifest_path: state_dir.join(MANIFEST_FILE),
        changes: Vec::new(),
        conflicts: Vec::new(),
        unsupported: Vec::new(),
        diff: String::new(),
        diff_truncated: false,
        git_dir: git_dir.to_path_buf(),
        candidates: Vec::new(),
    }
}

fn manifest_issue(issue: &ShadowIssue) -> serde_json::Value {
    serde_json::json!({
        "path": escape_shadow_path(&issue.path),
        "reason": issue.reason,
    })
}

fn write_shadow_manifest(
    state_dir: &Path,
    baseline: Oid,
    additions: Vec<serde_json::Value>,
    modifications: Vec<serde_json::Value>,
    deletions: Vec<serde_json::Value>,
    conflicts: &[ShadowIssue],
    unsupported: &[ShadowIssue],
) -> Result<PathBuf, GitError> {
    let manifest_path = state_dir.join(MANIFEST_FILE);
    if manifest_path.exists() {
        return Err(shadow_egress_error(format!(
            "refusing to overwrite file sync manifest {}",
            manifest_path.display()
        )));
    }
    let manifest = serde_json::json!({
        "baseline": baseline.to_string(),
        "additions": additions,
        "modifications": modifications,
        "deletions": deletions,
        "conflicts": conflicts.iter().map(manifest_issue).collect::<Vec<_>>(),
        "unsupported": unsupported.iter().map(manifest_issue).collect::<Vec<_>>(),
    });
    let mut temporary = tempfile::NamedTempFile::new_in(state_dir).map_err(|error| {
        shadow_egress_error(format!("creating file sync manifest temp file: {error}"))
    })?;
    serde_json::to_writer_pretty(temporary.as_file_mut(), &manifest)
        .map_err(|error| shadow_egress_error(format!("serializing file sync manifest: {error}")))?;
    temporary
        .as_file_mut()
        .write_all(b"\n")
        .map_err(|error| shadow_egress_error(format!("writing file sync manifest: {error}")))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| shadow_egress_error(format!("syncing file sync manifest: {error}")))?;
    temporary.persist(&manifest_path).map_err(|error| {
        shadow_egress_error(format!("persisting file sync manifest: {}", error.error))
    })?;
    Ok(manifest_path)
}

fn prepare_shadow_handoff(
    export: &Path,
    host_root: &Path,
    git_dir: &Path,
    baseline: Oid,
    state_dir: &Path,
) -> Result<ShadowHandoff, GitError> {
    remove_private_shadow_roots(export)?;
    let index_parent = tempfile::Builder::new()
        .prefix("shadow-egress-index-")
        .tempdir_in(state_dir)
        .map_err(|error| {
            shadow_egress_error(format!("creating file sync index directory: {error}"))
        })?;
    let index_path = index_parent.path().join("index");
    debug_assert!(!index_path.exists());

    let mut add = shadow_git_command(git_dir, export);
    add.env("GIT_INDEX_FILE", &index_path)
        .args(["add", "-A", "-f", "--", "."]);
    require_shadow_egress_success(
        "add",
        add.output()
            .map_err(|error| shadow_egress_error(format!("starting file sync git add: {error}")))?,
    )?;

    let mut diff_index = shadow_git_command(git_dir, export);
    diff_index
        .env("GIT_INDEX_FILE", &index_path)
        .args(["diff-index", "--name-status", "-z", "--no-renames"])
        .arg(baseline.to_string());
    let output = require_shadow_egress_success(
        "diff-index",
        diff_index.output().map_err(|error| {
            shadow_egress_error(format!("starting file sync git diff-index: {error}"))
        })?,
    )?;
    let raw_changes = parse_name_status_z(&output.stdout)?;
    let mut unsupported = collect_shadow_unsupported(export)?;
    if shadow_export_is_unchanged(&raw_changes, &unsupported) {
        return Ok(empty_shadow_handoff(state_dir, git_dir));
    }

    let mut ls_tree = shadow_git_command(git_dir, export);
    ls_tree.args(["ls-tree", "-rz"]).arg(baseline.to_string());
    let baseline_records = parse_tree_records(
        &require_shadow_egress_success(
            "ls-tree",
            ls_tree.output().map_err(|error| {
                shadow_egress_error(format!("starting file sync git ls-tree: {error}"))
            })?,
        )?
        .stdout,
    )?;

    let mut ls_files = shadow_git_command(git_dir, export);
    ls_files
        .env("GIT_INDEX_FILE", &index_path)
        .args(["ls-files", "-s", "-z"]);
    let index_records = parse_index_records(
        &require_shadow_egress_success(
            "ls-files",
            ls_files.output().map_err(|error| {
                shadow_egress_error(format!("starting file sync git ls-files: {error}"))
            })?,
        )?
        .stdout,
    )?;

    let staging_dir = state_dir.join("staging");
    if staging_dir.exists() {
        return Err(shadow_egress_error(format!(
            "refusing to overwrite file sync staging directory {}",
            staging_dir.display()
        )));
    }
    std::fs::create_dir(&staging_dir).map_err(|error| {
        shadow_egress_error(format!(
            "creating file sync staging directory {}: {error}",
            staging_dir.display()
        ))
    })?;

    let mut unsupported_paths: HashSet<PathBuf> =
        unsupported.iter().map(|issue| issue.path.clone()).collect();
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    let mut candidates = Vec::new();
    let mut additions = Vec::new();
    let mut modifications = Vec::new();
    let mut deletions = Vec::new();

    for raw in raw_changes {
        let raw_path = raw.path.clone();
        let path = path_from_git_bytes(raw.path);
        if let Err(reason) = validate_relative_path(&path) {
            if unsupported_paths.insert(path.clone()) {
                unsupported.push(ShadowIssue { path, reason });
            }
            continue;
        }
        if unsupported_paths.contains(&path) {
            continue;
        }
        let baseline_state = baseline_records.get(&raw_path);
        let capsule_state = index_records.get(&raw_path);
        let kind = match raw.status {
            b'A' => ShadowChangeKind::Added,
            b'M' => ShadowChangeKind::Modified,
            b'D' => ShadowChangeKind::Deleted,
            b'T' => {
                unsupported_paths.insert(path.clone());
                unsupported.push(ShadowIssue {
                    path,
                    reason: "file type changes are not applied".to_string(),
                });
                continue;
            }
            _ => unreachable!("parse_name_status_z validates statuses"),
        };

        if kind == ShadowChangeKind::Deleted {
            let Some(baseline_state) = baseline_state else {
                unsupported.push(ShadowIssue {
                    path,
                    reason: "deleted path is missing from the baseline".to_string(),
                });
                continue;
            };
            if !is_regular_git_mode(baseline_state.mode) {
                unsupported.push(ShadowIssue {
                    path,
                    reason: "deleted non-regular entry is not applied".to_string(),
                });
                continue;
            }
            changes.push(ShadowFileChange {
                kind,
                path: path.clone(),
            });
            deletions.push(serde_json::json!({
                "path": escape_shadow_path(&path),
                "baseline_mode": format!("{:06o}", baseline_state.mode),
                "baseline_hash": baseline_state.blob.to_string(),
                "capsule_mode": serde_json::Value::Null,
                "capsule_hash": serde_json::Value::Null,
            }));
            continue;
        }

        let Some(capsule_state) = capsule_state else {
            unsupported.push(ShadowIssue {
                path,
                reason: "changed path is missing from the file sync index".to_string(),
            });
            continue;
        };
        if !is_regular_git_mode(capsule_state.mode) {
            unsupported.push(ShadowIssue {
                path,
                reason: if capsule_state.mode == 0o120000 {
                    "symbolic links are not applied".to_string()
                } else {
                    format!("git mode {:06o} is not a regular file", capsule_state.mode)
                },
            });
            continue;
        }

        let source_mode = stage_regular_file(export, &staging_dir, &path)?;
        let (staged_file, _) = open_regular_from_root(&staging_dir, &path)?;
        let staged_blob = hash_open_file(git_dir, &staging_dir, staged_file)?;
        if staged_blob != capsule_state.blob {
            return Err(shadow_egress_error(format!(
                "staged content changed while capturing {}",
                escape_shadow_path(&path)
            )));
        }
        let host_state = inspect_host_file(host_root, &path, git_dir)?;
        let baseline_blob = baseline_state.map(|state| state.blob);
        let baseline_executable = baseline_state.map(|state| git_mode_is_executable(state.mode));
        let conflict = classify_conflict(kind, baseline_blob, baseline_executable, &host_state);
        if let Some(reason) = &conflict {
            conflicts.push(ShadowIssue {
                path: path.clone(),
                reason: reason.clone(),
            });
        } else {
            candidates.push(ShadowApplyCandidate {
                path: path.clone(),
                kind,
                capsule_executable: git_mode_is_executable(capsule_state.mode),
                capsule_blob: capsule_state.blob,
                baseline_blob,
                baseline_executable,
            });
        }
        changes.push(ShadowFileChange {
            kind,
            path: path.clone(),
        });
        let (host_hash, host_mode, host_type) = match host_state {
            HostFileState::Missing => (None, None, "missing"),
            HostFileState::Regular { blob, mode } => (
                Some(blob.to_string()),
                Some(format!("{mode:06o}")),
                "regular",
            ),
            HostFileState::NonFile(_) => (None, None, "non-regular"),
        };
        let record = serde_json::json!({
            "path": escape_shadow_path(&path),
            "baseline_mode": baseline_state.map(|state| format!("{:06o}", state.mode)),
            "baseline_hash": baseline_blob.map(|blob| blob.to_string()),
            "capsule_mode": format!("{:06o}", capsule_state.mode),
            "capsule_source_mode": format!("{source_mode:06o}"),
            "capsule_hash": capsule_state.blob.to_string(),
            "host_type": host_type,
            "host_mode": host_mode,
            "host_hash": host_hash,
            "conflict": conflict,
        });
        match kind {
            ShadowChangeKind::Added => additions.push(record),
            ShadowChangeKind::Modified => modifications.push(record),
            ShadowChangeKind::Deleted => unreachable!(),
        }
    }

    unsupported.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest_path = write_shadow_manifest(
        state_dir,
        baseline,
        additions,
        modifications,
        deletions,
        &conflicts,
        &unsupported,
    )?;

    let mut diff = shadow_git_command(git_dir, export);
    diff.env("GIT_INDEX_FILE", &index_path)
        .args([
            "diff",
            "--cached",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
        ])
        .arg(baseline.to_string())
        .arg("--");
    let diff_output = require_shadow_egress_success(
        "diff",
        diff.output().map_err(|error| {
            shadow_egress_error(format!("starting file sync git diff: {error}"))
        })?,
    )?;
    let diff_truncated = diff_output.stdout.len() > DIFF_OUTPUT_LIMIT;
    let shown_diff = &diff_output.stdout[..diff_output.stdout.len().min(DIFF_OUTPUT_LIMIT)];

    Ok(ShadowHandoff {
        state_dir: state_dir.to_path_buf(),
        staging_dir,
        backup_dir: state_dir.join("backup"),
        manifest_path,
        changes,
        conflicts,
        unsupported,
        diff: String::from_utf8_lossy(shown_diff).into_owned(),
        diff_truncated,
        git_dir: git_dir.to_path_buf(),
        candidates,
    })
}

pub async fn shadow_egress(
    capsule: &ContainerId,
    host_root: &Path,
    git_dir: &Path,
    baseline: Oid,
    state_dir: &Path,
    verbose: bool,
) -> Result<ShadowHandoff, GitError> {
    let export = state_dir.join("export");
    let export = if export.exists() {
        export
    } else {
        capture_shadow_export(capsule, state_dir, verbose).await?
    };
    prepare_shadow_handoff(&export, host_root, git_dir, baseline, state_dir)
}

fn back_up_host_file(
    host_root: &Path,
    backup_root: &Path,
    relative: &Path,
) -> Result<(), GitError> {
    let (mut source, metadata) = open_regular_from_root(host_root, relative)?;
    let destination = backup_root.join(relative);
    let parent = destination
        .parent()
        .ok_or_else(|| shadow_egress_error("backup destination has no parent"))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        shadow_egress_error(format!(
            "creating backup directory for {}: {error}",
            escape_shadow_path(relative)
        ))
    })?;
    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .map_err(|error| {
            shadow_egress_error(format!(
                "creating backup for {}: {error}",
                escape_shadow_path(relative)
            ))
        })?;
    std::io::copy(&mut source, &mut backup).map_err(|error| {
        shadow_egress_error(format!(
            "backing up {}: {error}",
            escape_shadow_path(relative)
        ))
    })?;
    #[cfg(unix)]
    std::fs::set_permissions(
        &destination,
        std::fs::Permissions::from_mode(metadata_mode(&metadata)),
    )
    .map_err(|error| shadow_egress_error(format!("preserving backup permissions: {error}")))?;
    backup
        .sync_all()
        .map_err(|error| shadow_egress_error(format!("syncing backup file: {error}")))?;
    Ok(())
}

fn apply_shadow_candidate(
    handoff: &ShadowHandoff,
    host_root: &Path,
    candidate: &ShadowApplyCandidate,
) -> Result<(), GitError> {
    let (staged_file, _) = open_regular_from_root(&handoff.staging_dir, &candidate.path)?;
    let staged_blob = hash_open_file(&handoff.git_dir, &handoff.staging_dir, staged_file)?;
    if staged_blob != candidate.capsule_blob {
        return Err(shadow_egress_error(format!(
            "staged file changed before apply: {}",
            escape_shadow_path(&candidate.path)
        )));
    }

    let destination = host_root.join(&candidate.path);
    let parent = destination
        .parent()
        .ok_or_else(|| shadow_egress_error("apply destination has no parent"))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        shadow_egress_error(format!(
            "creating apply directory for {}: {error}",
            escape_shadow_path(&candidate.path)
        ))
    })?;
    let current = inspect_host_file(host_root, &candidate.path, &handoff.git_dir)?;
    if let Some(reason) = classify_conflict(
        candidate.kind,
        candidate.baseline_blob,
        candidate.baseline_executable,
        &current,
    ) {
        return Err(shadow_egress_error(format!(
            "host changed before apply for {}: {reason}",
            escape_shadow_path(&candidate.path)
        )));
    }

    if matches!(&current, HostFileState::Regular { .. }) {
        back_up_host_file(host_root, &handoff.backup_dir, &candidate.path)?;
    }
    let (mut source, _) = open_regular_from_root(&handoff.staging_dir, &candidate.path)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        shadow_egress_error(format!(
            "creating atomic apply file for {}: {error}",
            escape_shadow_path(&candidate.path)
        ))
    })?;
    std::io::copy(&mut source, temporary.as_file_mut())
        .map_err(|error| shadow_egress_error(format!("preparing atomic apply file: {error}")))?;
    #[cfg(unix)]
    {
        let mode = match current {
            HostFileState::Regular { mode, .. } => mode,
            _ if candidate.capsule_executable => 0o755,
            _ => 0o644,
        };
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(mode)).map_err(
            |error| shadow_egress_error(format!("setting apply file permissions: {error}")),
        )?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| shadow_egress_error(format!("syncing apply file: {error}")))?;
    temporary.persist(&destination).map_err(|error| {
        shadow_egress_error(format!(
            "atomically replacing {}: {}",
            escape_shadow_path(&candidate.path),
            error.error
        ))
    })?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| shadow_egress_error(format!("syncing apply directory: {error}")))?;
    Ok(())
}

fn case_fold_collision_paths(paths: &[PathBuf]) -> HashSet<PathBuf> {
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut collisions = HashSet::new();
    for path in paths {
        let Some(text) = path.to_str() else {
            continue;
        };
        let folded = text
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<String>();
        if let Some(previous) = seen.get(&folded) {
            collisions.insert(previous.clone());
            collisions.insert(path.clone());
        } else {
            seen.insert(folded, path.clone());
        }
    }
    collisions
}

pub fn apply_shadow_handoff(
    handoff: &ShadowHandoff,
    host_root: &Path,
    allow_implicit_runners: bool,
) -> Result<ShadowApplyReport, GitError> {
    let mut report = ShadowApplyReport {
        applied: Vec::new(),
        refused: Vec::new(),
        skipped_implicit_runners: Vec::new(),
        failed: Vec::new(),
    };

    // Classify BEFORE path validation so refused entries never reach the apply
    // walk at all, and so a `.git/...` candidate cannot be validated into scope.
    let mut eligible = Vec::new();
    for candidate in &handoff.candidates {
        match classify_import_path(&candidate.path) {
            ImportClass::Refused => report.refused.push(candidate.path.clone()),
            ImportClass::ImplicitRunner if !allow_implicit_runners => {
                report.skipped_implicit_runners.push(candidate.path.clone())
            }
            _ => eligible.push(candidate),
        }
    }

    let eligible_paths = eligible
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<Vec<_>>();
    let collision_paths = case_fold_collision_paths(&eligible_paths);
    let mut admitted = Vec::new();
    for candidate in eligible {
        if collision_paths.contains(&candidate.path) {
            report.failed.push(ShadowIssue {
                path: candidate.path.clone(),
                reason: "case-insensitive path collides with another changed file".into(),
            });
        } else {
            admitted.push(candidate);
        }
    }

    let paths = admitted
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect::<Vec<_>>();
    validate_path_set(host_root, &paths)?;

    for candidate in admitted {
        match apply_shadow_candidate(handoff, host_root, candidate) {
            Ok(()) => report.applied.push(candidate.path.clone()),
            // One path whose host content moved between classification and write
            // says nothing about the others: record it and keep going, so a
            // single conflict cannot strand every remaining clean file.
            Err(error) => report.failed.push(ShadowIssue {
                path: candidate.path.clone(),
                reason: error.to_string(),
            }),
        }
    }
    Ok(report)
}

struct TarChildGuard(std::process::Child);

impl TarChildGuard {
    fn wait(mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.wait()
    }
}

impl Drop for TarChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct SeederCleanup {
    id: String,
    armed: bool,
}

impl SeederCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SeederCleanup {
    fn drop(&mut self) {
        if self.armed {
            let mut command = Command::new("docker");
            configure_docker_command(&mut command);
            let _ = command
                .args(["rm", "-f", &self.id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn seed_with_tar(container_id: &str, source: &Path) -> Result<(), GitError> {
    // Host tar (portable, no --mtime on BSD tar) -> docker exec tar extract.
    // Mtime normalization is performed inside the container after extract.
    let mut tar_out = {
        let mut c = std::process::Command::new("tar");
        c.arg("-c")
            .arg("-f")
            .arg("-")
            .arg("-C")
            .arg(source)
            .arg(".")
            // BSD tar otherwise serializes macOS extended attributes as `._*`
            // AppleDouble files, which Git then mistakes for capsule changes.
            .env("COPYFILE_DISABLE", "1");
        c.stdout(Stdio::piped());
        TarChildGuard(
            c.spawn()
                .map_err(|error| GitError::Ingress(format!("starting host tar: {error}")))?,
        )
    };
    let tar_stdout =
        tar_out.0.stdout.take().ok_or_else(|| {
            GitError::Ingress("host tar did not provide its output stream".into())
        })?;

    let mut docker = Command::new("docker");
    configure_docker_command(&mut docker);
    docker
        .arg("exec")
        .arg("-i")
        .arg(container_id)
        .arg("tar")
        .arg("-x")
        .arg("-C")
        .arg("/workspace")
        .arg("--no-same-owner")
        .arg("-f")
        .arg("-");
    docker.stdin(Stdio::from(tar_stdout));
    docker.stdout(Stdio::piped());
    docker.stderr(Stdio::piped());

    let docker_result = docker.output();
    let tar_status = tar_out
        .wait()
        .map_err(|error| GitError::Ingress(format!("waiting for host tar: {error}")))?;
    let out =
        docker_result.map_err(|error| GitError::Ingress(format!("docker tar pipe: {error}")))?;

    if !tar_status.success() {
        return Err(GitError::Ingress(format!(
            "host tar failed with status {tar_status}"
        )));
    }

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(GitError::Ingress(format!(
            "extract into volume failed: {}",
            err
        )));
    }
    Ok(())
}

async fn seed_volume(
    rt: &dyn RuntimeAdapter,
    vol: &VolumeId,
    source: &Path,
    is_git: bool,
    seeder_image: &capscode_core::ImageRef,
    log: &mut PhaseLog,
) -> Result<(), GitError> {
    // One-shot seeder container to receive the tree via tar.
    let spec = ContainerSpec {
        image: seeder_image.clone(),
        hostname: "seeder".to_string(),
        env: vec![],
        cmd: Some(vec!["sleep".to_string(), "infinity".to_string()]),
        entrypoint: None,
        mounts: vec![Mount::Volume {
            id: vol.clone(),
            target: PathBuf::from("/workspace"),
            read_only: false,
        }],
        network: NetworkMode::None,
        published_ports: vec![],
        cap_add: vec![],
        cap_drop: Vec::new(),
        no_new_privileges: false,
        devices: vec![],
        sysctls: vec![],
        labels: vec![
            (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
            (LABEL_SESSION.to_string(), "ingress-seeder".to_string()),
            (LABEL_ROLE.to_string(), "seeder".to_string()),
            (LABEL_CREATED.to_string(), time_label()),
        ],
        tmpfs_home: false,
        read_only_rootfs: false,
        init: false,
        pids_limit: capscode_core::DEFAULT_PIDS_LIMIT,
        memory_bytes: None,
    };

    let seeder = timed!(
        log,
        "seeder-create",
        rt.create_container(&spec)
            .await
            .map_err(|error| GitError::Ingress(format!("creating seeder container: {error}")))?
    );
    let mut cleanup = SeederCleanup {
        id: seeder.0.clone(),
        armed: true,
    };

    timed!(
        log,
        "seeder-start",
        rt.start(&seeder)
            .await
            .map_err(|error| GitError::Ingress(format!("starting seeder container: {error}")))?
    );

    // Stream the tree in.
    timed!(log, "tar-upload", seed_with_tar(&seeder.0, source).await?);

    let assertion = if is_git {
        timed!(
            log,
            "assert-clean-repo",
            assert_seeded_repo_clean(rt, &seeder).await
        )
    } else {
        Ok(())
    };

    // Always normalize mtimes inside the volume (epoch) after extract for both git and rsync paths.
    timed!(
        log,
        "mtime-normalize",
        let _ = rt
            .exec(
                &seeder,
                ExecSpec {
                    cmd: vec![
                        "sh".into(),
                        "-c".into(),
                        "find /workspace -exec touch -d @0 {} + || true".into(),
                    ],
                    ..Default::default()
                },
            )
            .await;
    );

    // Force-remove the running seeder; volume content persists.
    let remove_result = timed!(log, "seeder-remove", rt.remove(&seeder, true).await);
    remove_result
        .map_err(|error| GitError::Ingress(format!("removing seeder container: {error}")))?;
    cleanup.disarm();

    assertion?;
    Ok(())
}

async fn assert_seeded_repo_clean(
    rt: &dyn RuntimeAdapter,
    seeder: &ContainerId,
) -> Result<(), GitError> {
    let output = rt
        .exec(
            seeder,
            ExecSpec {
                cmd: vec!["cat".into(), "/workspace/.git/config".into()],
                ..Default::default()
            },
        )
        .await
        .map_err(|error| {
            GitError::Ingress(format!(
                "assert clean repo: reading /workspace/.git/config failed: {error}"
            ))
        })?;
    if output.exit_code != 0 {
        return Err(GitError::Ingress(format!(
            "assert clean repo: /workspace/.git/config is missing or unreadable (exit {}): {}",
            output.exit_code,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let config = String::from_utf8(output.stdout).map_err(|error| {
        GitError::Ingress(format!(
            "assert clean repo: /workspace/.git/config is not UTF-8: {error}"
        ))
    })?;
    assert_clean_repo_config(&config)
        .map_err(|reason| GitError::Ingress(format!("assert clean repo: {reason}")))
}

fn assert_clean_repo_config(config: &str) -> Result<(), String> {
    let mut section = String::new();
    let mut saw_content = false;

    for (index, line) in config.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        saw_content = true;

        if trimmed.starts_with('[') {
            let closing = trimmed.find(']').ok_or_else(|| {
                format!("malformed section header on line {line_number} of .git/config")
            })?;
            let trailing = trimmed[closing + 1..].trim();
            if !trailing.is_empty() && !trailing.starts_with('#') && !trailing.starts_with(';') {
                return Err(format!(
                    "malformed section header on line {line_number} of .git/config"
                ));
            }
            let header = trimmed[1..closing].trim();
            let section_name = header
                .split(|character: char| character.is_ascii_whitespace() || character == '.')
                .next()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    format!("empty section header on line {line_number} of .git/config")
                })?;
            section = section_name.to_ascii_lowercase();
            if section == "remote" {
                return Err(format!(
                    "repo-local remote section found on line {line_number} of .git/config"
                ));
            }
            // An include pulls keys from a file this scan never reads, so a remote
            // or credential helper could be smuggled past every check below.
            if section == "include" || section == "includeif" {
                return Err(format!(
                    "repo-local {section} directive found on line {line_number} of .git/config"
                ));
            }
            continue;
        }

        let Some((key, _)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if section == "credential" && key.eq_ignore_ascii_case("helper") {
            return Err(format!(
                "repo-local credential.helper found on line {line_number} of .git/config"
            ));
        }
        if section == "user" && key.eq_ignore_ascii_case("name") {
            return Err(format!(
                "repo-local user.name found on line {line_number} of .git/config"
            ));
        }
        if section == "user" && key.eq_ignore_ascii_case("email") {
            return Err(format!(
                "repo-local user.email found on line {line_number} of .git/config"
            ));
        }
    }

    if !saw_content {
        return Err(".git/config is missing or empty".to_string());
    }
    Ok(())
}

fn compute_egress_report(
    host: &Path,
    session: &capscode_core::SessionId,
    baseline: Option<Oid>,
) -> Result<EgressReport, GitError> {
    let repo = Repository::open(host).map_err(|_| GitError::NotARepo(host.to_path_buf()))?;
    let prefix = session_ref_prefix(session);
    let head_name = format!("{}/head", prefix);

    let head_ref = repo
        .find_reference(&head_name)
        .map_err(|_| GitError::Egress(format!("missing {} after egress", head_name)))?;
    let head_oid = head_ref
        .target()
        .ok_or_else(|| GitError::Egress("HEAD is symbolic".into()))?;

    // Prefer the exact ingress snapshot. The first-parent fallback exists for
    // older callers (notably `sync`) that do not yet retain session baselines.
    let base = baseline.unwrap_or_else(|| {
        repo.find_commit(head_oid)
            .ok()
            .and_then(|commit| commit.parents().next().map(|parent| parent.id()))
            .unwrap_or(head_oid)
    });
    let n_commits = if base == head_oid {
        0
    } else {
        count_commits_ahead(&repo, base, head_oid).unwrap_or(0)
    };

    let (files, ins, dels) = if base != head_oid {
        diff_stat(&repo, base, head_oid)?
    } else {
        (0, 0, 0)
    };

    Ok(EgressReport {
        base_sha: base,
        commits: n_commits,
        files_changed: files,
        insertions: ins,
        deletions: dels,
    })
}

fn count_commits_ahead(repo: &Repository, base: Oid, head: Oid) -> Result<u32, git2::Error> {
    let mut rw = repo.revwalk()?;
    rw.push(head)?;
    rw.hide(base)?;
    let mut c = 0u32;
    for _ in rw {
        c += 1;
    }
    Ok(c)
}

fn diff_stat(repo: &Repository, a: Oid, b: Oid) -> Result<(u32, u32, u32), GitError> {
    let ta = repo.find_commit(a)?.tree()?;
    let tb = repo.find_commit(b)?.tree()?;
    let d = repo.diff_tree_to_tree(Some(&ta), Some(&tb), None)?;
    let s = d.stats()?;
    Ok((
        s.files_changed() as u32,
        s.insertions() as u32,
        s.deletions() as u32,
    ))
}

fn capsule_patch(host_repo: &Path, baseline: Oid, head: Oid) -> Result<Vec<u8>, GitError> {
    let output = std::process::Command::new("git")
        .current_dir(host_repo)
        .args([
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            &baseline.to_string(),
            &head.to_string(),
            "--",
        ])
        .output()
        .map_err(|e| GitError::Egress(format!("spawn git diff: {e}")))?;
    if !output.status.success() {
        return Err(GitError::Egress(format!(
            "could not prepare capsule changes: {}",
            escape_command_output(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn apply_patch_to_worktree(host_repo: &Path, patch: &[u8], check: bool) -> Result<(), GitError> {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(host_repo)
        .args(["apply", "--binary", "--whitespace=nowarn"]);
    if check {
        command.arg("--check");
    }
    command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| GitError::Egress(format!("spawn git apply: {e}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| GitError::Egress("git apply stdin unavailable".into()))?
        .write_all(patch)
        .map_err(|e| GitError::Egress(format!("write git apply patch: {e}")))?;
    let output = child
        .wait_with_output()
        .map_err(|e| GitError::Egress(format!("wait for git apply: {e}")))?;
    if !output.status.success() {
        return Err(GitError::Egress(format!(
            "capsule changes do not apply cleanly: {}",
            escape_command_output(&output.stderr)
        )));
    }
    Ok(())
}

fn configure_insulated_egress_fetch(command: &mut TokioCommand, protocol_config: &str) {
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg(protocol_config)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");

    const REMOVE: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
    ];
    for name in REMOVE {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let name_text = name.to_string_lossy();
        if name_text.starts_with("GIT_CONFIG_KEY_") || name_text.starts_with("GIT_CONFIG_VALUE_") {
            command.env_remove(name);
        }
    }
}

pub fn validate_exported_git_repository(repo_root: &Path) -> Result<(), GitError> {
    let git_dir = repo_root.join(".git");
    let metadata = std::fs::symlink_metadata(&git_dir)
        .map_err(|_| GitError::Egress("capsule export does not contain a .git directory".into()))?;
    if metadata.file_type().is_symlink() {
        return Err(GitError::Egress(
            "capsule export contains a symlinked .git entry".into(),
        ));
    }
    if !metadata.is_dir() {
        return Err(GitError::Egress(
            "capsule export contains a .git pointer file instead of a repository".into(),
        ));
    }

    let common_dir = git_dir.join("commondir");
    match std::fs::symlink_metadata(&common_dir) {
        Ok(_) => {
            return Err(GitError::Egress(
                "capsule repository uses a separate common directory".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(GitError::Egress(
                "could not inspect capsule repository common directory".into(),
            ));
        }
    }

    let config_path = git_dir.join("config");
    match std::fs::symlink_metadata(&config_path) {
        Ok(metadata) if metadata.is_file() => {
            let config = git2::Config::open(&config_path).map_err(|_| {
                GitError::Egress("could not inspect copied capsule Git configuration".into())
            })?;
            if config.get_entry("core.worktree").is_ok() {
                return Err(GitError::Egress(
                    "capsule repository redirects its worktree outside the export".into(),
                ));
            }
        }
        Ok(_) => {
            return Err(GitError::Egress(
                "capsule Git configuration is not a regular file".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(GitError::Egress(
                "could not inspect copied capsule Git configuration".into(),
            ));
        }
    }

    let alternates = git_dir.join("objects/info/alternates");
    match std::fs::symlink_metadata(&alternates) {
        Ok(_) => {
            return Err(GitError::Egress(
                "capsule repository uses objects/info/alternates".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(GitError::Egress(
                "could not inspect capsule repository alternates".into(),
            ));
        }
    }

    let mut pending = vec![git_dir];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|_| {
            GitError::Egress("could not inspect the copied capsule Git repository".into())
        })?;
        for entry in entries {
            let entry = entry.map_err(|_| {
                GitError::Egress("could not inspect the copied capsule Git repository".into())
            })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
                GitError::Egress("could not inspect the copied capsule Git repository".into())
            })?;
            let relative = path.strip_prefix(repo_root).unwrap_or(Path::new(".git"));
            if metadata.file_type().is_symlink() {
                return Err(GitError::Egress(format!(
                    "capsule Git data contains a symbolic link: {}",
                    escape_shadow_path(relative)
                )));
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.is_file() {
                return Err(GitError::Egress(format!(
                    "capsule Git data contains a non-regular entry: {}",
                    escape_shadow_path(relative)
                )));
            }
        }
    }
    Ok(())
}

/// Return the host-side commit that represents a fetched capsule commit.
/// Redacted sessions are rebuilt on the real host baseline before publication.
pub fn publication_target(
    host_repo: &Path,
    session: &capscode_core::SessionId,
    source_head: Oid,
) -> Result<Oid, GitError> {
    let repo =
        Repository::open(host_repo).map_err(|_| GitError::NotARepo(host_repo.to_path_buf()))?;
    if let Some(mapping) = read_history_mapping(host_repo, session)? {
        recreate_redacted_history(&repo, source_head, mapping)
    } else if has_synthetic_history_root(&repo, source_head)? {
        Err(GitError::Egress(
            "local history metadata for this capsule is missing; refusing to create a disconnected branch".into(),
        ))
    } else {
        Ok(source_head)
    }
}

async fn egress_copy_fallback(plan: &EgressPlan) -> Result<EgressReport, GitError> {
    // docker cp <capsule>:<ws>/.git <tmp>/.git  then fetch from the dir containing .git
    let td = tempfile::tempdir().map_err(|e| GitError::Egress(e.to_string()))?;
    let dest = td.path().join("repo");
    std::fs::create_dir_all(&dest).map_err(|error| {
        GitError::Egress(format!(
            "creating temporary repository {}: {error}",
            dest.display()
        ))
    })?;

    // Copy the .git directory tree out.
    let cp_src = format!("{}:{}/.git", plan.capsule.0, plan.workspace_path.display());
    let mut copy = TokioCommand::new("docker");
    configure_tokio_docker_command(&mut copy);
    copy.args(["cp", &cp_src, dest.to_string_lossy().as_ref()]);
    let output = copy
        .output()
        .await
        .map_err(|e| GitError::Egress(e.to_string()))?;
    if plan.verbose && !output.stdout.is_empty() {
        eprint!("{}", escape_command_output(&output.stdout));
    }
    if !output.status.success() {
        return Err(GitError::Egress(format!(
            "docker cp .git fallback failed: {}",
            escape_command_output(&output.stderr)
        )));
    }

    // Fetching a local repository makes Git read its object metadata on the
    // host, so reject indirections before starting Git.
    validate_exported_git_repository(&dest)?;

    // dest now has a checked .git directory inside it. Treat dest as the repo root.
    let url = dest.to_string_lossy().to_string();
    let refspecs = egress_refspecs(&plan.session);

    let mut fetch = TokioCommand::new("git");
    configure_insulated_egress_fetch(&mut fetch, "protocol.file.allow=always");
    let fetch_out = fetch
        .args([
            "-C",
            plan.host_repo.to_string_lossy().as_ref(),
            "fetch",
            &url,
        ])
        .args(&refspecs)
        .output()
        .await
        .map_err(|e| GitError::Egress(e.to_string()))?;
    if !fetch_out.status.success() {
        return Err(GitError::Egress(format!(
            "fallback fetch failed: {}",
            escape_command_output(&fetch_out.stderr)
        )));
    }

    verify_fetched_baseline(plan)?;
    compute_egress_report(&plan.host_repo, &plan.session, plan.baseline)
}

#[async_trait]
impl GitEngine for DefaultGitEngine {
    async fn ingress(
        &self,
        plan: &IngressPlan,
        rt: &dyn RuntimeAdapter,
        on: &mut (dyn FnMut(IngressProgress) + Send),
    ) -> Result<IngressReport, GitError> {
        let t0 = Instant::now();
        let mut log = PhaseLog::default();
        on(IngressProgress {
            phase: "start".into(),
            bytes_copied: 0,
            total_bytes: None,
        });

        let is_git = Repository::open(&plan.host_repo).is_ok();
        let prepare_started = Instant::now();

        let (prep_dir, wip_from_prep, receipt, history_mapping) = if is_git {
            on(IngressProgress {
                phase: "prepare-git".into(),
                bytes_copied: 0,
                total_bytes: None,
            });
            let host = plan.host_repo.clone();
            let h = plan.history;
            let inc = plan.include.clone();
            let verbose = plan.verbose;
            let res = task::spawn_blocking(move || prepare_git_source(&host, h, &inc, verbose))
                .await
                .map_err(|e| GitError::Ingress(e.to_string()))??;
            let receipt = IngressReceipt::GitSnapshot {
                baseline: Some(res.snapshot_head.to_string()),
            };
            (res.directory, res.wip_oid, receipt, res.history_mapping)
        } else {
            on(IngressProgress {
                phase: "prepare-rsync".into(),
                bytes_copied: 0,
                total_bytes: None,
            });
            let host = plan.host_repo.clone();
            let state_dir = plan.state_dir.clone();
            let (prep_dir, git_dir, baseline) = task::spawn_blocking(move || {
                check_shadow_capacity(&host, &state_dir)?;
                let prep_dir = prepare_rsync_source(&host)?;
                let (git_dir, baseline) = build_shadow_repo(prep_dir.path(), &state_dir)?;
                Ok::<_, GitError>((prep_dir, git_dir, baseline))
            })
            .await
            .map_err(|e| GitError::Ingress(e.to_string()))??;
            (
                prep_dir,
                None,
                IngressReceipt::Shadow {
                    git_dir,
                    baseline: baseline.to_string(),
                    state_dir: plan.state_dir.clone(),
                },
                None,
            )
        };

        let prep_path = prep_dir.path().to_path_buf();

        log.record(
            if is_git {
                "prepare-git"
            } else {
                "prepare-rsync"
            },
            prepare_started.elapsed(),
        );

        timed!(
            log,
            "persist-receipt",
            persist_receipt(&plan.state_dir, &receipt)?
        );

        // Size before we consume it.
        let stats = timed!(log, "measure-tree", tree_stats(&prep_path));
        let copied = stats.bytes;

        on(IngressProgress {
            phase: "create-volume".into(),
            bytes_copied: 0,
            total_bytes: Some(copied),
        });

        let vol_name = workspace_volume_name(&plan.session);
        let vol = timed!(
            log,
            "create-volume",
            rt.create_volume(&VolumeSpec {
                name: vol_name,
                labels: vec![
                    (LABEL_OWNER.to_string(), LABEL_OWNER_VALUE.to_string()),
                    (LABEL_SESSION.to_string(), plan.session.as_str().to_string()),
                    (LABEL_ROLE.to_string(), ROLE_CAPSULE.to_string()),
                    (LABEL_CREATED.to_string(), time_label()),
                ],
            })
            .await
            .map_err(|error| GitError::Ingress(format!("create volume: {error}")))?
        );

        on(IngressProgress {
            phase: "seed".into(),
            bytes_copied: copied,
            total_bytes: Some(copied),
        });

        seed_volume(
            rt,
            &vol,
            &prep_path,
            matches!(&receipt, IngressReceipt::GitSnapshot { .. }),
            &plan.seeder_image,
            &mut log,
        )
        .await?;

        if let Some(mapping) = history_mapping {
            persist_history_mapping(&plan.host_repo, &plan.session, mapping)?;
        }

        timed!(log, "cleanup-temp", drop(prep_dir));

        // For git mode, the wip sha we computed on host is the one present in volume
        // (tree-identical commit creation).
        let final_wip = if matches!(&receipt, IngressReceipt::GitSnapshot { .. }) {
            wip_from_prep
        } else {
            None
        };

        let elapsed = t0.elapsed();
        on(IngressProgress {
            phase: "done".into(),
            bytes_copied: copied,
            total_bytes: Some(copied),
        });

        Ok(IngressReport {
            copied_bytes: copied,
            copied_files: stats.files,
            elapsed,
            wip_commit: final_wip,
            receipt,
            phases: log.phases,
        })
    }

    async fn egress(
        &self,
        plan: &EgressPlan,
        rt: &dyn RuntimeAdapter,
    ) -> Result<EgressReport, GitError> {
        // The `ext::`-over-exec transport can deadlock on the runtime's bidirectional
        // exec stdio, so the reliable `docker cp` copy transport is the default. Opt
        // into the negotiated ext:: path (only-missing-objects) with CAPSCODE_EGRESS=ext.
        let want_ext = std::env::var("CAPSCODE_EGRESS")
            .map(|v| v == "ext")
            .unwrap_or(false);
        if !want_ext {
            return egress_copy_fallback(plan).await;
        }

        let url = format!(
            "ext::{}",
            ext_upload_pack_cmd(&plan.capsule, &plan.workspace_path, rt.kind())
        );
        let refspecs = egress_refspecs(&plan.session);

        let host_s = plan.host_repo.to_string_lossy().to_string();

        let mut fetch = TokioCommand::new("git");
        configure_insulated_egress_fetch(&mut fetch, "protocol.ext.allow=always");
        if let Some(host) = docker_host_override() {
            fetch.env("DOCKER_HOST", host);
        }
        let fetch_res = fetch
            .args(["-C", &host_s, "fetch", &url])
            .args(&refspecs)
            .output()
            .await
            .map_err(|error| GitError::Egress(format!("spawn git fetch: {error}")))?;

        if !fetch_res.status.success() {
            // Fallback path (CAPSCODE_EGRESS=copy or transport failure).
            return egress_copy_fallback(plan).await;
        }

        verify_fetched_baseline(plan)?;
        compute_egress_report(&plan.host_repo, &plan.session, plan.baseline)
    }

    fn apply_as_branch(
        &self,
        host_repo: &Path,
        session: &capscode_core::SessionId,
        name: &str,
    ) -> Result<String, GitError> {
        let repo =
            Repository::open(host_repo).map_err(|_| GitError::NotARepo(host_repo.to_path_buf()))?;
        let source_name = format!("{}/head", session_ref_prefix(session));
        let source_ref = repo
            .find_reference(&source_name)
            .map_err(|_| GitError::Egress(format!("no fetched ref {}", source_name)))?;
        let source_head = source_ref
            .target()
            .ok_or_else(|| GitError::Egress("source ref is symbolic".into()))?;
        let target = if let Some(mapping) = read_history_mapping(host_repo, session)? {
            recreate_redacted_history(&repo, source_head, mapping)?
        } else if has_synthetic_history_root(&repo, source_head)? {
            return Err(GitError::Egress(
                "local history metadata for this capsule is missing; refusing to create a disconnected branch".into(),
            ));
        } else {
            source_head
        };

        let branch = format!("capscode/{}", name);
        let full = format!("refs/heads/{}", branch);
        if repo.find_reference(&full).is_ok() {
            return Err(GitError::RefConflict(full));
        }
        repo.reference(&full, target, false, "capscode apply_as_branch")
            .map_err(|error| {
                if error.code() == git2::ErrorCode::Exists {
                    GitError::RefConflict(full.clone())
                } else {
                    GitError::from(error)
                }
            })?;

        // Hard safety: we only created/updated a ref. No checkout, no worktree mutation.
        Ok(branch)
    }

    fn apply_to_worktree(
        &self,
        host_repo: &Path,
        session: &capscode_core::SessionId,
        baseline: Oid,
    ) -> Result<(), GitError> {
        let repo =
            Repository::open(host_repo).map_err(|_| GitError::NotARepo(host_repo.to_path_buf()))?;
        repo.find_commit(baseline)
            .map_err(|_| GitError::Egress(format!("missing capsule baseline commit {baseline}")))?;

        let source_name = format!("{}/head", session_ref_prefix(session));
        let head = repo
            .find_reference(&source_name)
            .map_err(|_| GitError::Egress(format!("no fetched ref {source_name}")))?
            .target()
            .ok_or_else(|| GitError::Egress("source ref is symbolic".into()))?;
        repo.find_commit(head)
            .map_err(|_| GitError::Egress(format!("capsule head {head} is not a commit")))?;

        let hooks_path = repo_hooks_path(&repo, host_repo)?;
        let mut held_paths = capsule_changed_paths(&repo, baseline, head)?
            .into_iter()
            .filter(|path| {
                classify_git_import_path(path, hooks_path.as_deref()) != ImportClass::Plain
            })
            .collect::<Vec<_>>();
        if !held_paths.is_empty() {
            held_paths.sort();
            held_paths.dedup();
            drop(repo);
            let branch = create_runner_files_branch(host_repo, session, head)?;
            return Err(GitError::RunnerFilesHeld {
                branch,
                paths: held_paths
                    .iter()
                    .map(|path| escape_shadow_path(path))
                    .collect(),
            });
        }
        drop(repo);

        let patch = capsule_patch(host_repo, baseline, head)?;
        apply_patch_to_worktree(host_repo, &patch, true)?;
        apply_patch_to_worktree(host_repo, &patch, false)
    }

    fn discard(
        &self,
        host_repo: &Path,
        session: &capscode_core::SessionId,
    ) -> Result<(), GitError> {
        let repo =
            Repository::open(host_repo).map_err(|_| GitError::NotARepo(host_repo.to_path_buf()))?;
        let prefix = session_ref_prefix(session);

        let mut victims: Vec<String> = Vec::new();
        {
            let refs = repo.references().map_err(GitError::from)?;
            for r in refs.flatten() {
                if let Ok(n) = r.name() {
                    if n.starts_with(&prefix) {
                        victims.push(n.to_string());
                    }
                }
            }
        }

        for v in victims {
            if let Ok(mut r) = repo.find_reference(&v) {
                let _ = r.delete();
            }
        }
        remove_history_mapping(host_repo, session)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Live-docker test adapter (gated; only for verification under CAPSCODE_TEST_DOCKER)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
