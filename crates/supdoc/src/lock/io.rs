//! Reading and atomically writing `supdoc.lock` (02 L9).

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::error::LockError;
use super::model::{LOCK_FILE_NAME, Lockfile};
use crate::limits::SizeLimit;
use crate::path::{AbsolutePath, ProjectFile, is_regular_file, resolve_project_file};

impl Lockfile {
    /// Reads and parses the lockfile at `path`. `Ok(None)` if the file doesn't exist; the
    /// command decides what that means.
    ///
    /// The project root is `path`'s directory. A lockfile that is a symlink is read only if the
    /// file it leads to is named `supdoc.lock` and lies inside that root once symlinks are
    /// followed (decision 6), so a hostile checkout can't make supdoc print another file's text
    /// in a parse error.
    ///
    /// # Errors
    ///
    /// `OutsideRoot` for a symlink that doesn't lead to a `supdoc.lock` inside the root, whether
    /// or not its target exists. `NotRegularFile` for a directory, FIFO or other non-file, refused
    /// before it is opened so a FIFO can't block the read. `TooLarge` for a file over `limit`
    /// (`max-lock-size`), measured on the open file before any of it is read, and never read past
    /// one byte over `limit` should it grow meanwhile. Otherwise fails if the file exists but
    /// cannot be read, or its text is not a valid lockfile.
    pub(crate) fn read(path: &AbsolutePath, limit: SizeLimit) -> Result<Option<Self>, LockError> {
        let read_failed = |error| LockError::Read {
            path: path.clone(),
            error,
        };
        let file = match locate(path).map_err(read_failed)? {
            Located::Absent => return Ok(None),
            Located::Inside(file) => file,
            Located::Outside => return Err(LockError::OutsideRoot { path: path.clone() }),
        };
        match is_regular_file(&file) {
            Ok(true) => {}
            Ok(false) => return Err(LockError::NotRegularFile { path: path.clone() }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(read_failed(error)),
        }
        let opened = match fs::File::open(&file) {
            Ok(opened) => opened,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(read_failed(error)),
        };
        let size = opened.metadata().map_err(read_failed)?.len();
        let too_large = |grew| LockError::TooLarge {
            path: path.clone(),
            size,
            limit,
            grew,
        };
        if limit.get().is_some_and(|max| size > max) {
            return Err(too_large(false));
        }
        // One byte past the ceiling is enough to tell a file that grew after the measure. The
        // length is checked before decoding, so a file cut off mid-character by the ceiling is
        // still reported as too large, never as unreadable.
        let ceiling = limit.get().map_or(u64::MAX, |max| max.saturating_add(1));
        let mut bytes = Vec::new();
        let read = opened
            .take(ceiling)
            .read_to_end(&mut bytes)
            .map_err(read_failed)?;
        let read = u64::try_from(read).unwrap_or(u64::MAX);
        if limit.get().is_some_and(|max| read > max) {
            return Err(too_large(true));
        }
        let text = String::from_utf8(bytes)
            .map_err(|error| read_failed(io::Error::new(io::ErrorKind::InvalidData, error)))?;
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
    /// A `path` that is a symlink to a `supdoc.lock` inside the root is written through: the real
    /// file [`Lockfile::read`] reads gets the new text and the symlink stays.
    ///
    /// # Errors
    ///
    /// `OutsideRoot` for a symlink that doesn't lead to a `supdoc.lock` inside the root, so no
    /// write ever lands outside it. Otherwise fails if the temporary file cannot be written or
    /// moved into place. Any existing lockfile is left unchanged.
    pub(crate) fn write(&self, path: &AbsolutePath) -> Result<(), LockError> {
        let to_error = |error| LockError::Write {
            path: path.clone(),
            error,
        };
        let target = match locate(path).map_err(to_error)? {
            Located::Absent => path.as_path().to_path_buf(),
            Located::Inside(file) => file,
            Located::Outside => return Err(LockError::OutsideRoot { path: path.clone() }),
        };
        write_atomically(&target, self.render().as_bytes(), None).map_err(to_error)
    }
}

/// Where the lockfile at a path really is, the one file reading and writing it both use.
enum Located {
    /// Nothing is there.
    Absent,
    /// The file to read and write: the path itself, or the real file a symlink there leads to
    /// inside the project root.
    Inside(PathBuf),
    /// A symlink that doesn't lead to a `supdoc.lock` inside the root: dangling, looping,
    /// pointing outside or at a file of another name, deliberately not told apart.
    Outside,
}

/// Finds the file to read or write for the lockfile at `path`, whose directory is the project
/// root.
///
/// Effect: reads metadata and symlinks along `path`.
///
/// # Errors
///
/// Fails if `path`'s own metadata can't be read for a reason other than its absence.
fn locate(path: &AbsolutePath) -> io::Result<Located> {
    Ok(match resolve_project_file(path)? {
        ProjectFile::Absent => Located::Absent,
        // The path itself when it isn't a symlink; otherwise its real file must be a lockfile.
        ProjectFile::Inside(file) if file == *path || is_named_lockfile(file.as_path()) => {
            Located::Inside(file.as_path().to_path_buf())
        }
        ProjectFile::Inside(_) | ProjectFile::Outside(_) => Located::Outside,
    })
}

/// Whether `real` is named `supdoc.lock`, so a symlink to another in-root file such as `.env`
/// is never parsed as a lockfile and quoted in the error.
fn is_named_lockfile(real: &Path) -> bool {
    real.file_name().is_some_and(|name| name == LOCK_FILE_NAME)
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

/// Writes `bytes` to a new file beside `target` and renames it into place, so an interrupted
/// run never leaves half a file. With `permissions`, the new file gets them before the rename;
/// without, it keeps the default for a new file. The temporary file is created exclusively, so a
/// planted path or symlink is never written through.
///
/// # Errors
///
/// Fails if the temporary file cannot be created, written or renamed; `target` is then unchanged.
pub(crate) fn write_atomically(
    target: &Path,
    bytes: &[u8],
    permissions: Option<&fs::Permissions>,
) -> io::Result<()> {
    let (mut file, temp) = create_exclusive_temp(|attempt| temp_path_beside(target, attempt))?;
    let written = file
        .write_all(bytes)
        .and_then(|()| permissions.map_or(Ok(()), |kept| file.set_permissions(kept.clone())))
        .and_then(|()| file.sync_all());
    drop(file);
    let finished = written.and_then(|()| fs::rename(&temp, target));
    if finished.is_err() {
        drop(fs::remove_file(&temp));
    }
    finished
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::limits::Limits;
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

        let read = Lockfile::read(&lock_path(&dir), Limits::MAX_LOCK_SIZE)
            .expect("a missing file is not an error");

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
            Lockfile::read(&path, Limits::MAX_LOCK_SIZE)
                .expect("the written lockfile should read back"),
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
            Lockfile::read(&path, Limits::MAX_LOCK_SIZE).expect("it should read back"),
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

        let error = Lockfile::read(&path, Limits::MAX_LOCK_SIZE)
            .expect_err("a newer lockfile should be rejected");

        assert_eq!(error.code(), "lock-newer-version");
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
        assert_eq!(error.code(), "lock-write");
        assert_eq!(
            Lockfile::read(&path, Limits::MAX_LOCK_SIZE)
                .expect("the old lockfile should still read"),
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
    fn directory_at_the_lockfile_path_is_refused_as_not_a_regular_file() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        fs::create_dir(path.as_path()).expect("the directory should be creatable");

        let error = Lockfile::read(&path, Limits::MAX_LOCK_SIZE)
            .expect_err("a directory should be refused");

        assert!(
            matches!(error, LockError::NotRegularFile { .. }),
            "got: {error}"
        );
        assert_eq!(error.code(), "lock-read");
        assert_eq!(error.status(), crate::exit::ExitStatus::Io);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_at_the_lockfile_path_is_refused_without_opening_it() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        let made = std::process::Command::new("mkfifo")
            .arg(path.as_path())
            .status()
            .expect("mkfifo should run");
        assert!(made.success(), "mkfifo should create the FIFO");

        let error = Lockfile::read(&path, Limits::MAX_LOCK_SIZE)
            .expect_err("a FIFO should be refused, not opened");

        assert!(
            matches!(error, LockError::NotRegularFile { .. }),
            "got: {error}"
        );
    }

    #[test]
    fn non_utf8_lockfile_is_a_read_error_whose_help_has_no_chmod() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        fs::write(path.as_path(), b"version = 1\n# caf\xe9\n")
            .expect("the fixture should be writable");

        let error = Lockfile::read(&path, Limits::MAX_LOCK_SIZE)
            .expect_err("Latin-1 text should be refused");
        let help = error.help_from(&path.parent().expect("the lockfile should have a parent"));

        assert_eq!(error.code(), "lock-read");
        assert!(
            help.contains("not UTF-8") && !help.contains("chmod"),
            "got: {help}"
        );
    }

    #[test]
    fn lockfile_over_max_lock_size_is_refused_with_its_size() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        lock_with_one_entry(b"x")
            .write(&path)
            .expect("writing to a temp directory should succeed");
        let size = fs::metadata(path.as_path())
            .expect("the lockfile should have metadata")
            .len();
        let limit: SizeLimit = "16B".parse().expect("16B should be a size");

        let error = Lockfile::read(&path, limit).expect_err("a lock over 16 B should be refused");

        assert!(
            matches!(error, LockError::TooLarge { size: measured, grew: false, .. } if measured == size),
            "got: {error}"
        );
        let help = error.help_from(&path.parent().expect("the lockfile should have a parent"));
        for named in [
            "max-lock-size",
            "SUPDOC_MAX_LOCK_SIZE",
            "--max-lock-size",
            &size.to_string(),
        ] {
            assert!(
                help.contains(named),
                "the help should name {named}, got: {help}"
            );
        }
        assert_eq!(
            (error.code(), error.status()),
            ("lock-too-large", crate::exit::ExitStatus::Config)
        );
    }

    #[test]
    fn lockfile_at_max_lock_size_or_with_no_limit_reads() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let path = lock_path(&dir);
        let lock = lock_with_one_entry(b"x");
        lock.write(&path)
            .expect("writing to a temp directory should succeed");
        let size = fs::metadata(path.as_path())
            .expect("the lockfile should have metadata")
            .len();
        let exact: SizeLimit = format!("{size}B").parse().expect("the size should parse");
        let none: SizeLimit = "0".parse().expect("0 should be no limit");

        let read = (Lockfile::read(&path, exact), Lockfile::read(&path, none));

        assert!(
            matches!(&read, (Ok(Some(a)), Ok(Some(b))) if *a == lock && *b == lock),
            "a lock exactly at the limit, and one under no limit, should read: {read:?}"
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

    #[cfg(unix)]
    #[test]
    fn write_through_an_in_root_symlink_updates_the_real_file_and_keeps_the_link() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let real = dir.path().join("state/supdoc.lock");
        fs::create_dir_all(dir.path().join("state")).expect("the state dir should be creatable");
        lock_with_one_entry(b"old")
            .write(&AbsolutePath::new(&real).expect("the real path should be absolute"))
            .expect("the real lockfile should be writable");
        let path = lock_path(&dir);
        std::os::unix::fs::symlink("state/supdoc.lock", path.as_path())
            .expect("the symlink should be creatable");
        let new = lock_with_one_entry(b"new");

        new.write(&path)
            .expect("writing through the in-root symlink should succeed");

        assert!(
            fs::symlink_metadata(path.as_path())
                .expect("the link should still exist")
                .file_type()
                .is_symlink(),
            "the symlink must stay a symlink"
        );
        assert_eq!(
            Lockfile::read(
                &AbsolutePath::new(&real).expect("the real path should be absolute"),
                Limits::MAX_LOCK_SIZE
            )
            .expect("the real file should read back"),
            Some(new),
            "the real file gets the new text"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_refuses_a_symlink_out_of_the_root_and_leaves_its_target_alone() {
        let project = tempfile::tempdir().expect("a temp directory should be creatable");
        let outside = tempfile::tempdir().expect("a temp directory should be creatable");
        let victim = outside.path().join("supdoc.lock");
        fs::write(&victim, "victim").expect("the outside file should be writable");
        let path = lock_path(&project);
        std::os::unix::fs::symlink(&victim, path.as_path())
            .expect("the symlink should be creatable");

        let written = lock_with_one_entry(b"x").write(&path);

        assert!(
            matches!(written, Err(LockError::OutsideRoot { .. })),
            "a symlink out of the root should be refused: {written:?}"
        );
        assert_eq!(
            fs::read_to_string(&victim).expect("the outside file should be readable"),
            "victim"
        );
    }
}
