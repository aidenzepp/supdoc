//! Where a path really lies once symlinks are followed, so supdoc can stay inside the project root.
//!
//! Outside the root, supdoc reads only its own inputs: a `supdoc.config.toml` reached through
//! `extends`, or a project config symlinked to an outside file of that name, whose lines can appear
//! in config errors; the ignore files in the directories above the root, up to the top of its git
//! work tree; the global excludes file; and a `.git/info/exclude` reached through a `.git` symlink,
//! a symlinked `.git/info` or `info/exclude`, or a `.git` file's `gitdir` and `commondir`. To find
//! the global excludes file it reads the git config files that can name it: `~/.gitconfig` and
//! `$XDG_CONFIG_HOME/git/config` (or the file `GIT_CONFIG_GLOBAL` names), and `/etc/gitconfig` (or
//! the file `GIT_CONFIG_SYSTEM` names). It never quotes a rule line from an ignore file outside the
//! root, or a line from a git config. It reads, hashes, lists symbols from and quotes no other file
//! outside the root, linked files included; a tool it runs on a code block may still read its own
//! config files there.
//!
//! It doesn't promise to hide whether a path outside the root exists: an out-and-back symlink can
//! change which error or exit code appears. That is accepted, since a pull request on ordinary
//! `pull_request` CI can already run commands.
//!
//! Every write, rename, delete and directory create goes through one [`Boundary::admit`] against
//! the directory that owns it, checked at the call. The write primitives here take the
//! [`Admitted`] path it returns, not a `&Path`, so a write that skips the check doesn't compile.
//! The check narrows the race between deciding to write and writing to the gap between `admit`
//! and the call; it doesn't close it.
//!
//! The `mode` the create primitives take is a Unix permission mode, applied on Unix only. On
//! Windows it is ignored: a new file or directory inherits its parent's ACL, and supdoc's private
//! places sit under the user's own profile or cache directory, whose default ACL admits only the
//! owner, SYSTEM and Administrators. The limit: a parent directory that is shared gives a shared
//! ACL, and nothing here narrows it.

use std::fs;
use std::io::{self, Write as _};
use std::marker::PhantomData;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::path::{AbsolutePath, Pointer, Suffix, rebase, with_suffix};

/// How many dangling symlinks one lookup follows before it gives up, as the OS limits loops.
const MAX_DANGLING_HOPS: u8 = 40;

/// The real location of `path`: every symlink along it followed, with any missing tail appended.
///
/// A dangling symlink is followed to where it points, so a link to a missing file outside the
/// root still lands outside. `None` when that can't be worked out (a symlink loop, or a `..`
/// below a missing name); callers treat that as outside.
///
/// Effect: reads file metadata and symlinks along `path`.
pub(crate) fn real_location(path: &Path) -> Option<PathBuf> {
    real_location_within(path, MAX_DANGLING_HOPS)
}

/// [`real_location`], following at most `hops` more dangling symlinks.
fn real_location_within(path: &Path, hops: u8) -> Option<PathBuf> {
    if let Ok(real) = fs::canonicalize(path) {
        return Some(real);
    }
    // Climb to the deepest entry that exists; what lies below it is the missing tail.
    let mut existing = path;
    while fs::symlink_metadata(existing).is_err() {
        existing = existing.parent()?;
    }
    let real_existing = if let Ok(real) = fs::canonicalize(existing) {
        real
    } else {
        // The entry exists but can't be resolved: a dangling symlink. Follow it by hand.
        let next = hops.checked_sub(1)?;
        let pointee = Pointer::from_path(fs::read_link(existing).ok()?)?;
        let parent = fs::canonicalize(existing.parent()?).ok()?;
        real_location_within(&pointee.resolve_from(&parent), next)?
    };
    if path == existing {
        return Some(real_existing);
    }
    // `rebase` refuses a `..` or root in the tail, so a `..` below a missing name is `None`.
    rebase(path, existing, &real_existing)
}

/// A directory supdoc may write beneath, with its real location worked out once.
#[derive(Debug, Clone)]
pub(crate) struct Boundary {
    /// The directory, or `None` when [`Boundary::resolve`] refused it: then nothing is admitted.
    dir: Option<Dir>,
}

/// The directory of a [`Boundary`] that [`Boundary::resolve`] accepted.
#[derive(Debug, Clone)]
struct Dir {
    /// The directory exactly as given: absolute, with no `..`.
    given: PathBuf,
    /// `given` with every symlink followed, or `given` as written when that can't be worked out.
    real: PathBuf,
    /// `given` with every symlink followed, or `None` when that can't be worked out. Only
    /// [`Boundary::real_inside`] reads it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used once its #371 callers land")
    )]
    strict: Option<PathBuf>,
}

/// A path [`Boundary::admit`] passed: the only argument the write primitives take.
///
/// The check holds when it is made, not after: a symlink swapped in between `admit` and the write
/// can still redirect it. Admit immediately before each write to keep that gap small.
#[derive(Debug)]
pub(crate) struct Admitted<'b> {
    /// The path to write, exactly as admitted.
    path: PathBuf,
    /// Ties the admission to the boundary it was checked against.
    boundary: PhantomData<&'b Boundary>,
}

/// Why [`Boundary::admit`] refused a path, so each caller can keep its own error code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Escape {
    /// The path isn't beneath the boundary as written: it is relative, holds a `..`, or names an
    /// absolute path elsewhere; or the boundary itself was relative or held a `..`.
    Lexical,
    /// The path is written beneath the boundary, but a symlink along it leads out. `real` is where
    /// it really lies, or `None` when that can't be worked out.
    Real {
        /// The path's real location, when known.
        real: Option<PathBuf>,
    },
}

