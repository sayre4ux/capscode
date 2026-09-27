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
use capscode_git::{apply_shadow_handoff, escape_shadow_path, ShadowHandoff};
use dialoguer::Confirm;

use crate::ui::note;

/// What a handoff did, for the caller to report or act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShadowOutcome {
    /// The capture found no changes; nothing was staged and nothing applied.
    NoChanges,
    /// Changes were staged and written to the host working tree.
    Applied,
    /// Changes were staged but NOT written — the user declined, the terminal was
    /// not interactive, or every change was a conflict. The staging directory
    /// holds the work.
    Staged,
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
    Ok(ShadowOutcome::Applied)
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
