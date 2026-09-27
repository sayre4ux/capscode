use std::path::Path;
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use capscode_core::SessionId;
use git2::Oid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishedBranch {
    pub name: String,
    pub source_ref: String,
    pub diverged: bool,
}

/// Publish the capsule's private head as a local branch without changing the
/// host worktree. Existing branches only move when the new head is a fast-forward.
pub(crate) fn publish_session_branch(
    host_repo: &Path,
    session: &SessionId,
) -> Result<Option<PublishedBranch>> {
    let source_ref = format!("refs/capscode/{}/head", session.as_str());
    if !ref_exists(host_repo, &source_ref)? {
        return Ok(None);
    }
    let source_head = match git_output(
        host_repo,
        &["rev-parse", "--verify", &format!("{source_ref}^{{commit}}")],
    ) {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)
            .context("git returned a non-UTF-8 capsule commit id")?
            .trim()
            .to_string(),
        Ok(output) if output.status.code() == Some(128) => return Ok(None),
        Ok(output) => bail!(
            "git rev-parse failed for {source_ref}: {}",
            output_message(&output)
        ),
        Err(error) => return Err(error).context("checking the capsule's saved git ref"),
    };
    let source_head = Oid::from_str(&source_head).context("parsing the capsule commit id")?;
    let target = capscode_git::publication_target(host_repo, session, source_head)
        .context("mapping capsule history for branch publication")?
        .to_string();

    let name = branch_candidate(host_repo, session)?;
    let reference = format!("refs/heads/{name}");

    if !ref_exists(host_repo, &reference)? {
        let output = git_output(host_repo, &["branch", "--", &name, &target])?;
        if output.status.success() {
            return Ok(Some(PublishedBranch {
                name,
                source_ref,
                diverged: false,
            }));
        }

        // A concurrent git operation may have created this ref. Re-read it and
        // apply the same fast-forward rule instead of overwriting it.
        if !ref_exists(host_repo, &reference)? {
            bail!(
                "could not create branch {name}: {}",
                output_message(&output)
            );
        }
    }

    let current = git_output(host_repo, &["rev-parse", "--verify", &reference])?;
    if !current.status.success() {
        bail!("could not read branch {name}: {}", output_message(&current));
    }
    let current = String::from_utf8(current.stdout)
        .context("git returned a non-UTF-8 branch commit id")?
        .trim()
        .to_string();

    let current_is_ancestor = git_output(
        host_repo,
        &["merge-base", "--is-ancestor", &current, &target],
    )?;
    if current_is_ancestor.status.success() {
        let updated = git_output(host_repo, &["update-ref", &reference, &target, &current])?;
        if updated.status.success() {
            return Ok(Some(PublishedBranch {
                name,
                source_ref,
                diverged: false,
            }));
        }
        // A concurrent writer may have advanced or changed the branch after the
        // ancestry check. Preserve its ref and direct the user to both commits.
        return Ok(Some(PublishedBranch {
            name,
            source_ref,
            diverged: true,
        }));
    }

    if current_is_ancestor.status.code() != Some(1) {
        bail!(
            "could not compare branch {name} with the capsule ref: {}",
            output_message(&current_is_ancestor)
        );
    }

    let target_is_ancestor = git_output(
        host_repo,
        &["merge-base", "--is-ancestor", &target, &current],
    )?;
    if target_is_ancestor.status.success() {
        return Ok(Some(PublishedBranch {
            name,
            source_ref,
            diverged: false,
        }));
    }
    if target_is_ancestor.status.code() != Some(1) {
        bail!(
            "could not compare branch {name} with the capsule ref: {}",
            output_message(&target_is_ancestor)
        );
    }

    Ok(Some(PublishedBranch {
        name,
        source_ref,
        diverged: true,
    }))
}

pub(crate) fn branch_candidate(host_repo: &Path, session: &SessionId) -> Result<String> {
    let short_id = short_session_id(session);
    if ref_exists(host_repo, "refs/heads/capscode")? {
        Ok(format!("capscode-{short_id}"))
    } else {
        Ok(format!("capscode/{short_id}"))
    }
}

pub(crate) fn branch_hint(branch: &PublishedBranch) -> String {
    if branch.diverged {
        format!(
            "Branch {} was left unchanged because it diverged; capsule work remains at {}. Inspect with `git log --oneline --decorate {} {}`.",
            branch.name, branch.source_ref, branch.name, branch.source_ref
        )
    } else {
        format!(
            "Saved as branch {}. Inspect with `git log --oneline --decorate -n 20 {}`.",
            branch.name, branch.name
        )
    }
}

