//! Where the files that build CapsCode's images come from.
//!
//! Images are never published: every user builds them locally with
//! `scripts/build-images.sh`. A source install runs the script from its checkout.
//! A binary install has no checkout, so the installer (and `capscode update --cli`)
//! unpacks the release's build-context tarball into
//! `<CapsCode home>/build/<version>/`. Lookup order:
//!
//! 1. `CAPSCODE_BUILD_IMAGES`, the user's explicit choice of script;
//! 2. the build context for this CLI's own version;
//! 3. the source tree this binary was compiled from, only when it still exists and
//!    belongs to the current user;
//! 4. otherwise an error that says how to get the files.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

pub(crate) const BUILD_SCRIPT_ENV: &str = "CAPSCODE_BUILD_IMAGES";
const SCRIPT: &str = "scripts/build-images.sh";
const LOCK: &str = "images/toolchain.lock.toml";
pub(crate) const INSTALLER_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/sayre4ux/capscode/main/scripts/install.sh | bash";
/// The command that builds the active toolchain's images on any install.
pub(crate) const BUILD_COMMAND: &str = "capscode toolchain build";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Env,
    Installed,
    SourceTree,
}

#[derive(Clone, Debug)]
pub(crate) struct BuildFiles {
    pub script: PathBuf,
    /// The directory holding `images/` next to the script's `scripts/`, when it exists.
    pub root: Option<PathBuf>,
    pub origin: Origin,
}

impl BuildFiles {
    /// One line saying which build script runs, for the user's benefit.
    pub(crate) fn describe(&self) -> String {
        let source = match self.origin {
            Origin::Env => "set by CAPSCODE_BUILD_IMAGES",
            Origin::Installed => "from the installed build context",
            Origin::SourceTree => "from the source checkout",
        };
        format!("Building images with {} ({source})", self.script.display())
    }

    /// The lock shipped beside the script, if it pins exactly `digest`. The
    /// built-in lock is compiled into the CLI; the file is only used when it is the
    /// same toolchain, so a stale checkout cannot build a different one.
    pub(crate) fn lock_for_digest(&self, digest: &str) -> Option<PathBuf> {
        let path = self.root.as_ref()?.join(LOCK);
        let lock = crate::toolchain::read_validated(&path).ok()?;
        (crate::toolchain::digest(&lock) == digest).then_some(path)
    }
}

/// `<CapsCode home>/build/<version>`, where binary installs keep the build context.
pub(crate) fn installed_dir(home: &Path, version: &str) -> PathBuf {
    home.join("build").join(version)
}

/// The workspace root this binary was compiled from (it may not exist here).
pub(crate) fn compiled_source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Locate the build files for this process: environment, CapsCode home, own version.
pub(crate) fn locate() -> Result<BuildFiles> {
    locate_in(
        std::env::var_os(BUILD_SCRIPT_ENV).as_deref(),
        &crate::toolchain::config_home(),
        crate::update::own_version(),
        &compiled_source_root(),
    )
}

pub(crate) fn locate_in(
    env_script: Option<&OsStr>,
    home: &Path,
    version: &str,
    source_root: &Path,
) -> Result<BuildFiles> {
    if let Some(script) = env_script.filter(|value| !value.is_empty()) {
        let script = PathBuf::from(script);
        if !script.is_file() {
            bail!(
                "{BUILD_SCRIPT_ENV} is set to {}, which is not a file; point it at scripts/build-images.sh or unset it",
                script.display()
            );
        }
        let root = script
            .parent()
            .and_then(Path::parent)
            .filter(|root| root.join("images").is_dir())
            .map(Path::to_path_buf);
        return Ok(BuildFiles {
            script,
            root,
            origin: Origin::Env,
        });
    }

    let installed = installed_dir(home, version);
    if installed.join(SCRIPT).is_file() {
        return Ok(BuildFiles {
            script: installed.join(SCRIPT),
            root: Some(installed),
            origin: Origin::Installed,
        });
    }

    // DECISION: trust the compiled-in source tree only when the current user owns it.
    // A released binary records the CI runner's checkout path. Running a script
    // found there is only safe when the tree is the user's own: on a shared machine
    // another account could create that path.
    let script = source_root.join(SCRIPT);
    if script.is_file()
        && source_root.join("Cargo.toml").is_file()
        && owned_by_current_user(source_root)
        && owned_by_current_user(&script)
    {
        return Ok(BuildFiles {
            script,
            root: Some(source_root.to_path_buf()),
            origin: Origin::SourceTree,
        });
    }

    bail!("{}", missing_message(version, &installed))
}

fn missing_message(version: &str, installed: &Path) -> String {
    format!(
        "the image build files for CapsCode {version} are missing (expected {}); reinstall with `{INSTALLER_COMMAND}`, or set {BUILD_SCRIPT_ENV} to scripts/build-images.sh in a CapsCode checkout",
        installed.display()
    )
}