/// Why [`Boundary::real_inside`] refused a path, with the boundary's failure kept apart from the
/// path's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used once its #371 callers land")
)]
pub(crate) enum ReadEscape {
    /// The boundary itself was refused by [`Boundary::resolve`] or its real location can't be
    /// worked out, so no path can be judged against it.
    BoundaryUnresolved,
    /// The path's real location can't be worked out: it is missing, a dangling symlink, or a
    /// symlink loop.
    Unresolved,
    /// The path resolves, but outside the boundary.
    Outside {
        /// The path's real location.
        real: PathBuf,
    },
}

impl Boundary {
    /// The boundary at `dir`, its real location worked out now.
    ///
    /// `dir` is taken exactly as written, never folded: a relative `dir`, or one holding a `..`,
    /// is refused, and the boundary then admits nothing ([`Escape::Lexical`]) and contains
    /// nothing. Folding `..` lexically would name a different directory from the one the kernel
    /// reaches when the `..` follows a symlink. When `dir`'s real location can't be worked out,
    /// `dir` as written stands for it.
    ///
    /// Effect: reads file metadata and symlinks along `dir`.
    #[must_use]
    pub(crate) fn resolve(dir: &(impl AsRef<Path> + ?Sized)) -> Self {
        let dir = dir.as_ref();
        let accepted = plain(dir).then(|| Dir {
            given: dir.to_path_buf(),
            real: real_location(dir).unwrap_or_else(|| dir.to_path_buf()),
            strict: fs::canonicalize(dir).ok(),
        });
        Self { dir: accepted }
    }

    /// The directory as given, or an empty path when [`Boundary::resolve`] refused it. A path
    /// joined onto the empty path is relative, which [`Boundary::admit`] refuses, so a refused
    /// boundary still writes nothing.
    #[must_use]
    pub(crate) fn dir(&self) -> &Path {
        self.dir
            .as_ref()
            .map_or(Path::new(""), |dir| dir.given.as_path())
    }

    /// Whether `path` lies inside the boundary once every symlink along it is followed.
    ///
    /// A dangling symlink counts where it points, and a path whose real location can't be worked
    /// out counts as outside. A refused boundary contains nothing.
    ///
    /// Effect: reads file metadata and symlinks along `path`.
    #[must_use]
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.dir
            .as_ref()
            .is_some_and(|dir| real_location(path).is_some_and(|real| real.starts_with(&dir.real)))
    }

    /// The real location of `path`, once checked to lie inside the boundary: the strict read
    /// check. It refuses exactly what `canonicalize(root)`, `canonicalize(path)` and `starts_with`
    /// refuse, so a missing path or a dangling symlink is refused even where
    /// [`Boundary::contains`] counts it inside.
    ///
    /// Effect: reads file metadata and symlinks along the boundary's directory and `path`.
    ///
    /// # Errors
    ///
    /// [`ReadEscape::BoundaryUnresolved`] when the boundary was refused or its directory can't be
    /// resolved; [`ReadEscape::Unresolved`] when `path` can't be; [`ReadEscape::Outside`] naming
    /// where it lies when that is outside the boundary.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used once its #371 callers land")
    )]
    pub(crate) fn real_inside(&self, path: &Path) -> Result<PathBuf, ReadEscape> {
        let root = self
            .dir
            .as_ref()
            .and_then(|dir| dir.strict.as_deref())
            .ok_or(ReadEscape::BoundaryUnresolved)?;
        let real = fs::canonicalize(path).map_err(|_| ReadEscape::Unresolved)?;
        if real.starts_with(root) {
            Ok(real)
        } else {
            Err(ReadEscape::Outside { real })
        }
    }

    /// `target`, once it is checked to lie inside the boundary both as written and once every
    /// symlink along it is followed. A missing tail counts where it would be.
    ///
    /// `target` is taken exactly as written, never folded, and the admitted path is that same
    /// path, so a caller that reads by it and writes by the admitted one names one file.
    /// Nothing is cached: call it immediately before the write it guards.
    ///
    /// Effect: reads file metadata and symlinks along `target`.
    ///
    /// # Errors
    ///
    /// [`Escape::Lexical`] when `target` is relative, holds a `..`, or isn't beneath the boundary
    /// as given, or when [`Boundary::resolve`] refused the boundary. [`Escape::Real`] when it is
    /// beneath, but its real location lies outside the boundary's or can't be worked out.
    pub(crate) fn admit(
        &self,
        target: &(impl AsRef<Path> + ?Sized),
    ) -> Result<Admitted<'_>, Escape> {
        let target = target.as_ref();
        let Some(dir) = self.dir.as_ref() else {
            return Err(Escape::Lexical);
        };
        if !plain(target) || !target.starts_with(&dir.given) {
            return Err(Escape::Lexical);
        }
        match real_location(target) {
            Some(real) if real.starts_with(&dir.real) => Ok(Admitted {
                path: target.to_path_buf(),
                boundary: PhantomData,
            }),
            real => Err(Escape::Real { real }),
        }
    }

    /// [`Boundary::admit`] for `real`, a file whose symlinks were already followed, such as the
    /// real lockfile or config an in-root symlink leads to. When `real` is named beneath the
    /// boundary's real location rather than as given, the same file is admitted named beneath
    /// the boundary as given, so a root reached through a symlink still admits its own files.
    ///
    /// Effect: reads file metadata and symlinks along `real`.
    ///
    /// # Errors
    ///
    /// As [`Boundary::admit`]; [`Escape::Real`] naming `real` when it lies beneath neither.
    pub(crate) fn admit_resolved(&self, real: &AbsolutePath) -> Result<Admitted<'_>, Escape> {
        let Some(dir) = self.dir.as_ref() else {
            return Err(Escape::Lexical);
        };
        let real = real.as_path();
        if real.starts_with(&dir.given) {
            return self.admit(real);
        }
        let named = rebase(real, &dir.real, &dir.given).ok_or_else(|| Escape::Real {
            real: Some(real.to_path_buf()),
        })?;
        self.admit(&named)
    }
}