fn short_session_id(session: &SessionId) -> String {
    session
        .as_str()
        .chars()
        .take(8)
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn ref_exists(host_repo: &Path, reference: &str) -> Result<bool> {
    let output = git_output(host_repo, &["show-ref", "--verify", "--quiet", reference])?;
    match output.status.code() {
        Some(0) => Ok(true),
        // `show-ref` uses 1 for an absent ref in a valid repository and 128
        // when the directory is not a Git repository. Both mean there is no
        // saved capsule ref to publish (for example, a shadow/non-Git session).
        Some(1 | 128) => Ok(false),
        _ => bail!(
            "could not inspect git ref {reference}: {}",
            output_message(&output)
        ),
    }
}

fn git_output(host_repo: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(host_repo)
        .args(args)
        .output()
        .with_context(|| format!("launching git {}", args.join(" ")))
}

fn output_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use super::publish_session_branch;
    use capscode_core::SessionId;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo() -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        git(
            directory.path(),
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        );
        directory
    }

    fn add_capsule_commit(repo: &Path, session: &SessionId, parent: &str) -> String {
        let tree = git(repo, &["write-tree"]);
        let commit = git(
            repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit-tree",
                &tree,
                "-p",
                parent,
                "-m",
                "capsule",
            ],
        );
        git(
            repo,
            &[
                "update-ref",
                &format!("refs/capscode/{}/head", session.as_str()),
                &commit,
            ],
        );
        commit
    }

    #[test]
    fn publishes_and_fast_forwards_without_checkout() {
        let repo = repo();
        let session = SessionId::from("12345678-0000-0000-0000-000000000000".to_string());
        let original_head = git(repo.path(), &["rev-parse", "HEAD"]);
        let first = add_capsule_commit(
            repo.path(),
            &session,
            &git(repo.path(), &["rev-parse", "HEAD"]),
        );
        let published = publish_session_branch(repo.path(), &session)
            .unwrap()
            .unwrap();
        assert_eq!(published.name, "capscode/12345678");
        assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), original_head);
        assert_eq!(
            git(repo.path(), &["rev-parse", "refs/heads/capscode/12345678"]),
            first
        );

        let second = add_capsule_commit(repo.path(), &session, &first);
        let published = publish_session_branch(repo.path(), &session)
            .unwrap()
            .unwrap();
        assert!(!published.diverged);
        assert_eq!(
            git(repo.path(), &["rev-parse", "refs/heads/capscode/12345678"]),
            second
        );
    }

    #[test]
    fn missing_capsule_ref_is_not_an_error() {
        let repo = repo();
        let session = SessionId::from("12345678-0000-0000-0000-000000000000".to_string());

        assert_eq!(publish_session_branch(repo.path(), &session).unwrap(), None);
    }

    #[test]
    fn a_non_git_host_directory_has_no_capsule_branch_to_publish() {
        let host = TempDir::new().unwrap();
        let session = SessionId::from("12345678-0000-0000-0000-000000000000".to_string());
        assert_eq!(publish_session_branch(host.path(), &session).unwrap(), None);
    }

    #[test]
    fn falls_back_when_capscode_is_a_branch_and_keeps_diverged_branch() {
        let repo = repo();
        git(repo.path(), &["branch", "capscode"]);
        let session = SessionId::from("abcdefab-0000-0000-0000-000000000000".to_string());
        let base = git(repo.path(), &["rev-parse", "HEAD"]);
        let capsule = add_capsule_commit(repo.path(), &session, &base);
        let first = publish_session_branch(repo.path(), &session)
            .unwrap()
            .unwrap();
        assert_eq!(first.name, "capscode-abcdefab");

        let divergent = git(
            repo.path(),
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit-tree",
                &git(repo.path(), &["write-tree"]),
                "-p",
                &base,
                "-m",
                "host",
            ],
        );
        git(
            repo.path(),
            &["update-ref", "refs/heads/capscode-abcdefab", &divergent],
        );
        let outcome = publish_session_branch(repo.path(), &session)
            .unwrap()
            .unwrap();
        assert!(outcome.diverged);
        assert_eq!(
            git(repo.path(), &["rev-parse", "refs/heads/capscode-abcdefab"]),
            divergent
        );
        assert_eq!(
            git(
                repo.path(),
                &[
                    "rev-parse",
                    &format!("refs/capscode/{}/head", session.as_str())
                ]
            ),
            capsule
        );
    }

    #[test]
    fn redacted_session_branch_is_reconnected_to_host_history() {
        let repo = repo();
        let session = SessionId::from("abcdefab-1111-0000-0000-000000000000".to_string());
        let host_baseline = git(repo.path(), &["rev-parse", "HEAD"]);
        let tree = git(repo.path(), &["write-tree"]);
        let capsule_baseline = git(
            repo.path(),
            &[
                "-c",
                "user.name=capsule",
                "-c",
                "user.email=capsule@localhost",
                "commit-tree",
                &tree,
                "-m",
                "capsule: redacted workspace baseline",
            ],
        );
        let capsule_head = git(
            repo.path(),
            &[
                "-c",
                "user.name=capsule",
                "-c",
                "user.email=capsule@localhost",
                "commit-tree",
                &tree,
                "-p",
                &capsule_baseline,
                "-m",
                "agent change",
            ],
        );
        git(
            repo.path(),
            &[
                "update-ref",
                &format!("refs/capscode/{}/head", session.as_str()),
                &capsule_head,
            ],
        );
        let metadata_dir = repo.path().join(".git/capscode");
        std::fs::create_dir_all(&metadata_dir).unwrap();
        std::fs::write(
            metadata_dir.join(format!("{}-history.json", session.as_str())),
            format!(
                "{{\"capsule_baseline\":\"{capsule_baseline}\",\"host_baseline\":\"{host_baseline}\"}}"
            ),
        )
        .unwrap();

        let published = publish_session_branch(repo.path(), &session)
            .unwrap()
            .unwrap();
        let branch_head = git(
            repo.path(),
            &["rev-parse", &format!("refs/heads/{}", published.name)],
        );
        assert_ne!(branch_head, capsule_head);
        assert!(Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["merge-base", "--is-ancestor", &host_baseline, &branch_head])
            .status()
            .unwrap()
            .success());
    }
}