#[cfg(unix)]
fn owned_by_current_user(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    std::fs::metadata(path).is_ok_and(|metadata| metadata.uid() == uid)
}

#[cfg(not(unix))]
fn owned_by_current_user(_path: &Path) -> bool {
    true
}

// DECISION: every "image is not built" hint names `capscode toolchain build`, which
// works for source and binary installs alike and builds the active toolchain.
/// What to tell a user whose images are missing: the command that works for this
/// install, or how to restore the build files when there are none.
pub(crate) fn image_build_hint() -> String {
    match locate() {
        Ok(_) => BUILD_COMMAND.to_owned(),
        Err(_) => format!(
            "reinstall CapsCode with `{INSTALLER_COMMAND}` (the image build files are missing), then run `{BUILD_COMMAND}`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_script(root: &Path) -> PathBuf {
        let script = root.join(SCRIPT);
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        script
    }

    fn source_tree(root: &Path) -> PathBuf {
        write_script(root);
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(root.join("images")).unwrap();
        root.to_path_buf()
    }

    #[test]
    fn the_environment_wins_over_every_other_location() {
        let temp = tempfile::tempdir().unwrap();
        let env_root = temp.path().join("custom");
        let env_script = write_script(&env_root);
        std::fs::create_dir_all(env_root.join("images")).unwrap();
        let home = temp.path().join("home");
        write_script(&installed_dir(&home, "0.2.0"));
        let source = source_tree(&temp.path().join("src"));

        let found = locate_in(Some(env_script.as_os_str()), &home, "0.2.0", &source).unwrap();
        assert_eq!(found.origin, Origin::Env);
        assert_eq!(found.script, env_script);
        assert_eq!(found.root.as_deref(), Some(env_root.as_path()));
    }

    #[test]
    fn a_missing_environment_script_is_an_error_not_a_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let source = source_tree(&temp.path().join("src"));
        let missing = temp.path().join("nope.sh");
        let error = locate_in(Some(missing.as_os_str()), temp.path(), "0.2.0", &source)
            .unwrap_err()
            .to_string();
        assert!(error.contains(BUILD_SCRIPT_ENV), "{error}");
    }

    #[test]
    fn the_build_context_for_this_version_wins_over_the_source_tree() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let installed = installed_dir(&home, "0.2.0");
        write_script(&installed);
        write_script(&installed_dir(&home, "0.1.0"));
        let source = source_tree(&temp.path().join("src"));

        let found = locate_in(None, &home, "0.2.0", &source).unwrap();
        assert_eq!(found.origin, Origin::Installed);
        assert_eq!(found.script, installed.join(SCRIPT));
        assert_eq!(found.root.as_deref(), Some(installed.as_path()));
        // An empty variable counts as unset.
        let found = locate_in(Some(OsStr::new("")), &home, "0.2.0", &source).unwrap();
        assert_eq!(found.origin, Origin::Installed);
    }

    #[test]
    fn another_versions_build_context_is_not_used() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        write_script(&installed_dir(&home, "0.1.0"));
        let source = source_tree(&temp.path().join("src"));
        let found = locate_in(None, &home, "0.2.0", &source).unwrap();
        assert_eq!(found.origin, Origin::SourceTree);
    }

    #[test]
    fn the_source_tree_is_used_only_when_it_exists() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let source = source_tree(&temp.path().join("src"));
        let found = locate_in(None, &home, "0.2.0", &source).unwrap();
        assert_eq!(found.origin, Origin::SourceTree);
        assert_eq!(found.script, source.join(SCRIPT));

        let gone = temp.path().join("runner/work/capscode");
        let error = locate_in(None, &home, "0.2.0", &gone)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("image build files for CapsCode 0.2.0 are missing"),
            "{error}"
        );
        assert!(error.contains(INSTALLER_COMMAND), "{error}");
        assert!(error.contains(BUILD_SCRIPT_ENV), "{error}");
    }

    #[test]
    fn a_source_tree_without_a_workspace_manifest_is_not_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("src");
        write_script(&root);
        assert!(locate_in(None, temp.path(), "0.2.0", &root).is_err());
    }

    #[test]
    fn the_shipped_lock_is_used_only_for_the_same_toolchain() {
        let temp = tempfile::tempdir().unwrap();
        let root = source_tree(&temp.path().join("ctx"));
        let builtin = crate::toolchain::BUILTIN_LOCK;
        std::fs::write(root.join(LOCK), builtin).unwrap();
        let files = BuildFiles {
            script: root.join(SCRIPT),
            root: Some(root.clone()),
            origin: Origin::Installed,
        };
        let digest = crate::toolchain::digest(&crate::toolchain::builtin_lock().unwrap());
        assert_eq!(files.lock_for_digest(&digest), Some(root.join(LOCK)));
        assert_eq!(files.lock_for_digest("000000000000"), None);
    }
}
