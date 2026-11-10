//! Verifies a downloaded release archive and replaces the running binary with what it holds.
//!
//! Order matters: the archive's SHA-256 is checked against its `SHA256SUMS` line first, then the
//! one expected entry is read out in memory, and only then is anything written beside the
//! binary. A failure at any step leaves the old binary, and removes the temporary file.
//!
//! A running `supdoc.exe` cannot be overwritten on Windows, so there the old file is first
//! renamed to `supdoc.exe.old` and the new one moved in; both renames retry briefly on a
//! transient access denial. Known limit: if the process dies between those two renames, no
//! `supdoc.exe` is on disk, and `supdoc.exe.old` is the copy to rename back.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::config::confine::{self, Boundary, Escape};
use crate::hash::Hash;
use crate::output::text::visible;
use crate::path::{self, Component};

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
    #[error("{} has no parent directory", shown(.0))]
    NoDirectory(PathBuf),
    /// A path to write lies outside the binary's directory once symlinks are followed.
    #[error("{} lies outside the binary's directory", shown(.0))]
    Outside(PathBuf),
    /// The new binary could not be moved in, and the original could not be put back either.
    #[error(
        "could not replace {}: {replace}; and could not put the original back, it is at {}: {restore}",
        shown(.exe),
        shown(.aside)
    )]
    RestoreFailed {
        /// The binary being replaced.
        exe: PathBuf,
        /// Where the original is: the file it was moved aside to.
        aside: PathBuf,
        /// Why the new binary could not be moved in.
        replace: io::Error,
        /// Why the original could not be put back.
        restore: io::Error,
    },
    /// A file operation failed.
    #[error("{action} {}: {source}", shown(path))]
    Io {
        /// What was being done.
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The operating system's error.
        source: io::Error,
    },
}

/// `path` as text that cannot drive the terminal.
fn shown(path: &Path) -> String {
    visible(&path.display().to_string())
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

/// Replaces the file at `exe` with `binary`, mode 0755.
///
/// `exe` is the binary's real path. The new bytes go to a fresh file in the same directory and
/// are synced. On Unix the file is renamed over `exe` atomically; the running process keeps its
/// open image. On Windows a running `.exe` cannot be overwritten but can be renamed, so `exe`
/// is renamed aside to [`ASIDE_NAME`], the new file is moved to `exe`, and a failed move puts
/// the original back. Every write, rename and remove is admitted against that directory first.
/// On failure the temporary file is removed and `exe` is the original.
///
/// Effect: creates, renames and removes files beside `exe`.
///
/// # Errors
///
/// [`ReplaceError`] when `exe` has no directory, a path leaves the directory, or a file
/// operation fails.
pub(super) fn replace_exe(exe: &Path, binary: &[u8]) -> Result<(), ReplaceError> {
    replace_exe_with(exe, binary, cfg!(windows), confine::rename_into_retrying)
}

/// [`replace_exe`] with the way the file is put in place chosen (`aside`: move the old file
/// aside first, as Windows needs), and every rename supplied, so a failing rename can be tested
/// on any platform.
///
/// Effect: creates, renames and removes files beside `exe`.
///
/// # Errors
///
/// As [`replace_exe`].
fn replace_exe_with(
    exe: &Path,
    binary: &[u8],
    aside: bool,
    rename: impl Fn(&confine::Admitted<'_>, &confine::Admitted<'_>) -> io::Result<()>,
) -> Result<(), ReplaceError> {
    let dir = exe
        .parent()
        .ok_or_else(|| ReplaceError::NoDirectory(exe.to_path_buf()))?;
    let boundary = Boundary::resolve(dir);
    let temp_path = beside(dir, &temp_name())?;
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
        if aside {
            let aside_path = beside(dir, ASIDE_NAME)?;
            let aside = boundary
                .admit(&aside_path)
                .map_err(|escape| outside(&aside_path, escape))?;
            rename(&target, &aside).map_err(io_error("could not move aside", exe))?;
            rename(&temp, &target).map_err(|replace| match rename(&aside, &target) {
                Ok(()) => io_error("could not replace", exe)(replace),
                Err(restore) => ReplaceError::RestoreFailed {
                    exe: exe.to_path_buf(),
                    aside: aside_path.clone(),
                    replace,
                    restore,
                },
            })
        } else {
            rename(&temp, &target).map_err(io_error("could not replace", exe))
        }
    })();
    drop(file);
    if finished.is_err()
        && let Ok(temp) = boundary.admit(&temp_path)
    {
        drop(confine::remove_file(&temp));
    }
    finished
}

/// The prefix of the temporary files an upgrade creates beside the binary.
const TEMP_PREFIX: &str = ".supdoc-upgrade-";

/// The name the running `supdoc.exe` is moved aside to on Windows while its replacement goes in.
pub(super) const ASIDE_NAME: &str = "supdoc.exe.old";

