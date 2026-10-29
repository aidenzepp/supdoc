//! Reading and atomically writing `supdoc.lock` (02 L9).

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
        let to_error = |error| LockError::Write {
            path: path.clone(),
            error,
        };
        let (file, temp) =
            create_exclusive_temp(|attempt| temp_path_beside(path.as_path(), attempt))
                .map_err(to_error)?;
        write_then_rename(file, &temp, path.as_path(), self.render().as_bytes()).map_err(|error| {
            drop(fs::remove_file(&temp));
            to_error(error)
        })
    }
}

/// How many fresh names to try before giving up on creating a temporary file.
const TEMP_ATTEMPTS: u32 = 8;

/// A scratch name next to `target`, so the final rename stays on one filesystem. The name mixes
/// the process id, a process-wide counter, the clock, and the attempt number so it cannot be
/// guessed and pre-planted.
fn temp_path_beside(target: &Path, attempt: u32) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.{nanos:x}.{attempt}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    target.with_file_name(name)
}

/// Creates a new file at the first free candidate path. `create_new` refuses an existing path
/// and never follows a symlink, so a pre-placed file or link is skipped, not written through.
///
/// # Errors
///
/// Fails on any error other than `AlreadyExists`, or when every attempt finds its path taken.
fn create_exclusive_temp(
    mut candidate: impl FnMut(u32) -> PathBuf,
) -> io::Result<(fs::File, PathBuf)> {
    for attempt in 0..TEMP_ATTEMPTS {
        let temp = candidate(attempt);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => return Ok((file, temp)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no unused temporary file name found after {TEMP_ATTEMPTS} attempts"),
    ))
}

/// Writes `bytes` to the open `file`, flushes them to disk, then renames `temp` over `target`.
///
/// # Errors
///
/// Fails if any step fails; `target` is only replaced by the final rename.
fn write_then_rename(
    mut file: fs::File,
    temp: &Path,
    target: &Path,
    bytes: &[u8],
) -> io::Result<()> {
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

    #[cfg(unix)]
    #[test]
    fn preplaced_file_or_symlink_at_a_candidate_name_is_never_written_through() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let victim = dir.path().join("victim");
        fs::write(&victim, "precious").expect("the victim should be writable");
        let planted_file = dir.path().join("planted-file");
        fs::write(&planted_file, "planted").expect("the planted file should be writable");
        let planted_link = dir.path().join("planted-link");
        std::os::unix::fs::symlink(&victim, &planted_link)
            .expect("the symlink should be creatable");
        let free = dir.path().join("free");
        let candidates = [planted_file.clone(), planted_link.clone(), free.clone()];

        let mut candidates = candidates.into_iter();
        let (mut file, chosen) = create_exclusive_temp(|_| {
            candidates
                .next()
                .expect("the free candidate should be reached before candidates run out")
        })
        .expect("the free candidate should be used after the taken ones are refused");
        file.write_all(b"new")
            .expect("the new file should be writable");

        assert_eq!(chosen, free);
        assert_eq!(
            fs::read_to_string(&victim).expect("the victim should be readable"),
            "precious",
            "the symlink target must not be written through"
        );
        assert_eq!(
            fs::read_to_string(&planted_file).expect("the planted file should be readable"),
            "planted",
            "the pre-existing file must not be overwritten"
        );
    }

    #[test]
    fn exhausted_candidates_fail_instead_of_overwriting() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let taken = dir.path().join("taken");
        fs::write(&taken, "planted").expect("the planted file should be writable");

        let error = create_exclusive_temp(|_| taken.clone())
            .expect_err("every attempt finds the path taken");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(&taken).expect("the planted file should be readable"),
            "planted"
        );
    }
}
