//! Optional `gh attestation verify` bridge for downloaded release assets (toolchain
//! locks, CLI binaries, and the image build context).
//!
//! Off by default (`update.verify_with_gh`). When on, it fails closed: a missing
//! `gh`, a non-zero exit, or a timeout all refuse the download.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::channel::ChannelName;

pub(crate) const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// Find an executable `gh` in a PATH-style list of directories.
pub(crate) fn find_gh(path_env: Option<&OsStr>) -> Option<PathBuf> {
    let path_env = path_env?;
    std::env::split_paths(path_env)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("gh"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// The workflows that produce attested release assets, relative to the channel repository.
pub(crate) const TOOLCHAIN_WORKFLOW: &str = ".github/workflows/release-toolchain.yml";
pub(crate) const CLI_WORKFLOW: &str = ".github/workflows/release-cli.yml";

/// `<owner>/<repo>/<workflow path>`, the form `gh attestation verify --signer-workflow` takes.
pub(crate) fn signer_workflow(channel: &ChannelName, workflow: &str) -> String {
    format!("{channel}/{workflow}")
}

/// Run `gh attestation verify` on the exact downloaded lock bytes.
pub(crate) fn verify_lock(
    lock_bytes: &[u8],
    channel: &ChannelName,
    path_env: Option<&OsStr>,
    timeout: Duration,
) -> Result<()> {
    verify_bytes(
        &Subject {
            bytes: lock_bytes,
            file_name: "toolchain.lock.toml",
            what: "lock",
            workflow: TOOLCHAIN_WORKFLOW,
        },
        channel,
        path_env,
        timeout,
    )
}

/// One downloaded artifact to verify.
pub(crate) struct Subject<'a> {
    pub bytes: &'a [u8],
    pub file_name: &'a str,
    /// How messages name the artifact, for example "lock" or "binary".
    pub what: &'a str,
    /// The release workflow that must have signed the attestation.
    pub workflow: &'a str,
}

/// Run `gh attestation verify <file> --repo <channel> --signer-workflow
/// <channel>/<workflow>` on the exact downloaded bytes. Pinning the signer workflow
/// means an attestation made by any other workflow in the repository (a test job,
/// a pull-request build) does not vouch for a release asset.
pub(crate) fn verify_bytes(
    subject: &Subject<'_>,
    channel: &ChannelName,
    path_env: Option<&OsStr>,
    timeout: Duration,
) -> Result<()> {
    let what = subject.what;
    let Some(gh) = find_gh(path_env) else {
        bail!(
            "update.verify_with_gh is on, but the GitHub CLI `gh` is not on PATH; install gh, or turn the check off with `capscode config set update.verify_with_gh false`"
        );
    };
    let dir = tempfile::tempdir()
        .context("creating a temporary directory for attestation verification")?;
    let path = dir.path().join(subject.file_name);
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("writing the {what} for attestation verification"))?;
    file.write_all(subject.bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing the {what} for attestation verification"))?;
    drop(file);
    let signer = signer_workflow(channel, subject.workflow);
    let mut child = Command::new(&gh)
        .arg("attestation")
        .arg("verify")
        .arg(&path)
        .arg("--repo")
        .arg(channel.as_str())
        .arg("--signer-workflow")
        .arg(&signer)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // gh explains a failed verification on stderr; let the user see it.
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting {}", gh.display()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for gh")? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "gh attestation verify did not finish within {} seconds; refusing the {what}",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        bail!(
            "gh attestation verify rejected the downloaded {what} for {channel}; refusing it. Run `gh attestation verify <file> --repo {channel} --signer-workflow {signer}` yourself to see why"
        );
    }
    Ok(())
}