/// Whether `path` is absolute and holds no `..`, so the directory it names as written is the one
/// the kernel reaches.
fn plain(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| component == Component::ParentDir)
}

impl Admitted<'_> {
    /// The admitted path, for messages and reads.
    #[must_use]
    pub(crate) fn as_path(&self) -> &Path {
        &self.path
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
    let tail = format!(
        ".{}.{}.{nanos:x}.{attempt}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    // A target with no file name is a root or ends in `..`, which no write reaches; it falls
    // back to the target itself, which `create_new` then refuses.
    Suffix::new(&tail)
        .and_then(|tail| with_suffix(target, &tail))
        .unwrap_or_else(|| target.to_path_buf())
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

/// Renames to try in all when another thread is replacing the same target (Windows only).
const RENAME_ATTEMPTS: u32 = 10;

/// Pause between those renames; ten tries wait at most 180 ms in all.
const RENAME_PAUSE: Duration = Duration::from_millis(20);

/// Windows `ERROR_ACCESS_DENIED`, which std maps to `PermissionDenied`.
const ERROR_ACCESS_DENIED: i32 = 5;

/// Windows `ERROR_SHARING_VIOLATION`, which std leaves uncategorized.
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Whether a failed rename, `tried` renames in, is worth another go. Only on Windows, where a
/// rename onto a file that a concurrent replace holds open fails with `PermissionDenied` or
/// with raw OS error 5 (access denied) or 32 (sharing violation), and only while attempts
/// remain. Any other error means the rename can't succeed by waiting.
fn rename_is_worth_retrying(error: &io::Error, tried: u32, windows: bool) -> bool {
    windows
        && tried < RENAME_ATTEMPTS
        && (error.kind() == io::ErrorKind::PermissionDenied
            || matches!(
                error.raw_os_error(),
                Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
            ))
}

/// Runs `rename`, rerunning it after `pause(RENAME_PAUSE)` while [`rename_is_worth_retrying`] says so, and
/// returns the last result.
///
/// Effect: runs `rename` and sleeps through `pause`.
///
/// # Errors
///
/// The last error `rename` returned.
fn rename_retrying(
    mut rename: impl FnMut() -> io::Result<()>,
    windows: bool,
    mut pause: impl FnMut(Duration),
) -> io::Result<()> {
    let mut tried = 1;
    loop {
        match rename() {
            Err(error) if rename_is_worth_retrying(&error, tried, windows) => {
                pause(RENAME_PAUSE);
                tried += 1;
            }
            result => return result,
        }
    }
}

/// Writes `bytes` to a new file beside `target` and renames it into place, so an interrupted
/// run never leaves half a file. With `permissions`, the new file gets them before the rename;
/// without, it keeps the default for a new file. The temporary file is created exclusively, so a
/// planted path or symlink is never written through, and it shares `target`'s admitted parent.
///
/// Effect: creates, writes and renames a file beside `target`.
///
/// # Errors
///
/// Fails if the temporary file cannot be created, written or renamed; `target` is then unchanged.
pub(crate) fn write_atomically(
    target: &Admitted<'_>,
    bytes: &[u8],
    permissions: Option<&fs::Permissions>,
) -> io::Result<()> {
    let target = target.path.as_path();
    let (mut file, temp) = create_exclusive_temp(|attempt| temp_path_beside(target, attempt))?;
    let written = file
        .write_all(bytes)
        .and_then(|()| permissions.map_or(Ok(()), |kept| file.set_permissions(kept.clone())))
        .and_then(|()| file.sync_all());
    drop(file);
    let finished = written
        .and_then(|()| rename_retrying(|| fs::rename(&temp, target), cfg!(windows), thread::sleep));
    if finished.is_err() {
        drop(fs::remove_file(&temp));
    }
    finished
}

/// Creates `dir` and any missing parents, each new one with `mode` (Unix only; ignored on Windows).
///
/// Effect: creates directories.
///
/// # Errors
///
/// Fails when a directory can't be created, or `dir` exists and isn't a directory.
#[cfg_attr(
    not(unix),
    expect(
        unused_variables,
        reason = "`mode` is a Unix permission mode, ignored on Windows"
    )
)]
pub(crate) fn create_dir_all(dir: &Admitted<'_>, mode: u32) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    let builder = builder.recursive(true);
    #[cfg(unix)]
    let builder = builder.mode(mode);
    builder.create(&dir.path)
}

/// Creates the new directory `dir` with `mode` (Unix only; ignored on Windows). Its parent must
/// exist. Refuses an existing path, and never follows a symlink there, so the caller that creates
/// it owns it alone.
///
/// Effect: creates a directory.
///
/// # Errors
///
/// `AlreadyExists` when anything is at `dir`; otherwise fails when it can't be created.
#[cfg_attr(
    not(unix),
    expect(
        unused_variables,
        reason = "`mode` is a Unix permission mode, ignored on Windows"
    )
)]
pub(crate) fn create_dir(dir: &Admitted<'_>, mode: u32) -> io::Result<()> {
    let builder = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = builder.mode(mode);
    builder.create(&dir.path)
}

