//! The non-git ("file sync") handoff, shared by every caller that performs one.
//!
//! A non-git workspace has no history to read, so file sync compares the
//! capsule's `/workspace` against a baseline captured when the session starts.
//! The CLI presents the changes for review before applying them to the host.
//!
//! The capture and apply primitives in `capscode-git` never prompt — the
//! questions were always the CLI's, not the handoff's. So this module owns the
//! whole sequence (quiesce, capture, present, apply) and the exit path, `sync`,
//! and `rm` all call it. One implementation, no drift.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};
use capscode_core::{ContainerId, RuntimeAdapter};
use capscode_git::{
    apply_shadow_handoff, escape_shadow_path, ShadowApplyReport, ShadowChangeKind, ShadowHandoff,
};
use dialoguer::Confirm;

use crate::ui::note;

/// What a handoff did, for the caller to report or act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShadowOutcome {
    /// The capture found no changes; nothing was staged and nothing applied.
    NoChanges,
    /// Every change that can come back was written to the host working tree.
    /// Capsule deletions may still be pending: they are reported, never applied.
    Applied,
    /// Some clean files were written, but other work is still only in the
    /// capsule and the staging directory: conflicts, failed or refused writes,
    /// skipped implicit-runner files, or unsupported entries.
    PartlyApplied,
    /// Changes were staged but NOT written — the user declined, the terminal was
    /// not interactive, or every change was a conflict. The staging directory
    /// holds the work.
    Staged,
}

impl ShadowOutcome {
    /// Whether the host now holds everything this round could bring back, so the
    /// box may be recorded as synced. `rm` and `uninstall` trust that record when
    /// deciding whether deleting the box would lose work.
    pub(crate) fn left_nothing_behind(self) -> bool {
        matches!(self, Self::NoChanges | Self::Applied)
    }
}

const WORKSPACE_LOCK: &str = "workspace.lock";

/// Exclusive hold on one session's workspace, released on drop.
///
/// File sync reads a stopped workspace, writes host files, and records what it wrote and
/// which workspace state it synced. Another command starting the box or syncing it in the
/// middle could fold edits nobody reviewed into that record. So `sync`, `rm`, `resume`, and
/// `uninstall` hold this lock for their whole run, and a one-shot session's own file sync
/// exit holds it from its capture onward.
#[derive(Debug)]
pub(crate) struct WorkspaceLock {
    _file: Option<std::fs::File>,
}

/// Take a session's workspace lock, refusing if another command holds it. A session with no
/// state directory has no workspace to sync or record, so it gets an empty guard rather than
/// a directory created for the lock.
pub(crate) fn lock_workspace(state_dir: &Path) -> Result<WorkspaceLock> {
    try_workspace_lock(state_dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "another capscode command is syncing, removing, or starting this box; try again when it finishes"
        )
    })
}

/// Take a session's workspace lock, waiting for another command to release it. A session's
/// own exit handoff uses this: refusing would abandon the capsule while a `sync` or `rm` of
/// it is still copying the workspace out. The wait polls, so a signal can still end it.
pub(crate) async fn lock_workspace_waiting(state_dir: &Path) -> Result<WorkspaceLock> {
    let mut announced = false;
    loop {
        if let Some(lock) = try_workspace_lock(state_dir)? {
            return Ok(lock);
        }
        if !announced {
            note("Waiting for another capscode command on this session to finish.");
            announced = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Take the workspace lock of every session under `sessions_dir`, for a command that removes
/// them all. Refuses if any is held.
pub(crate) fn lock_all_workspaces(sessions_dir: &Path) -> Result<Vec<WorkspaceLock>> {
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("listing {}", sessions_dir.display()))
        }
    };
    let mut locks = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", sessions_dir.display()))?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            locks.push(
                lock_workspace(&entry.path())
                    .with_context(|| format!("session {}", entry.file_name().to_string_lossy()))?,
            );
        }
    }
    Ok(locks)
}

/// Take the lock without waiting; `None` means another command holds it.
fn try_workspace_lock(state_dir: &Path) -> Result<Option<WorkspaceLock>> {
    if !state_dir.is_dir() {
        return Ok(Some(WorkspaceLock { _file: None }));
    }
    let path = state_dir.join(WORKSPACE_LOCK);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this function owns; it reads no memory.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("locking {}", path.display()));
        }
    }
    Ok(Some(WorkspaceLock { _file: Some(file) }))
}

/// Stop the capsule, then take a fresh export of its workspace.
///
/// The stop is the point: `docker cp` of a tree an agent is still writing to can
/// tear, and a torn capture that reads as empty would let teardown delete the
/// only copy of the work. The shared ten-second grace lets the keeper and tool
/// finish their shutdown before the runtime kills the capsule.
///
/// Note this leaves the capsule STOPPED. Callers that destroy the box next want
/// exactly that; callers that do not must restart it.
pub(crate) async fn quiesce_and_capture(
    runtime: &dyn RuntimeAdapter,
    capsule: &ContainerId,
    state_dir: &Path,
    verbose: bool,
) -> Result<()> {
    runtime
        .stop(capsule, crate::boxes::SESSION_STOP_GRACE)
        .await
        .context("stopping the capsule to freeze its workspace before capture")?;
    capscode_git::recapture_shadow_export(capsule, state_dir, verbose)
        .await
        .context("exporting the non-git workspace")?;
    Ok(())
}

