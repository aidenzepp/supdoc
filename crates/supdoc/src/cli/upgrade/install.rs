//! Verifies a downloaded release archive and replaces the running binary with what it holds.
//!
//! Order matters: the archive's SHA-256 is checked against its `SHA256SUMS` line first, then the
//! one expected entry is read out in memory, and only then is anything written beside the
//! binary. A failure at any step leaves the old binary, and removes the temporary file.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::config::confine::{self, Boundary, Escape};
use crate::hash::Hash;

use super::ReleaseVersion;
use super::release::{self, ArchiveError, Triple};

/// The permission bits a replaced binary gets.
const BINARY_MODE: u32 = 0o755;

/// Why an archive was refused before anything was written.
#[derive(Debug, thiserror::Error)]
pub(super) enum VerifyError {
    /// The archive's SHA-256 is not the one `SHA256SUMS` lists.
    #[error("the downloaded archive has SHA-256 {actual}, but SHA256SUMS lists {expected}")]
    Mismatch {
        /// The digest `SHA256SUMS` lists.
        expected: String,
        /// The digest of the bytes received.
        actual: String,
    },
    /// The archive does not hold exactly the expected binary.
    #[error(transparent)]
    Archive(#[from] ArchiveError),
}

/// Why the binary could not be put in place.
#[derive(Debug, thiserror::Error)]
pub(super) enum ReplaceError {
    /// The binary has no directory to write beside.
    #[error("{} has no parent directory", .0.display())]
    NoDirectory(PathBuf),
    /// A path to write lies outside the binary's directory once symlinks are followed.
    #[error("{} lies outside the binary's directory", .0.display())]
    Outside(PathBuf),
    /// A file operation failed.
    #[error("{action} {}: {source}", .path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The operating system's error.
        source: io::Error,
    },
}

/// The binary inside `archive`, once the archive's SHA-256 equals `expected`.
///
/// The digest is compared before the archive is opened, so nothing in an unverified archive is
/// read.
///
/// # Errors
///
/// [`VerifyError::Mismatch`] for a different digest, [`VerifyError::Archive`] when the archive
/// does not hold exactly one regular, size-capped `supdoc` entry at the expected path.
pub(super) fn verified_binary(
    archive: &[u8],
    expected: &str,
    version: &ReleaseVersion,
    triple: Triple,
) -> Result<Vec<u8>, VerifyError> {
    let actual = Hash::digest(archive).to_string();
    if actual != expected {
        return Err(VerifyError::Mismatch {
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(release::extract_binary(
        archive,
        version,
        triple,
        release::MAX_BINARY_BYTES,
    )?)
}

/// Replaces the file at `exe` with `binary`, mode 0755, atomically.
///
/// `exe` is the binary's real path. The new bytes go to a fresh file in the same directory, are
/// synced, and renamed over `exe`; the running process keeps its open image. Every write,
/// rename and remove is admitted against that directory first. On failure the temporary file is
/// removed and `exe` is untouched.
///
/// Effect: creates, renames and removes files beside `exe`.
///
/// # Errors
///
/// [`ReplaceError`] when `exe` has no directory, a path leaves the directory, or a file
/// operation fails.
pub(super) fn replace_exe(exe: &Path, binary: &[u8]) -> Result<(), ReplaceError> {
    let dir = exe
        .parent()
        .ok_or_else(|| ReplaceError::NoDirectory(exe.to_path_buf()))?;
    let boundary = Boundary::resolve(dir);
    let temp_path = dir.join(temp_name());
    let outside = |path: &Path, _: Escape| ReplaceError::Outside(path.to_path_buf());
    let temp = boundary
        .admit(&temp_path)
        .map_err(|escape| outside(&temp_path, escape))?;
    let io_error = |action: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| ReplaceError::Io {
            action,
            path,
            source,
        }
    };
    let mut file = confine::create_new(&temp, BINARY_MODE)
        .map_err(io_error("could not create", temp.as_path()))?;
    let finished = (|| {
        file.write_all(binary)
            .map_err(io_error("could not write", temp.as_path()))?;
        set_mode(&file).map_err(io_error("could not set the mode of", temp.as_path()))?;
        file.sync_all()
            .map_err(io_error("could not sync", temp.as_path()))?;
        let target = boundary.admit(exe).map_err(|escape| outside(exe, escape))?;
        let temp = boundary
            .admit(&temp_path)
            .map_err(|escape| outside(&temp_path, escape))?;
        confine::rename_into(&temp, &target).map_err(io_error("could not replace", exe))
    })();
    drop(file);
    if finished.is_err()
        && let Ok(temp) = boundary.admit(&temp_path)
    {
        drop(confine::remove_file(&temp));
    }
    finished
}

/// A name for the temporary file that no earlier run is likely to have used.
fn temp_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!(".supdoc-upgrade-{}-{nanos}", std::process::id())
}

/// Gives `file` exactly [`BINARY_MODE`], whatever the umask was at creation.
///
/// Effect: changes the file's permissions.
///
/// # Errors
///
/// Fails when the permissions cannot be changed.
#[cfg(unix)]
fn set_mode(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(BINARY_MODE))
}

/// Windows has no permission bits to set.
///
/// # Errors
///
/// Never fails.
#[cfg(not(unix))]
fn set_mode(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_rename_removes_the_temp_file_and_keeps_the_target() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::create_dir(&exe).expect("the stand-in target should be creatable");
        fs::write(exe.join("keep"), "old").expect("the stand-in content should write");

        let result = replace_exe(&exe, b"new binary");

        assert!(
            matches!(result, Err(ReplaceError::Io { .. })),
            "renaming a file over a non-empty directory should fail: {result:?}"
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .expect("the directory should list")
            .map(|entry| entry.expect("the entry should read").file_name())
            .collect();
        assert_eq!(names, ["supdoc"], "no temporary file may remain");
        assert_eq!(
            fs::read(exe.join("keep")).expect("the old content should read"),
            b"old"
        );
    }
}