/// `name` as a file directly in `dir`. Every name this module writes is one plain component, so
/// the refusal is never reached; it names `dir` as outside rather than writing anywhere else.
///
/// # Errors
///
/// Returns [`ReplaceError::Outside`] when `name` is not exactly one plain component.
fn beside(dir: &Path, name: &str) -> Result<PathBuf, ReplaceError> {
    Component::new(name)
        .map(|name| path::beneath(dir, &name))
        .ok_or_else(|| ReplaceError::Outside(dir.to_path_buf()))
}

/// A name for the temporary file that no earlier run is likely to have used.
fn temp_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format_temp_name(std::process::id(), nanos)
}

/// The temporary file name for process `pid` and `nanos`: [`TEMP_PREFIX`], `pid`, `-`, `nanos`.
///
/// [`is_leftover_name`] matches exactly this shape, so the two stay in step.
fn format_temp_name(pid: u32, nanos: u32) -> String {
    format!("{TEMP_PREFIX}{pid}-{nanos}")
}

/// Whether `text` is one or more ASCII digits and nothing else.
fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether `name` is exactly what [`format_temp_name`] makes, or exactly [`ASIDE_NAME`].
fn is_leftover_name(name: &str) -> bool {
    name == ASIDE_NAME
        || name
            .strip_prefix(TEMP_PREFIX)
            .and_then(|rest| rest.split_once('-'))
            .is_some_and(|(pid, nanos)| all_digits(pid) && all_digits(nanos))
}

/// Removes the files an earlier upgrade left beside `exe`, and says what it could not remove.
///
/// Only regular files named exactly like [`temp_name`]'s output (`.supdoc-upgrade-<digits>-<digits>`)
/// or [`ASIDE_NAME`] go: a symlink, a directory, any other name, and `exe` itself stay. Each
/// removal is admitted against `exe`'s directory first. A failed removal, an entry that cannot be
/// read and a directory that cannot be listed each come back as a note, never as an error.
///
/// Effect: lists the directory beside `exe` and removes the leftover files in it.
pub(super) fn remove_leftovers(exe: &Path) -> Vec<String> {
    remove_leftovers_with(exe, confine::remove_file)
}

