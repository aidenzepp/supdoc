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

use std::fs;
use std::io;
use std::marker::PhantomData;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};

use crate::path::AbsolutePath;

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
    // Climb to the deepest entry that exists, remembering the missing names below it.
    let mut existing = path;
    let mut missing = Vec::new();
    while fs::symlink_metadata(existing).is_err() {
        missing.push(normal_name(existing)?);
        existing = existing.parent()?;
    }
    let real_existing = if let Ok(real) = fs::canonicalize(existing) {
        real
    } else {
        // The entry exists but can't be resolved: a dangling symlink. Follow it by hand.
        let next = hops.checked_sub(1)?;
        let pointee = fs::read_link(existing).ok()?;
        let parent = fs::canonicalize(existing.parent()?).ok()?;
        real_location_within(&parent.join(pointee), next)?
    };
    Some(
        missing
            .iter()
            .rev()
            .fold(real_existing, |real, name| real.join(name)),
    )
}

/// The last component of `path` when it is an ordinary name, not `..`, `.` or a root.
fn normal_name(path: &Path) -> Option<&std::ffi::OsStr> {
    match path.components().next_back()? {
        Component::Normal(name) => Some(name),
        Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => {
            None
        }
    }
}

/// A directory supdoc may write beneath, with its real location worked out once.
#[derive(Debug, Clone)]
pub(crate) struct Boundary {
    /// The directory as given, normalized.
    given: AbsolutePath,
    /// `given` with every symlink followed, or `given` as written when that can't be worked out.
    real: PathBuf,
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
    /// The path isn't beneath the boundary as written: it climbs out with `..` or names an
    /// absolute path elsewhere.
    Lexical,
    /// The path is written beneath the boundary, but a symlink along it leads out. `real` is where
    /// it really lies, or `None` when that can't be worked out.
    Real {
        /// The path's real location, when known.
        real: Option<PathBuf>,
    },
}

impl Boundary {
    /// The boundary at `dir`, its real location worked out now.
    ///
    /// When `dir`'s real location can't be worked out, `dir` as written stands for it.
    ///
    /// Effect: reads file metadata and symlinks along `dir`.
    #[must_use]
    pub(crate) fn resolve(dir: &AbsolutePath) -> Self {
        let real = real_location(dir.as_path()).unwrap_or_else(|| dir.as_path().to_path_buf());
        Self {
            given: dir.clone(),
            real,
        }
    }

    /// Whether `path` lies inside the boundary once every symlink along it is followed.
    ///
    /// A dangling symlink counts where it points, and a path whose real location can't be worked
    /// out counts as outside.
    ///
    /// Effect: reads file metadata and symlinks along `path`.
    #[must_use]
    pub(crate) fn contains(&self, path: &Path) -> bool {
        real_location(path).is_some_and(|real| real.starts_with(&self.real))
    }

    /// `target`, once it is checked to lie inside the boundary both as written and once every
    /// symlink along it is followed. A missing tail counts where it would be.
    ///
    /// Nothing is cached: call it immediately before the write it guards.
    ///
    /// Effect: reads file metadata and symlinks along `target`.
    ///
    /// # Errors
    ///
    /// [`Escape::Lexical`] when `target` isn't beneath the boundary as given. [`Escape::Real`] when
    /// it is, but its real location lies outside the boundary's or can't be worked out.
    pub(crate) fn admit(&self, target: &AbsolutePath) -> Result<Admitted<'_>, Escape> {
        if target.strip_prefix(&self.given).is_none() {
            return Err(Escape::Lexical);
        }
        match real_location(target.as_path()) {
            Some(real) if real.starts_with(&self.real) => Ok(Admitted {
                path: target.as_path().to_path_buf(),
                boundary: PhantomData,
            }),
            real => Err(Escape::Real { real }),
        }
    }
}

impl Admitted<'_> {
    /// The admitted path, for messages and reads.
    #[must_use]
    pub(crate) fn as_path(&self) -> &Path {
        &self.path
    }
}

/// Writes `bytes` to a new file beside `target` and renames it into place, as
/// [`crate::lock::io::write_atomically`] does. The temporary file shares `target`'s admitted
/// parent.
///
/// Effect: creates, writes and renames a file beside `target`.
///
/// # Errors
///
/// Fails when the temporary file can't be created, written or renamed onto `target`.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "write sites admit through it from PH02")
)]
pub(crate) fn write_atomically(
    target: &Admitted<'_>,
    bytes: &[u8],
    permissions: Option<&fs::Permissions>,
) -> io::Result<()> {
    crate::lock::io::write_atomically(&target.path, bytes, permissions)
}

/// Creates `dir` and any missing parents, each new one with `mode`.
///
/// Effect: creates directories.
///
/// # Errors
///
/// Fails when a directory can't be created, or `dir` exists and isn't a directory.
pub(crate) fn create_dir_all(dir: &Admitted<'_>, mode: u32) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(&dir.path)
}

/// Creates the new file `file` with `mode`, open for writing. Refuses an existing path, and never
/// follows a symlink there.
///
/// Effect: creates a file.
///
/// # Errors
///
/// `AlreadyExists` when anything is at `file`; otherwise fails when it can't be created.
pub(crate) fn create_new(file: &Admitted<'_>, mode: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&file.path)
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "private dirs are removed through it from PH05")
)]
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
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "`init` links its file into place through it from PH03"
    )
)]
pub(crate) fn hard_link_new(original: &Admitted<'_>, link: &Admitted<'_>) -> io::Result<()> {
    fs::hard_link(&original.path, &link.path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use super::*;

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
}
