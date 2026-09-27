//! Unpacking the release's image build context (`capscode-<ver>-build-context.tar.gz`).
//!
//! The tarball's SHA-256 is checked against the release's `SHA256SUMS` before it
//! gets here, so it is as trusted as the binary itself. It is still parsed as
//! hostile input: a release built from a compromised checkout must not be able to
//! write outside the destination. Every entry must sit under the expected top-level
//! directory, use only plain path components, and be a regular file or directory.
//! Links of any kind and device files are refused, which is stricter than "links
//! pointing outside": the build context needs none.
// DECISION: refuse every link instead of resolving targets; resolving them safely is
// the error-prone part of tar extraction and the release script never emits one. Files are written by this code
//! (never by tar's own unpacker) into a fresh private directory that is renamed
//! into place only after the whole archive passed.

use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use tar::EntryType;

/// The unpacked context is about 2 MiB today; 64 MiB leaves room for growth while
/// bounding a decompression bomb.
pub(crate) const MAX_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_ENTRIES: usize = 20_000;
/// The file that proves the archive is a build context.
const REQUIRED_FILE: &str = "scripts/build-images.sh";

/// The directory every entry must sit under, matching the asset name.
pub(crate) fn top_level_dir(version: &str) -> String {
    format!("capscode-{version}-build-context")
}

/// A version string is used as a directory name; refuse anything that is not a
/// plain SemVer-shaped component.
fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

/// Unpack `archive` (gzip-compressed tar) for `version` into
/// `<build_dir>/<version>`, replacing an earlier copy only after the new one is
/// complete. Returns the final directory.
pub(crate) fn install_build_context(
    archive: &[u8],
    version: &str,
    build_dir: &Path,
) -> Result<PathBuf> {
    if !safe_component(version) {
        bail!("refusing to unpack a build context for an unexpected version string");
    }
    fs::create_dir_all(build_dir).with_context(|| format!("creating {}", build_dir.display()))?;
    set_mode(build_dir, 0o700)?;
    let staging = tempfile::Builder::new()
        .prefix(".unpack-")
        .tempdir_in(build_dir)
        .with_context(|| format!("creating a staging directory in {}", build_dir.display()))?;
    extract(archive, &top_level_dir(version), staging.path())?;
    if !staging.path().join(REQUIRED_FILE).is_file() {
        bail!("the build context has no {REQUIRED_FILE}; refusing it");
    }

    let destination = build_dir.join(version);
    let previous = match fs::symlink_metadata(&destination) {
        Ok(_) => {
            let aside = tempfile::Builder::new()
                .prefix(&format!(".old-{version}-"))
                .tempdir_in(build_dir)
                .context("reserving a name for the previous build context")?;
            let aside_path = aside.path().join("previous");
            fs::rename(&destination, &aside_path)
                .with_context(|| format!("moving the previous {} aside", destination.display()))?;
            Some((aside, aside_path))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", destination.display()))
        }
    };
    let staged = staging.keep();
    if let Err(error) = fs::rename(&staged, &destination) {
        let _ = fs::remove_dir_all(&staged);
        if let Some((_, aside_path)) = &previous {
            let _ = fs::rename(aside_path, &destination);
        }
        return Err(error)
            .with_context(|| format!("moving the build context to {}", destination.display()));
    }
    // Dropping the reservation removes the previous copy. remove_dir_all does not
    // follow symlinks, so a link left at the old path is removed, not its target.
    drop(previous);
    sync_dir(build_dir);
    Ok(destination)
}

/// Validate and write every entry of `archive` under `root`, stripping `top`.
pub(crate) fn extract(archive: &[u8], top: &str, root: &Path) -> Result<()> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let limited = Limited {
        inner: decoder,
        remaining: MAX_UNPACKED_BYTES,
    };
    let mut tar = tar::Archive::new(limited);
    let mut count = 0usize;
    for entry in tar.entries().context("reading the build context archive")? {
        let mut entry = entry.context("reading the build context archive")?;
        count += 1;
        if count > MAX_ENTRIES {
            bail!("the build context has more than {MAX_ENTRIES} entries; refusing it");
        }
        let path = entry
            .path()
            .context("reading a build context entry name")?
            .into_owned();
        let shown = path.display().to_string();
        let relative = relative_path(&path, top)
            .with_context(|| format!("refusing build context entry {shown}"))?;
        let kind = entry.header().entry_type();
        match kind {
            EntryType::Directory => {
                if let Some(relative) = relative {
                    let dir = root.join(relative);
                    fs::create_dir_all(&dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                    set_mode(&dir, 0o755)?;
                }
            }
            EntryType::Regular | EntryType::Continuous => {
                let Some(relative) = relative else {
                    bail!(
                        "refusing build context entry {shown}: the top-level directory is a file"
                    );
                };
                let executable = entry.header().mode().unwrap_or(0) & 0o111 != 0;
                let file_path = root.join(relative);
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                write_new_file(&mut entry, &file_path, executable)
                    .with_context(|| format!("writing build context file {shown}"))?;
            }
            EntryType::Symlink | EntryType::Link => {
                bail!("refusing build context entry {shown}: links are not allowed");
            }
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                bail!("refusing build context entry {shown}: device and special files are not allowed");
            }
            _ => bail!("refusing build context entry {shown}: unsupported entry type"),
        }
    }
    if count == 0 {
        bail!("the build context archive is empty");
    }
    Ok(())
}

/// `top/<plain components>` -> the components after `top` (`None` for `top` itself).
fn relative_path(path: &Path, top: &str) -> Result<Option<PathBuf>> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == top => {}
        Some(Component::RootDir | Component::Prefix(_)) => bail!("absolute paths are not allowed"),
        _ => bail!("every entry must be under {top}/"),
    }
    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::ParentDir => bail!("`..` is not allowed"),
            _ => bail!("only plain path components are allowed"),
        }
    }
    Ok((!relative.as_os_str().is_empty()).then_some(relative))
}

fn write_new_file(source: &mut impl Read, path: &Path, executable: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    // create_new refuses an existing path, so a duplicate entry cannot overwrite
    // an earlier file and nothing is written through a pre-existing link.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    io::copy(source, &mut file)?;
    file.sync_all()?;
    drop(file);
    set_mode(path, if executable { 0o755 } else { 0o644 })
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn sync_dir(path: &Path) {
    if let Ok(dir) = fs::File::open(path) {
        let _ = dir.sync_all();
    }
}

/// Fails, rather than ending early, once more than the allowed bytes come out of
/// the decompressor: a truncated read could otherwise look like a short archive.
struct Limited<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read as u64 > self.remaining {
            return Err(io::Error::other(format!(
                "the build context unpacks to more than {MAX_UNPACKED_BYTES} bytes"
            )));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}
