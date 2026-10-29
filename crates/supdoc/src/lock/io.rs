//! Reading and atomically writing `supdoc.lock` (02 L9).

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use super::error::LockError;
use super::model::Lockfile;
use crate::path::AbsolutePath;

impl Lockfile {
    /// Reads and parses the lockfile at `path`. `Ok(None)` if the file doesn't exist; the
    /// command decides what that means.
    ///
    /// # Errors
    ///
    /// Fails if the file exists but cannot be read, or its text is not a valid lockfile.
    pub fn read(path: &AbsolutePath) -> Result<Option<Self>, LockError> {
        let text = match fs::read_to_string(path.as_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(LockError::Read {
                    path: path.clone(),
                    error,
                });
            }
        };
        Self::parse(&text)
            .map(Some)
            .map_err(|error| LockError::Parse {
                path: path.clone(),
                error: Box::new(error),
            })
    }

    /// Writes the canonical text to `path`: a temporary file in the same directory, then a
    /// rename, so an interrupted run never leaves half a lockfile.
    ///
    /// # Errors
    ///
    /// Fails if the temporary file cannot be written or moved into place. Any existing lockfile
    /// is left unchanged.
    pub fn write(&self, path: &AbsolutePath) -> Result<(), LockError> {
        let temp = temp_path_beside(path.as_path());
        write_then_rename(&temp, path.as_path(), self.render().as_bytes()).map_err(|error| {
            drop(fs::remove_file(&temp));
            LockError::Write {
                path: path.clone(),
                error,
            }
        })
    }
}

/// A scratch name next to `target`, so the final rename stays on one filesystem.
fn temp_path_beside(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    target.with_file_name(name)
}

/// Writes `bytes` to `temp`, flushes them to disk, then renames `temp` over `target`.
///
/// # Errors
///
/// Fails if any step fails; `target` is only replaced by the final rename.
fn write_then_rename(temp: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::link::LinkKey;

    fn lock_with_one_entry(content: &[u8]) -> Lockfile {
        let key = LinkKey {
            source: "docs/a.md"
                .parse()
                .expect("the fixture source should be valid"),
            target: "src/lib.rs"
                .parse()
                .expect("the fixture target should be valid"),
            symbol: None,
        };
        [(key, Hash::digest(content))].into_iter().collect()
    }

    fn lock_path(dir: &tempfile::TempDir) -> AbsolutePath {
        AbsolutePath::new(&dir.path().join("supdoc.lock"))
            .expect("a temp directory path should be absolute")
    }

    #[test]
    fn missing_lockfile_reads_as_none() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");

        let read = Lockfile::read(&lock_path(&dir)).expect("a missing file is not an error");

        assert_eq!(read, None);
    }

    #[test]
    fn write_then_read_returns_same_entries() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        let lock = lock_with_one_entry(b"x");

        lock.write(&path)
            .expect("writing to a temp directory should succeed");

        assert_eq!(
            Lockfile::read(&path).expect("the written lockfile should read back"),
            Some(lock)
        );
    }

    #[test]
    fn write_replaces_existing_file_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        lock_with_one_entry(b"old")
            .write(&path)
            .expect("the first write should succeed");
        let new = lock_with_one_entry(b"new");

        new.write(&path).expect("the second write should succeed");

        assert_eq!(
            Lockfile::read(&path).expect("it should read back"),
            Some(new)
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .expect("the directory should be listable")
            .map(|entry| {
                entry
                    .expect("a directory entry should be readable")
                    .file_name()
            })
            .collect();
        assert_eq!(
            names,
            ["supdoc.lock"],
            "only the lockfile should remain after a write"
        );
    }

    #[test]
    fn invalid_lockfile_text_reports_its_path_and_code() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        fs::write(path.as_path(), "version = 2\n").expect("the fixture should be writable");

        let error = Lockfile::read(&path).expect_err("a newer lockfile should be rejected");

        assert_eq!(error.code(), "lock/newer-version");
        assert!(
            error.to_string().starts_with(&path.display().to_string()),
            "the error should lead with the path, got: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_leaves_previous_lockfile_intact() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        let old = lock_with_one_entry(b"old");
        old.write(&path).expect("the first write should succeed");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555))
            .expect("the directory should become read-only");

        let result = lock_with_one_entry(b"new").write(&path);

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
            .expect("the directory should become writable again");
        let error = result.expect_err("writing into a read-only directory should fail");
        assert_eq!(error.code(), "lock/write");
        assert_eq!(
            Lockfile::read(&path).expect("the old lockfile should still read"),
            Some(old)
        );
    }
}