/// Creates the new file `file` with `mode` (Unix only; ignored on Windows), open for writing.
/// Refuses an existing path, and never follows a symlink there.
///
/// Effect: creates a file.
///
/// # Errors
///
/// `AlreadyExists` when anything is at `file`; otherwise fails when it can't be created.
#[cfg_attr(
    not(unix),
    expect(
        unused_variables,
        reason = "`mode` is a Unix permission mode, ignored on Windows"
    )
)]
pub(crate) fn create_new(file: &Admitted<'_>, mode: u32) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    let options = options.write(true).create_new(true);
    #[cfg(unix)]
    let options = options.mode(mode);
    options.open(&file.path)
}

/// Renames `from` onto `to`, replacing any file there.
///
/// Effect: renames a file.
///
/// # Errors
///
/// Fails when the rename does.
pub(crate) fn rename_into(from: &Admitted<'_>, to: &Admitted<'_>) -> io::Result<()> {
    fs::rename(&from.path, &to.path)
}

/// [`rename_into`], rerun briefly on Windows while the target is transiently denied.
///
/// Effect: renames a file, sleeping between tries.
///
/// # Errors
///
/// Fails when the last try does.
pub(crate) fn rename_into_retrying(from: &Admitted<'_>, to: &Admitted<'_>) -> io::Result<()> {
    rename_retrying(
        || fs::rename(&from.path, &to.path),
        cfg!(windows),
        thread::sleep,
    )
}

/// Opens the existing file `file` for writing, without creating or truncating it.
///
/// Effect: opens a file.
///
/// # Errors
///
/// Fails when nothing is at `file` or it can't be opened for writing.
pub(crate) fn open_existing(file: &Admitted<'_>) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).open(&file.path)
}

/// Removes the file `file`. A symlink there is removed, not what it leads to.
///
/// Effect: deletes a file.
///
/// # Errors
///
/// Fails when nothing is there, it is a directory, or it can't be removed.
pub(crate) fn remove_file(file: &Admitted<'_>) -> io::Result<()> {
    fs::remove_file(&file.path)
}

/// Removes the directory `dir` and everything in it. Symlinks inside are removed, not followed.
/// When `dir` is itself a symlink to a directory, only the link is removed, and that succeeds.
///
/// Effect: deletes a directory tree, or one symlink.
///
/// # Errors
///
/// Fails when nothing is at `dir`, it is a file, or something in it can't be removed.
pub(crate) fn remove_dir_all(dir: &Admitted<'_>) -> io::Result<()> {
    fs::remove_dir_all(&dir.path)
}