/// Show what the capture found, ask whether to write it, and write it.
///
/// `interactive` is the caller's judgement about whether a person is present;
/// this additionally requires a real stdin, because a confirmation nobody can
/// answer must not be treated as a yes. Without one, changes stay staged and the
/// host working tree is untouched — the safe half of the decision.
pub(crate) fn present_and_apply(
    handoff: &ShadowHandoff,
    host_repo: &Path,
    interactive: bool,
) -> Result<ShadowOutcome> {
    if handoff.is_empty() {
        note("No changes to hand off.");
        return Ok(ShadowOutcome::NoChanges);
    }

    if !(interactive && std::io::stdin().is_terminal()) {
        note(format!(
            "File changes captured: {} staged ({} clean, {} conflicts), {} deletions, {} unsupported. Host files were not changed.\nStaging: {}\nManifest: {}",
            handoff.staged_change_count(),
            handoff.clean_count(),
            handoff.conflicts.len(),
            handoff.deletion_count(),
            handoff.unsupported.len(),
            handoff.staging_dir.display(),
            handoff.manifest_path.display()
        ));
        return Ok(ShadowOutcome::Staged);
    }

    print_shadow_changes(handoff);
    if handoff.clean_count() == 0 {
        note(format!(
            "No clean files to apply; handoff retained at {}.",
            handoff.state_dir.display()
        ));
        return Ok(ShadowOutcome::Staged);
    }

    let apply = Confirm::with_theme(&crate::ui::theme())
        .with_prompt(format!(
            "Apply {} clean file(s) to {}? ({} conflicts + {} deletions kept in {})",
            handoff.clean_count(),
            host_repo.display(),
            handoff.conflicts.len(),
            handoff.deletion_count(),
            handoff.staging_dir.display()
        ))
        .default(false)
        .interact()
        .context("reading file sync apply confirmation")?;
    if !apply {
        note(format!(
            "Host files were not changed; handoff retained at {}.",
            handoff.state_dir.display()
        ));
        return Ok(ShadowOutcome::Staged);
    }

    // Implicit-runner files (git hooks, .husky, direnv) execute on the HOST the
    // next time an ordinary command runs there, so they are never carried by a
    // bulk "apply all" confirmation. Anything under .git/ is refused outright
    // and is not offered at all.
    let report = apply_shadow_handoff(handoff, host_repo, false)
        .context("applying staged file sync changes")?;
    if !report.refused.is_empty() {
        note(format!(
            "Refused {} git-metadata path(s) (never imported): {}",
            report.refused.len(),
            join_paths(&report.refused)
        ));
    }
    if !report.skipped_implicit_runners.is_empty() {
        note(format!(
            "Skipped {} file(s) the host would execute from repo content; copy them by hand after review: {}",
            report.skipped_implicit_runners.len(),
            join_paths(&report.skipped_implicit_runners)
        ));
    }
    if !report.kept_host_mode.is_empty() {
        note(format!(
            "Kept your file mode for {} file(s) whose executable bit the box changed; set it by hand if you want it: {}",
            report.kept_host_mode.len(),
            join_paths(&report.kept_host_mode)
        ));
    }
    if report.applied.is_empty() {
        note(format!(
            "No files were applied; handoff retained at {}.",
            handoff.state_dir.display()
        ));
        return Ok(ShadowOutcome::Staged);
    }
    if !report.failed.is_empty() {
        note(format!(
            "Applied {} of {} clean file(s); {} could not be applied and stay staged in {}: {}",
            report.applied.len(),
            handoff.clean_count(),
            report.failed.len(),
            handoff.staging_dir.display(),
            report
                .failed
                .iter()
                .map(|issue| format!("{} ({})", escape_shadow_path(&issue.path), issue.reason))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        note(format!(
            "Applied {} clean file(s); retained handoff at {}.",
            report.applied.len(),
            handoff.state_dir.display()
        ));
    }
    let open_conflicts = handoff
        .conflicts
        .iter()
        .filter(|issue| {
            !handoff
                .changes
                .iter()
                .any(|change| change.path == issue.path && change.kind == ShadowChangeKind::Deleted)
        })
        .count();
    Ok(applied_outcome(
        handoff.clean_count(),
        &report,
        open_conflicts,
        handoff.unsupported.len(),
    ))
}

/// Whether an apply that wrote at least one file brought back everything it could.
///
/// Callers record a box as synced only on `Applied`, and `rm` and `uninstall`
/// trust that record. So anything still left behind makes the round `PartlyApplied`.
// DECISION: pending capsule deletions do not count as open work. They are never applied, so
// counting them would keep a box unsynced forever, and removing the box loses no file content.
fn applied_outcome(
    clean_count: usize,
    report: &ShadowApplyReport,
    open_conflicts: usize,
    unsupported: usize,
) -> ShadowOutcome {
    let complete = report.applied.len() == clean_count
        && report.failed.is_empty()
        && report.refused.is_empty()
        && report.skipped_implicit_runners.is_empty()
        && open_conflicts == 0
        && unsupported == 0;
    if complete {
        ShadowOutcome::Applied
    } else {
        ShadowOutcome::PartlyApplied
    }
}

fn join_paths(paths: &[std::path::PathBuf]) -> String {
    paths
        .iter()
        .map(|path| escape_shadow_path(path))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn print_shadow_changes(handoff: &ShadowHandoff) {
    const DISPLAY_LIMIT: usize = 40;
    let mut shown = 0;
    note("File changes:");
    for change in &handoff.changes {
        if shown == DISPLAY_LIMIT {
            break;
        }
        let conflict = handoff
            .conflicts
            .iter()
            .find(|issue| issue.path == change.path)
            .map(|issue| format!(" — conflict: {}", issue.reason))
            .unwrap_or_default();
        note(format!(
            "  {} {}{}",
            change.kind.label(),
            escape_shadow_path(&change.path),
            conflict
        ));
        shown += 1;
    }
    for issue in &handoff.unsupported {
        if shown == DISPLAY_LIMIT {
            break;
        }
        note(format!(
            "  ! {} — unsupported: {}",
            escape_shadow_path(&issue.path),
            issue.reason
        ));
        shown += 1;
    }
    let total = handoff.changes.len() + handoff.unsupported.len();
    if shown < total {
        note(format!("  … {} more entries omitted", total - shown));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use capscode_git::{ShadowApplyReport, ShadowIssue};

    use super::{applied_outcome, ShadowOutcome};

    fn report(applied: usize) -> ShadowApplyReport {
        ShadowApplyReport {
            applied: (0..applied)
                .map(|index| PathBuf::from(format!("f{index}")))
                .collect(),
            refused: Vec::new(),
            skipped_implicit_runners: Vec::new(),
            failed: Vec::new(),
            kept_host_mode: Vec::new(),
        }
    }

    #[test]
    fn a_second_workspace_lock_on_the_same_box_is_refused_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let first = super::lock_workspace(dir.path()).unwrap();
        let error = super::lock_workspace(dir.path()).unwrap_err();
        assert!(
            error.to_string().contains("try again when it finishes"),
            "{error}"
        );
        drop(first);
        super::lock_workspace(dir.path()).unwrap();

        let missing = dir.path().join("gone");
        super::lock_workspace(&missing).unwrap();
        assert!(
            !missing.exists(),
            "locking does not create a state directory"
        );
    }

    #[tokio::test]
    async fn a_waiting_lock_proceeds_once_the_holder_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let holder = super::lock_workspace(dir.path()).unwrap();
        let path = dir.path().to_path_buf();
        let waiter =
            tokio::spawn(async move { super::lock_workspace_waiting(&path).await.is_ok() });
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(!waiter.is_finished(), "it waits while the lock is held");
        drop(holder);
        assert!(waiter.await.unwrap());
    }

    #[test]
    fn locking_every_session_refuses_while_one_is_busy() {
        let sessions = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::create_dir(sessions.path().join(name)).unwrap();
        }
        let busy = super::lock_workspace(&sessions.path().join("b")).unwrap();
        let error = super::lock_all_workspaces(sessions.path()).unwrap_err();
        assert!(format!("{error:#}").contains("session b"), "{error:#}");
        drop(busy);
        assert_eq!(
            super::lock_all_workspaces(sessions.path()).unwrap().len(),
            2
        );
        assert!(super::lock_all_workspaces(&sessions.path().join("none"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_round_that_wrote_every_clean_file_is_applied() {
        assert_eq!(applied_outcome(3, &report(3), 0, 0), ShadowOutcome::Applied);
    }

    #[test]
    fn any_work_left_behind_makes_the_round_partly_applied() {
        assert_eq!(
            applied_outcome(3, &report(3), 1, 0),
            ShadowOutcome::PartlyApplied,
            "an open conflict stays only in the capsule and staging"
        );
        assert_eq!(
            applied_outcome(3, &report(3), 0, 1),
            ShadowOutcome::PartlyApplied,
            "an unsupported entry was never captured"
        );
        assert_eq!(
            applied_outcome(3, &report(2), 0, 0),
            ShadowOutcome::PartlyApplied
        );

        let mut failed = report(2);
        failed.failed.push(ShadowIssue {
            path: PathBuf::from("late"),
            reason: "host changed before apply".into(),
        });
        assert_eq!(
            applied_outcome(3, &failed, 0, 0),
            ShadowOutcome::PartlyApplied
        );

        let mut skipped = report(2);
        skipped
            .skipped_implicit_runners
            .push(PathBuf::from(".envrc"));
        assert_eq!(
            applied_outcome(3, &skipped, 0, 0),
            ShadowOutcome::PartlyApplied
        );

        let mut refused = report(2);
        refused.refused.push(PathBuf::from(".git/config"));
        assert_eq!(
            applied_outcome(3, &refused, 0, 0),
            ShadowOutcome::PartlyApplied
        );
    }
}