/// [`remove_leftovers`] with the removal itself supplied, so a failing removal can be tested.
fn remove_leftovers_with(
    exe: &Path,
    remove: impl Fn(&confine::Admitted<'_>) -> io::Result<()>,
) -> Vec<String> {
    let Some(dir) = exe.parent() else {
        return Vec::new();
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            return vec![format!(
                "Could not look for files an earlier upgrade left in {}: {error}",
                shown(dir)
            )];
        }
    };
    let boundary = Boundary::resolve(dir);
    let mut notes = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                notes.push(format!(
                    "Could not read an entry of {} while looking for files an earlier upgrade \
                     left: {error}",
                    shown(dir)
                ));
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_name().to_str().is_some_and(is_leftover_name) || path == exe {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_file() => {}
            Ok(_) => continue,
            Err(error) => {
                notes.push(format!(
                    "Could not tell what {} is, so it was left: {error}",
                    shown(&path)
                ));
                continue;
            }
        }
        let removed = match boundary.admit(&path) {
            Ok(admitted) => remove(&admitted).map_err(|error| error.to_string()),
            Err(_) => Err("it lies outside the binary's directory".to_owned()),
        };
        if let Err(reason) = removed {
            notes.push(format!(
                "Could not remove {}, left by an earlier upgrade: {reason}",
                shown(&path)
            ));
        }
    }
    notes.sort();
    notes
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
    fn only_the_exact_leftover_shapes_are_leftover_names() {
        for name in [
            ".supdoc-upgrade-123-456",
            ".supdoc-upgrade-0-0",
            "supdoc.exe.old",
        ] {
            assert!(is_leftover_name(name), "{name} should be a leftover");
        }
        for name in [
            ".supdoc-upgrade-notes.txt",
            ".supdoc-upgrade-x",
            ".supdoc-upgrade-123",
            ".supdoc-upgrade-123-",
            ".supdoc-upgrade--456",
            ".supdoc-upgrade-123-456-789",
            ".supdoc-upgrade-123-456.bak",
            ".supdoc-upgrade-1a-456",
            "supdoc-upgrade",
            ".supdoc-upgrade",
            ".supdoc-upgrade-",
            "supdoc",
            "supdoc.exe",
            "supdoc.exe.old.bak",
        ] {
            assert!(!is_leftover_name(name), "{name} should not be a leftover");
        }
    }

    #[test]
    fn a_name_temp_name_makes_is_a_leftover_name() {
        assert!(is_leftover_name(&temp_name()));
        assert!(is_leftover_name(&format_temp_name(u32::MAX, 999_999_999)));
    }

    #[test]
    fn a_failed_removal_is_reported_as_a_note_and_the_rest_are_still_tried() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::write(&exe, "bin").expect("the binary should write");
        let first = dir.path().join(".supdoc-upgrade-1-1");
        let second = dir.path().join(".supdoc-upgrade-2-2");
        fs::write(&first, "a").expect("the first leftover should write");
        fs::write(&second, "b").expect("the second leftover should write");

        let notes = remove_leftovers_with(&exe, |admitted| {
            if admitted.as_path().ends_with(".supdoc-upgrade-1-1") {
                Err(io::Error::other("injected failure"))
            } else {
                confine::remove_file(admitted)
            }
        });

        assert_eq!(notes.len(), 1, "one removal failed: {notes:?}");
        let note = notes.concat();
        assert!(
            note.contains(".supdoc-upgrade-1-1") && note.contains("injected failure"),
            "{notes:?}"
        );
        assert!(first.exists(), "the failed one stays");
        assert!(!second.exists(), "the next one is still removed");
    }

    #[test]
    fn a_directory_with_a_leftover_name_is_left_without_a_note() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::write(&exe, "bin").expect("the binary should write");
        let folder = dir.path().join(".supdoc-upgrade-1-1");
        fs::create_dir(&folder).expect("the directory should create");

        let notes = remove_leftovers_with(&exe, |_| Err(io::Error::other("must not be called")));

        assert!(
            notes.is_empty(),
            "a directory is skipped silently: {notes:?}"
        );
        assert!(folder.is_dir());
    }

    /// A rename that does the real move except on the calls numbered in `failing` (1-based),
    /// which fail with an error naming the call.
    fn failing_calls(
        failing: &'static [usize],
    ) -> impl Fn(&confine::Admitted<'_>, &confine::Admitted<'_>) -> io::Result<()> {
        let calls = std::cell::Cell::new(0_usize);
        move |from, to| {
            calls.set(calls.get() + 1);
            if failing.contains(&calls.get()) {
                Err(io::Error::other(format!(
                    "injected failure {}",
                    calls.get()
                )))
            } else {
                confine::rename_into(from, to)
            }
        }
    }

    #[test]
    fn a_failed_move_in_after_moving_aside_restores_the_original_and_removes_the_temp_file() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::write(&exe, "old").expect("the binary should write");

        let result = replace_exe_with(&exe, b"new binary", true, failing_calls(&[2]));

        assert!(
            matches!(
                &result,
                Err(ReplaceError::Io {
                    action: "could not replace",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(fs::read(&exe).expect("the original should read"), b"old");
        assert_eq!(
            names_in(dir.path()),
            ["supdoc"],
            "no aside or temp file may remain"
        );
    }

    #[test]
    fn a_failed_move_in_and_a_failed_restore_name_both_causes_and_both_paths() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::write(&exe, "old").expect("the binary should write");

        let result = replace_exe_with(&exe, b"new binary", true, failing_calls(&[2, 3]));

        let Err(error @ ReplaceError::RestoreFailed { .. }) = result else {
            panic!("expected a restore failure, got {result:?}");
        };
        let text = error.to_string();
        assert!(text.contains("injected failure 2"), "{text}");
        assert!(text.contains("injected failure 3"), "{text}");
        assert!(text.contains("supdoc"), "{text}");
        assert!(text.contains(ASIDE_NAME), "{text}");
        assert_eq!(
            fs::read(dir.path().join(ASIDE_NAME)).expect("the original should be aside"),
            b"old"
        );
    }

    #[test]
    fn moving_aside_then_in_leaves_the_new_binary_and_the_old_one_aside() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::write(&exe, "old").expect("the binary should write");

        replace_exe_with(&exe, b"new binary", true, confine::rename_into)
            .expect("the replacement should succeed");

        assert_eq!(
            fs::read(&exe).expect("the new binary should read"),
            b"new binary"
        );
        assert_eq!(
            fs::read(dir.path().join(ASIDE_NAME)).expect("the aside file should read"),
            b"old"
        );
        assert_eq!(names_in(dir.path()), ["supdoc", ASIDE_NAME]);
    }

    #[test]
    fn a_failed_move_aside_leaves_the_original_in_place() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::write(&exe, "old").expect("the binary should write");
        let blocker = dir.path().join(ASIDE_NAME);
        fs::create_dir(&blocker).expect("the blocking directory should create");
        fs::write(blocker.join("keep"), "x").expect("the blocker content should write");

        let result = replace_exe_with(&exe, b"new binary", true, confine::rename_into);

        assert!(
            matches!(
                &result,
                Err(ReplaceError::Io {
                    action: "could not move aside",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(fs::read(&exe).expect("the original should read"), b"old");
        assert_eq!(names_in(dir.path()), ["supdoc", ASIDE_NAME]);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("the directory should list")
            .map(|entry| {
                entry
                    .expect("the entry should read")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file_and_keeps_the_target() {
        let dir = tempfile::tempdir().expect("a scratch directory should be creatable");
        let exe = dir.path().join("supdoc");
        fs::create_dir(&exe).expect("the stand-in target should be creatable");
        fs::write(exe.join("keep"), "old").expect("the stand-in content should write");

        let result = replace_exe_with(&exe, b"new binary", false, confine::rename_into);

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