/// Creates `link` as a new hard link to `original`, both admitted, so a file outside the boundary
/// can't be linked in. Refuses an existing path at `link`.
///
/// Effect: creates a directory entry.
///
/// # Errors
///
/// `AlreadyExists` when anything is at `link`; otherwise fails when the link can't be made.
pub(crate) fn hard_link_new(original: &Admitted<'_>, link: &Admitted<'_>) -> io::Result<()> {
    fs::hard_link(&original.path, &link.path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::cli::testing::symlink;
    use crate::path::AbsolutePath;

    /// A temp dir holding an empty project `root` and an `outside` dir beside it.
    struct Fixture {
        /// Keeps the directory alive for the test.
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temp directory should be creatable");
            for name in ["root", "outside"] {
                fs::create_dir(dir.path().join(name))
                    .expect("the fixture dirs should be creatable");
            }
            Self { dir }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        fn absolute(&self, rel: &str) -> AbsolutePath {
            AbsolutePath::new(&self.path(rel)).expect("a temp directory path should be absolute")
        }

        fn boundary(&self) -> Boundary {
            Boundary::resolve(&self.absolute("root"))
        }

        fn link(&self, pointee: &str, at: &str) {
            symlink(pointee, self.path(at)).expect("the fixture symlink should be creatable");
        }

        fn write(&self, rel: &str, text: &str) {
            fs::write(self.path(rel), text).expect("the fixture file should be writable");
        }

        /// The names directly inside `rel`.
        fn names(&self, rel: &str) -> BTreeSet<String> {
            fs::read_dir(self.path(rel))
                .expect("the fixture dir should be listable")
                .map(|entry| {
                    entry
                        .expect("a fixture dir entry should be readable")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn refusal(fixture: &Fixture, target: &str) -> Escape {
        fixture
            .boundary()
            .admit(&fixture.absolute(target))
            .expect_err("the target should be refused")
    }

    fn is_real_escape(escape: &Escape) -> bool {
        matches!(escape, Escape::Real { .. })
    }

    #[test]
    fn admit_refuses_a_target_that_climbs_out_with_dot_dot() {
        let fixture = Fixture::new();

        assert_eq!(refusal(&fixture, "root/../outside/x"), Escape::Lexical);
    }

    #[test]
    fn admit_refuses_a_dot_dot_that_stays_inside_when_folded() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.path("root/a")).expect("the fixture dir should be creatable");

        let refused = fixture
            .boundary()
            .admit(&fixture.path("root/a/../b"))
            .map(|admitted| admitted.path);

        assert_eq!(refused, Err(Escape::Lexical));
    }

    #[test]
    fn admit_refuses_a_dot_dot_after_a_symlink_unfolded() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path("outside/deep/x"))
            .expect("the fixture dirs should be creatable");
        fixture.link("../outside/deep/x", "root/link");

        let refused = fixture
            .boundary()
            .admit(&fixture.path("root/link/../x"))
            .map(|admitted| admitted.path);

        assert_eq!(refused, Err(Escape::Lexical));
    }

    #[test]
    fn a_boundary_given_with_a_dot_dot_admits_and_contains_nothing() {
        let fixture = Fixture::new();
        let boundary = Boundary::resolve(&fixture.path("outside/../root"));

        let refused = boundary.admit(&fixture.path("root/x"));

        assert_eq!(refused.map(|admitted| admitted.path), Err(Escape::Lexical));
        assert!(!boundary.contains(&fixture.path("root")));
    }

    #[test]
    fn admit_hands_back_the_target_exactly_as_written() {
        let fixture = Fixture::new();
        let target = fixture.path("root/./x");

        let admitted = fixture
            .boundary()
            .admit(&target)
            .map(|admitted| admitted.path.into_os_string());

        assert_eq!(admitted, Ok(target.into_os_string()));
    }

    #[test]
    fn admit_refuses_an_absolute_target_outside_the_boundary() {
        let fixture = Fixture::new();

        assert_eq!(refusal(&fixture, "outside/x"), Escape::Lexical);
    }

    #[test]
    fn admit_refuses_a_target_under_an_in_root_symlink_to_an_outside_dir() {
        let fixture = Fixture::new();
        fixture.link("../outside", "root/out");

        let escape = refusal(&fixture, "root/out/x");

        assert!(
            is_real_escape(&escape),
            "expected Escape::Real, got {escape:?}"
        );
    }

    #[test]
    fn admit_refuses_a_dangling_symlink_leaf_pointing_outside() {
        let fixture = Fixture::new();
        fixture.link("../outside/missing", "root/leaf");

        let escape = refusal(&fixture, "root/leaf");

        assert_eq!(
            escape,
            Escape::Real {
                real: real_location(&fixture.path("outside/missing")),
            },
            "the refusal should name where the dangling link lands"
        );
    }

    #[test]
    fn admit_refuses_an_in_root_symlink_leaf_pointing_to_an_outside_file() {
        let fixture = Fixture::new();
        fixture.write("outside/secret", "outside");
        fixture.link("../outside/secret", "root/leaf");

        let escape = refusal(&fixture, "root/leaf");

        assert!(
            is_real_escape(&escape),
            "expected Escape::Real, got {escape:?}"
        );
    }

    #[test]
    fn admit_refuses_an_out_and_back_alias_of_the_root_that_leaves_it() {
        let fixture = Fixture::new();
        // Out of the root, back in through its name, then out again.
        fixture.link("../root/../outside", "root/alias");

        let escape = refusal(&fixture, "root/alias/x");

        assert!(
            is_real_escape(&escape),
            "expected Escape::Real, got {escape:?}"
        );
    }

    #[test]
    fn admit_accepts_a_missing_file_under_missing_dirs_inside() {
        let fixture = Fixture::new();
        let boundary = fixture.boundary();
        let target = fixture.absolute("root/a/b/c.txt");

        let admitted = boundary
            .admit(&target)
            .expect("a missing path inside the root should be admitted");

        assert_eq!(admitted.as_path(), target.as_path());
    }

    #[test]
    fn admit_accepts_an_in_root_symlink_leaf_to_an_in_root_file() {
        let fixture = Fixture::new();
        fixture.write("root/real.txt", "inside");
        fixture.link("real.txt", "root/leaf");
        let boundary = fixture.boundary();

        let admitted = boundary.admit(&fixture.absolute("root/leaf"));

        assert!(
            admitted.is_ok(),
            "an in-root link to an in-root file should be admitted, got {admitted:?}"
        );
    }

    #[test]
    fn admit_accepts_the_roots_own_files_when_the_root_path_holds_a_symlink() {
        let fixture = Fixture::new();
        fixture.link("root", "root-link");
        let boundary = Boundary::resolve(&fixture.absolute("root-link"));

        let admitted = boundary.admit(&fixture.absolute("root-link/supdoc.lock"));

        assert!(
            admitted.is_ok(),
            "a file under a symlinked root should be admitted, got {admitted:?}"
        );
    }

    /// `relative` with every symlink followed.
    fn canonical(fixture: &Fixture, relative: &str) -> AbsolutePath {
        let real =
            fs::canonicalize(fixture.path(relative)).expect("the fixture path should resolve");
        AbsolutePath::new(&real).expect("a canonical path should be absolute")
    }

    /// The real location of the fixture's `root`, with `name` appended.
    fn beneath_real_root(fixture: &Fixture, name: &str) -> AbsolutePath {
        let real = canonical(fixture, "root");
        AbsolutePath::new(&real.as_path().join(name)).expect("a canonical path should be absolute")
    }

    #[test]
    fn admit_resolved_accepts_a_real_file_of_a_root_reached_through_a_symlink() {
        let fixture = Fixture::new();
        fixture.link("root", "root-link");
        let boundary = Boundary::resolve(&fixture.absolute("root-link"));

        let admitted = boundary
            .admit_resolved(&beneath_real_root(&fixture, "supdoc.lock"))
            .expect("the root's own real file should be admitted");

        assert_eq!(
            admitted.as_path(),
            fixture.path("root-link/supdoc.lock"),
            "the file should be admitted named beneath the root as given"
        );
    }

    #[test]
    fn admit_resolved_refuses_a_path_beneath_neither_the_root_nor_its_real_location() {
        let fixture = Fixture::new();
        fixture.link("root", "root-link");
        let boundary = Boundary::resolve(&fixture.absolute("root-link"));
        let elsewhere = canonical(&fixture, "outside");

        let escape = boundary
            .admit_resolved(&elsewhere)
            .expect_err("a path outside the root should be refused");

        assert_eq!(
            escape,
            Escape::Real {
                real: Some(elsewhere.as_path().to_path_buf()),
            }
        );
    }

    #[test]
    fn admit_resolved_refuses_a_real_file_whose_parent_was_swapped_for_a_symlink_out() {
        let fixture = Fixture::new();
        fixture.link("root", "root-link");
        fs::create_dir(fixture.path("root/sub")).expect("the fixture dir should be creatable");
        let boundary = Boundary::resolve(&fixture.absolute("root-link"));
        let real = beneath_real_root(&fixture, "sub/supdoc.lock");
        fs::rename(fixture.path("root/sub"), fixture.path("root/sub-old"))
            .expect("the fixture dir should be movable");
        fixture.link("../outside", "root/sub");

        let escape = boundary
            .admit_resolved(&real)
            .expect_err("a parent swapped for a symlink out should be refused");

        assert!(
            is_real_escape(&escape),
            "expected Escape::Real, got {escape:?}"
        );
        assert!(
            fixture.names("outside").is_empty(),
            "nothing should be written outside"
        );
    }

    #[test]
    fn a_denied_rename_is_retried_on_windows_until_it_succeeds() {
        let mut calls = 0;
        let mut pauses = 0;
        let result = rename_retrying(
            || {
                calls += 1;
                if calls < 4 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            },
            true,
            |_| pauses += 1,
        );

        assert!(result.is_ok(), "the fourth rename succeeds: {result:?}");
        assert_eq!((calls, pauses), (4, 3), "three denials, three pauses");
    }

    #[test]
    fn a_denied_rename_stops_after_the_attempt_limit_with_the_same_error() {
        let mut calls = 0;
        let result = rename_retrying(
            || {
                calls += 1;
                Err(io::ErrorKind::PermissionDenied.into())
            },
            true,
            |_| {},
        );

        assert_eq!(calls, RENAME_ATTEMPTS, "the limit bounds the renames");
        assert_eq!(
            result.expect_err("every rename was denied").kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn a_denied_rename_is_not_retried_off_windows() {
        let mut calls = 0;
        let result = rename_retrying(
            || {
                calls += 1;
                Err(io::ErrorKind::PermissionDenied.into())
            },
            false,
            |_| {},
        );

        assert!(result.is_err(), "the denial is returned");
        assert_eq!(calls, 1, "Unix renames once");
    }

    #[test]
    fn other_rename_errors_are_not_retried_on_windows() {
        let mut calls = 0;
        let result = rename_retrying(
            || {
                calls += 1;
                Err(io::ErrorKind::NotFound.into())
            },
            true,
            |_| {},
        );

        assert!(result.is_err(), "the error is returned");
        assert_eq!(calls, 1, "only a denial is worth waiting out");
    }

    #[test]
    fn a_retryable_rename_error_is_not_retried_off_windows() {
        let mut calls = 0;
        let result = rename_retrying(
            || {
                calls += 1;
                Err(io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION))
            },
            false,
            |_| {},
        );

        assert!(result.is_err(), "the error is returned");
        assert_eq!(calls, 1, "Unix renames once even for a retryable error");
    }

    #[test]
    fn the_retry_bound_and_pause_are_the_declared_ones() {
        let mut pauses = Vec::new();
        let mut calls = 0;
        drop(rename_retrying(
            || {
                calls += 1;
                Err(io::ErrorKind::PermissionDenied.into())
            },
            true,
            |pause| pauses.push(pause),
        ));

        assert_eq!(calls, 10, "ten tries");
        assert_eq!(pauses.len(), 9, "ten tries pause nine times");
        assert!(
            pauses
                .iter()
                .all(|pause| *pause == Duration::from_millis(20)),
            "every pause is 20 ms: {pauses:?}"
        );
    }

    #[test]
    fn each_retryable_raw_os_error_is_retried_on_windows() {
        for code in [5, 32] {
            let mut calls = 0;
            let result = rename_retrying(
                || {
                    calls += 1;
                    if calls < 3 {
                        Err(io::Error::from_raw_os_error(code))
                    } else {
                        Ok(())
                    }
                },
                true,
                |_| {},
            );

            assert!(result.is_ok(), "raw error {code} is retried: {result:?}");
            assert_eq!(calls, 3, "raw error {code} took two retries");
        }
    }

    #[test]
    fn another_raw_os_error_is_not_retried_on_windows() {
        let mut calls = 0;
        let result = rename_retrying(
            || {
                calls += 1;
                Err(io::Error::from_raw_os_error(2))
            },
            true,
            |_| {},
        );

        assert!(result.is_err(), "the error is returned");
        assert_eq!(calls, 1, "raw error 2 is not worth waiting out");
    }

    #[test]
    fn write_atomically_writes_only_the_admitted_file() {
        let fixture = Fixture::new();
        let boundary = fixture.boundary();
        let target = boundary
            .admit(&fixture.absolute("root/out.txt"))
            .expect("a new file inside the root should be admitted");

        write_atomically(&target, b"written", None).expect("the write should succeed");

        assert_eq!(
            fs::read_to_string(target.as_path()).expect("the written file should be readable"),
            "written"
        );
        assert_eq!(
            fixture.names("root"),
            names(&["out.txt"]),
            "no temp file should remain"
        );
        assert!(
            fixture.names("outside").is_empty(),
            "nothing should be written outside"
        );
    }

    #[test]
    fn real_location_of_a_dangling_leaf_symlink_is_its_pointee_byte_for_byte() {
        let fixture = Fixture::new();
        let outside = fs::canonicalize(fixture.path("outside"))
            .expect("the outside dir should be canonicalizable");
        fixture.link(
            outside
                .join("missing")
                .to_str()
                .expect("the temp path should be UTF-8"),
            "root/dangling",
        );

        let real = real_location(&fixture.path("root/dangling"))
            .expect("a dangling link to a missing file should still resolve");

        assert_eq!(
            real.as_os_str(),
            outside.join("missing").as_os_str(),
            "the link's own location must add nothing, not even a trailing separator"
        );
    }

    #[test]
    fn write_atomically_replaces_an_existing_file_and_leaves_no_temp_file() {
        let fixture = Fixture::new();
        fixture.write("root/out.txt", "old");
        let boundary = fixture.boundary();
        let target = boundary
            .admit(&fixture.absolute("root/out.txt"))
            .expect("an existing file inside the root should be admitted");

        write_atomically(&target, b"new", None).expect("the replacing write should succeed");

        assert_eq!(
            fs::read_to_string(target.as_path()).expect("the written file should be readable"),
            "new"
        );
        assert_eq!(
            fixture.names("root"),
            names(&["out.txt"]),
            "no temp file should remain beside the target"
        );
    }

    #[test]
    fn temp_path_beside_keeps_the_directory_and_extends_the_file_name() {
        let target = Path::new("dir").join("out.txt");

        let temp = temp_path_beside(&target, 3);

        assert_eq!(
            temp.parent(),
            target.parent(),
            "the temp file stays beside the target"
        );
        let name = temp
            .file_name()
            .and_then(|name| name.to_str())
            .expect("the temp name should be UTF-8");
        assert!(
            name.starts_with("out.txt.") && name.ends_with(".3.tmp"),
            "the temp name should extend the target's name and carry the attempt, got {name}"
        );
        assert_ne!(temp, target);
    }

    #[cfg(unix)]
    #[test]
    fn temp_path_beside_a_root_falls_back_to_the_root_itself() {
        assert_eq!(temp_path_beside(Path::new("/"), 0), Path::new("/"));
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_all_creates_only_the_admitted_dirs_with_the_mode() {
        let fixture = Fixture::new();
        let boundary = fixture.boundary();
        let dir = boundary
            .admit(&fixture.absolute("root/a/b"))
            .expect("missing dirs inside the root should be admitted");

        create_dir_all(&dir, 0o700).expect("the dirs should be creatable");

        let mode = fs::metadata(dir.as_path())
            .expect("the created dir should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(fixture.names("root"), names(&["a"]));
        assert_eq!(fixture.names("root/a"), names(&["b"]));
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_creates_only_the_admitted_dir_with_the_mode() {
        let fixture = Fixture::new();
        let boundary = fixture.boundary();
        let dir = boundary
            .admit(&fixture.absolute("root/fresh"))
            .expect("a missing dir inside the root should be admitted");

        create_dir(&dir, 0o700).expect("the dir should be creatable");

        let mode = fs::metadata(dir.as_path())
            .expect("the created dir should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(fixture.names("root"), names(&["fresh"]));
    }

    #[test]
    fn create_dir_refuses_an_existing_dir_file_or_symlink_and_changes_nothing() {
        let fixture = Fixture::new();
        let boundary = fixture.boundary();
        fs::create_dir(fixture.path("root/dir")).expect("the fixture dir should be creatable");
        fixture.write("root/file", "kept");
        fixture.link("missing", "root/link");

        for at in ["root/dir", "root/file", "root/link"] {
            let path = boundary
                .admit(&fixture.absolute(at))
                .expect("a path inside the root should be admitted");

            let made = create_dir(&path, 0o700);

            assert_eq!(
                made.map_err(|error| error.kind()),
                Err(io::ErrorKind::AlreadyExists),
                "{at}"
            );
        }
        assert_eq!(fixture.names("root"), names(&["dir", "file", "link"]));
        assert!(fixture.names("root/dir").is_empty());
        assert_eq!(
            fs::read_to_string(fixture.path("root/file"))
                .ok()
                .as_deref(),
            Some("kept")
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_new_creates_only_the_admitted_file_and_refuses_an_existing_one() {
        let fixture = Fixture::new();
        let boundary = fixture.boundary();
        let file = boundary
            .admit(&fixture.absolute("root/new"))
            .expect("a new file inside the root should be admitted");

        drop(create_new(&file, 0o600).expect("the file should be creatable"));
        let again = create_new(&file, 0o600).map(drop);

        let mode = fs::metadata(file.as_path())
            .expect("the created file should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fixture.names("root"), names(&["new"]));
        assert_eq!(
            again.map_err(|error| error.kind()),
            Err(io::ErrorKind::AlreadyExists)
        );
    }

    #[test]
    fn rename_into_moves_one_admitted_path_onto_another() {
        let fixture = Fixture::new();
        fixture.write("root/temp", "new");
        fixture.write("root/target", "old");
        let boundary = fixture.boundary();
        let from = boundary
            .admit(&fixture.absolute("root/temp"))
            .expect("the temp file should be admitted");
        let to = boundary
            .admit(&fixture.absolute("root/target"))
            .expect("the target should be admitted");

        rename_into(&from, &to).expect("the rename should succeed");

        assert_eq!(
            fs::read_to_string(to.as_path()).expect("the target should be readable"),
            "new"
        );
        assert_eq!(fixture.names("root"), names(&["target"]));
    }

    #[test]
    fn remove_file_removes_an_in_root_symlink_leaf_not_its_pointee() {
        let fixture = Fixture::new();
        fixture.write("root/real.txt", "kept");
        fixture.link("real.txt", "root/leaf");
        let boundary = fixture.boundary();
        let leaf = boundary
            .admit(&fixture.absolute("root/leaf"))
            .expect("the in-root link should be admitted");

        remove_file(&leaf).expect("the link should be removable");

        assert_eq!(fixture.names("root"), names(&["real.txt"]));
    }

    #[test]
    fn remove_dir_all_removes_only_the_admitted_tree() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path("root/gone/deep"))
            .expect("the fixture tree should be creatable");
        fixture.write("root/kept", "kept");
        fixture.write("outside/kept", "kept");
        fixture.link("../../outside", "root/gone/out");
        let boundary = fixture.boundary();
        let dir = boundary
            .admit(&fixture.absolute("root/gone"))
            .expect("the in-root dir should be admitted");

        remove_dir_all(&dir).expect("the tree should be removable");

        assert_eq!(fixture.names("root"), names(&["kept"]));
        assert_eq!(
            fixture.names("outside"),
            names(&["kept"]),
            "a symlink inside the tree should be removed, not followed"
        );
    }

    #[test]
    fn hard_link_new_links_only_the_admitted_path_and_refuses_an_existing_one() {
        let fixture = Fixture::new();
        fixture.write("root/original", "shared");
        let boundary = fixture.boundary();
        let original = boundary
            .admit(&fixture.absolute("root/original"))
            .expect("the in-root original should be admitted");
        let link = boundary
            .admit(&fixture.absolute("root/link"))
            .expect("a new path inside the root should be admitted");

        hard_link_new(&original, &link).expect("the link should be creatable");
        let again = hard_link_new(&original, &link);

        assert_eq!(
            fs::read_to_string(link.as_path()).expect("the link should be readable"),
            "shared"
        );
        assert_eq!(fixture.names("root"), names(&["link", "original"]));
        assert_eq!(
            again.map_err(|error| error.kind()),
            Err(io::ErrorKind::AlreadyExists)
        );
    }

    #[test]
    fn real_inside_returns_the_real_path_of_an_inside_file() {
        let fixture = Fixture::new();
        fixture.write("root/real.txt", "inside");
        fixture.link("real.txt", "root/leaf");

        let real = fixture.boundary().real_inside(&fixture.path("root/leaf"));

        assert_eq!(
            real,
            Ok(canonical(&fixture, "root/real.txt").as_path().to_path_buf())
        );
    }

    #[test]
    fn real_inside_refuses_a_dangling_in_root_symlink_as_unresolved_where_contains_accepts() {
        let fixture = Fixture::new();
        fixture.link("missing", "root/leaf");
        let boundary = fixture.boundary();
        let leaf = fixture.path("root/leaf");

        assert!(
            boundary.contains(&leaf),
            "contains should count a dangling link pointing inside as inside"
        );
        assert_eq!(boundary.real_inside(&leaf), Err(ReadEscape::Unresolved));
    }

    #[test]
    fn real_inside_refuses_every_path_under_a_missing_root_as_boundary_unresolved() {
        let fixture = Fixture::new();
        let boundary = Boundary::resolve(&fixture.path("missing-root"));

        assert_eq!(
            boundary.real_inside(&fixture.path("root")),
            Err(ReadEscape::BoundaryUnresolved),
            "the root failure should win over the path's resolving"
        );
    }

    #[test]
    fn real_inside_names_where_an_in_root_symlink_to_an_outside_file_lands() {
        let fixture = Fixture::new();
        fixture.write("outside/secret", "outside");
        fixture.link("../outside/secret", "root/leaf");

        let escape = fixture.boundary().real_inside(&fixture.path("root/leaf"));

        assert_eq!(
            escape,
            Err(ReadEscape::Outside {
                real: canonical(&fixture, "outside/secret")
                    .as_path()
                    .to_path_buf(),
            })
        );
    }

    #[test]
    fn admit_refuses_an_outside_file_symlink_into_the_root_as_lexical() {
        let fixture = Fixture::new();
        fixture.write("root/file", "inside");
        fixture.link("../root/file", "outside/leaf");

        let escape = refusal(&fixture, "outside/leaf");

        assert_eq!(escape, Escape::Lexical);
        assert!(
            fs::symlink_metadata(fixture.path("outside/leaf"))
                .expect("the outside link should still exist")
                .file_type()
                .is_symlink(),
            "the outside link should still be a link"
        );
        assert_eq!(fixture.names("outside"), names(&["leaf"]));
    }

    #[test]
    fn admit_refuses_a_target_under_an_outside_dir_symlink_into_the_root_as_lexical() {
        let fixture = Fixture::new();
        fixture.link("../root", "outside/in");

        let escape = refusal(&fixture, "outside/in/x");

        assert_eq!(escape, Escape::Lexical);
        assert!(
            fs::symlink_metadata(fixture.path("outside/in"))
                .expect("the outside link should still exist")
                .file_type()
                .is_symlink(),
            "the outside link should still be a link"
        );
        assert_eq!(fixture.names("outside"), names(&["in"]));
        assert!(
            fixture.names("root").is_empty(),
            "nothing should be written in the root"
        );
    }

    #[test]
    fn hard_link_new_cannot_take_an_outside_original() {
        let fixture = Fixture::new();
        fixture.write("outside/secret", "outside");

        let escape = refusal(&fixture, "outside/secret");

        assert_eq!(
            escape,
            Escape::Lexical,
            "an outside original is never admitted, so never linked"
        );
        assert!(
            fixture.names("root").is_empty(),
            "nothing should be linked into the root"
        );
    }

    #[test]
    fn remove_dir_all_on_an_in_root_dir_symlink_removes_only_the_link() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.path("root/real")).expect("the fixture dir should be creatable");
        fixture.write("root/real/kept", "kept");
        fixture.link("real", "root/alias");
        let boundary = fixture.boundary();
        let alias = boundary
            .admit(&fixture.absolute("root/alias"))
            .expect("the in-root link should be admitted");

        remove_dir_all(&alias).expect("removing a dir symlink should succeed");

        assert_eq!(fixture.names("root"), names(&["real"]));
        assert_eq!(fixture.names("root/real"), names(&["kept"]));
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
