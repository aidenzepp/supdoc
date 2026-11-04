//! The trust store: the checkouts the operator trusts to run code they control.
//!
//! Trust is granted only from outside the checkout: by `SUPDOC_TRUST=1` for one process, or by
//! the root's real path in `$XDG_STATE_HOME/supdoc/trusted-roots` (under `$HOME/.local/state` when
//! `XDG_STATE_HOME` isn't an absolute path), one absolute path per line. A linked worktree of a
//! listed repository inherits its trust when it sits at the same path in its work tree. No file in
//! the checkout can grant trust: the only files read inside it are the `.git` file or directory
//! that locates its git directories.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::path::holds_git;
use crate::scan::git::{common_dir, first_line};

/// Tells concurrent writers' temporary files apart within one process.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// The variables trust is decided from, read once at the process boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrustEnv {
    /// `XDG_STATE_HOME`, when set.
    state_home: Option<OsString>,
    /// `HOME`, when set.
    home: Option<OsString>,
    /// `SUPDOC_TRUST`, when set.
    trust: Option<OsString>,
}

impl TrustEnv {
    /// Reads `XDG_STATE_HOME`, `HOME` and `SUPDOC_TRUST` through `var`.
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            state_home: var("XDG_STATE_HOME"),
            home: var("HOME"),
            trust: var("SUPDOC_TRUST"),
        }
    }

    /// The trust store: `trusted-roots` under `XDG_STATE_HOME`'s `supdoc`, else under `HOME`'s
    /// `.local/state/supdoc`. `None` when neither is an absolute path.
    pub(crate) fn store(&self) -> Option<Store> {
        let absolute = |value: &Option<OsString>| {
            value
                .as_ref()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let base = absolute(&self.state_home)
            .or_else(|| absolute(&self.home).map(|home| home.join(".local").join("state")))?;
        Some(Store {
            file: base.join("supdoc").join("trusted-roots"),
        })
    }
}

/// `SUPDOC_TRUST` holds a value other than `1`, `0` or empty.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`SUPDOC_TRUST` is `{}`, which supdoc doesn't accept: set it to `1` to trust this run, or to \
     `0` or empty (or unset it) to leave trust to `supdoc trust`",
    .0.to_string_lossy()
)]
pub(crate) struct BadTrustValue(OsString);

/// Whether `root` is trusted: `SUPDOC_TRUST` is `1`, or the trust store lists it ([`Store::trusts`]).
/// A store that is missing, unreadable or can't be located trusts nothing.
///
/// Effect: reads the trust store and, for the worktree rule, the `.git` files and git directories
/// of `root` and of each listed root.
///
/// # Errors
///
/// Fails when `SUPDOC_TRUST` is set to anything but `1`, `0` or empty.
pub(crate) fn granted(root: &Path, env: &TrustEnv) -> Result<bool, BadTrustValue> {
    match env.trust.as_deref().map(OsStr::as_bytes) {
        Some(b"1") => Ok(true),
        None | Some(b"" | b"0") => Ok(env.store().is_some_and(|store| store.trusts(root))),
        Some(_) => Err(BadTrustValue(env.trust.clone().unwrap_or_default())),
    }
}

/// The trust store file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Store {
    /// The `trusted-roots` file.
    file: PathBuf,
}

impl Store {
    /// Lists `root`'s real path, unless it is already listed. Returns whether it was added.
    ///
    /// Effect: resolves `root`, creates the store's directory mode 0700, and under the store's
    /// lock ([`Store::lock`]) reads the store and, when adding, writes it atomically, mode 0600.
    ///
    /// # Errors
    ///
    /// Fails when `root` can't be resolved, its real path holds a newline, or the store or its
    /// lock can't be read or written.
    pub(crate) fn add(&self, root: &Path) -> io::Result<bool> {
        let real = root.canonicalize()?;
        if real.as_os_str().as_bytes().contains(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root's real path holds a newline, which the trust store can't list",
            ));
        }
        let _lock = self.lock()?;
        let mut roots = self.read()?;
        if roots.contains(&real) {
            return Ok(false);
        }
        roots.push(real);
        self.write(&roots)?;
        Ok(true)
    }

    /// Removes `root` from the store: every entry that is the path as given or its real path, so
    /// a listed path that has since become a symlink can still be revoked. Returns whether one
    /// was listed, with the first entry removed; when none was, the store is left alone.
    ///
    /// Effect: resolves `root`, reads the store, and when removing re-reads and writes it
    /// atomically under the store's lock ([`Store::lock`]).
    ///
    /// # Errors
    ///
    /// Fails when the store exists but can't be read, or it or its lock can't be written.
    pub(crate) fn revoke(&self, root: &Path) -> io::Result<Option<PathBuf>> {
        let real = root.canonicalize().ok();
        let names = |listed: &PathBuf| *listed == root || real.as_ref() == Some(listed);
        if !self.read()?.iter().any(names) {
            return Ok(None);
        }
        let _lock = self.lock()?;
        let mut roots = self.read()?;
        let Some(removed) = roots.iter().find(|listed| names(listed)).cloned() else {
            return Ok(None);
        };
        roots.retain(|listed| !names(listed));
        self.write(&roots)?;
        Ok(Some(removed))
    }

    /// Takes the store's exclusive lock, held until the returned file drops: `trusted-roots.lock`
    /// beside the store, created mode 0600 in a directory created mode 0700. Every
    /// read-modify-rename holds it, so concurrent `add` and `revoke` calls never lose an update.
    ///
    /// Effect: creates the store's directory and lock file, and blocks until the lock is free.
    ///
    /// # Errors
    ///
    /// Fails when the directory or lock file can't be created or locked.
    fn lock(&self) -> io::Result<fs::File> {
        let dir = self.dir()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("trusted-roots.lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// The store's directory.
    ///
    /// # Errors
    ///
    /// Fails when the store's path has no parent, which [`TrustEnv::store`] never builds.
    fn dir(&self) -> io::Result<&Path> {
        self.file.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the trust store has no directory",
            )
        })
    }

    /// Whether the store trusts `root`: its real path is listed, or it sits in a linked worktree
    /// ([`Place::linked`]) whose common git directory and path in its work tree equal a listed
    /// root's. A listed root counts for that only while it still resolves to itself: the store
    /// holds real paths, so one that resolves elsewhere has been replaced since it was listed. A
    /// missing or unreadable store trusts nothing.
    ///
    /// Effect: reads the store, resolves `root`, and reads the `.git` files and git directories of
    /// `root` and of each listed root.
    fn trusts(&self, root: &Path) -> bool {
        let Ok(real) = root.canonicalize() else {
            return false;
        };
        let Ok(roots) = self.read() else {
            return false;
        };
        if roots.contains(&real) {
            return true;
        }
        let Some(place) = Place::linked(&real) else {
            return false;
        };
        roots.iter().any(|listed| {
            listed.canonicalize().is_ok_and(|real| real == *listed)
                && Place::of(listed).is_some_and(|listed| listed == place)
        })
    }

    /// The listed roots, in order: each line that is an absolute path. None when the file is
    /// missing.
    ///
    /// Effect: reads the store.
    ///
    /// # Errors
    ///
    /// Fails when the store exists but can't be read.
    fn read(&self) -> io::Result<Vec<PathBuf>> {
        let bytes = match fs::read(&self.file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(bytes
            .split(|byte| *byte == b'\n')
            .map(|line| PathBuf::from(OsString::from_vec(line.to_vec())))
            .filter(|path| path.is_absolute())
            .collect())
    }

    /// Replaces the store with `roots`, one per line: a temporary file in the same directory,
    /// created mode 0600, renamed over it. The caller holds the lock, which made the directory.
    ///
    /// Effect: writes, then renames, a temporary file in the store's directory.
    ///
    /// # Errors
    ///
    /// Fails when the file can't be written or renamed; the temporary file is then removed and
    /// the store left as it was.
    fn write(&self, roots: &[PathBuf]) -> io::Result<()> {
        let dir = self.dir()?;
        let temp = dir.join(format!(
            ".trusted-roots.{}.{}.tmp",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            for root in roots {
                file.write_all(root.as_os_str().as_bytes())?;
                file.write_all(b"\n")?;
            }
            file.sync_all()?;
            fs::rename(&temp, &self.file)
        })();
        if written.is_err() {
            drop(fs::remove_file(&temp));
        }
        written
    }
}

/// Where a directory sits in git: its work tree's common git directory (real path) and its path
/// relative to the work tree's top.
#[derive(Debug, PartialEq, Eq)]
struct Place {
    /// The real path of the common git directory.
    common: PathBuf,
    /// The directory's path relative to its work tree's top.
    relative: PathBuf,
}

impl Place {
    /// The place of the real directory `real`, in whichever work tree holds it: the nearest
    /// ancestor-or-self whose `.git` git accepts ([`holds_git`]). For a `.git` file the common
    /// directory is the one its git directory names ([`common_dir`]); for a `.git` directory it is
    /// that directory. `None` outside a work tree or when the common directory can't be found.
    ///
    /// Effect: reads `.git` metadata up from `real`, the `.git` file and its `commondir`.
    fn of(real: &Path) -> Option<Self> {
        let top = real.ancestors().find(|dir| holds_git(dir))?;
        let dot_git = top.join(".git");
        let common = if fs::metadata(&dot_git).ok()?.is_file() {
            common_dir(top, &dot_git)?
        } else {
            dot_git
        };
        Some(Self {
            common: common.canonicalize().ok()?,
            relative: real.strip_prefix(top).ok()?.to_path_buf(),
        })
    }

    /// The place of `real` when it sits in a linked worktree that git itself made: the top's
    /// `.git` is a regular file, not a symlink to one, naming a git directory in the common directory's `worktrees`, whose
    /// `gitdir` file names that `.git` file back. A `.git` file that merely names a trusted
    /// repository's git directory, as a forged checkout could, doesn't count, since only git
    /// writes inside the trusted repository; nor does a `.git` symlink to a real worktree's
    /// `.git` file, which names that worktree back.
    ///
    /// Effect: reads `.git` metadata up from `real`, the `.git` file, its git directory's
    /// `commondir` and `gitdir` files.
    fn linked(real: &Path) -> Option<Self> {
        let top = real.ancestors().find(|dir| holds_git(dir))?;
        let dot_git = top.join(".git");
        if !fs::symlink_metadata(&dot_git).ok()?.is_file() {
            return None;
        }
        let line = first_line(&dot_git)?;
        let git_dir = top
            .join(line.strip_prefix("gitdir: ")?)
            .canonicalize()
            .ok()?;
        let place = Self::of(real)?;
        let back = git_dir.join(first_line(&git_dir.join("gitdir"))?);
        let names_back = back.canonicalize().ok()? == dot_git.canonicalize().ok()?;
        let in_worktrees = git_dir.parent() == Some(place.common.join("worktrees").as_path());
        (names_back && in_worktrees).then_some(place)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::process::Command;
    use std::sync::{Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// A scratch directory holding a real path, with a trust environment rooted in it.
    struct Scratch {
        /// The scratch directory, removed on drop.
        dir: tempfile::TempDir,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().expect("a temporary directory should be creatable"),
            }
        }

        /// The scratch directory's real path.
        fn real(&self) -> PathBuf {
            self.dir
                .path()
                .canonicalize()
                .expect("the scratch directory should resolve")
        }

        /// A new directory `name` in the scratch directory, by its real path.
        fn mkdir(&self, name: &str) -> PathBuf {
            let path = self.real().join(name);
            fs::create_dir_all(&path).expect("a scratch subdirectory should be creatable");
            path
        }

        /// A trust environment whose state lives in the scratch directory, with `SUPDOC_TRUST`
        /// set to `trust` when given.
        fn env(&self, trust: Option<&str>) -> TrustEnv {
            TrustEnv {
                state_home: Some(self.real().join("state").into()),
                home: Some(self.real().join("home").into()),
                trust: trust.map(OsString::from),
            }
        }

        /// The store of [`Scratch::env`].
        fn store(&self) -> Store {
            self.env(None)
                .store()
                .expect("an absolute XDG_STATE_HOME should locate the store")
        }
    }

    /// Runs `git` with `args` in `dir`, with no user config and a fixed identity.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=supdoc",
                "-c",
                "user.email=supdoc@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false", "-C"])
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git should be runnable to build the fixture repository");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A repository `repo` with one commit and a `sub` directory, and a linked worktree `wt` of it
    /// that also has `sub`.
    fn repo_with_worktree(scratch: &Scratch) -> (PathBuf, PathBuf) {
        let repo = scratch.mkdir("repo");
        git(&repo, &["init", "--quiet"]);
        fs::create_dir(repo.join("sub")).expect("the repo's sub directory should be creatable");
        fs::write(repo.join("sub/file"), "").expect("the repo's file should be writable");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "fixture"]);
        let worktree = scratch.real().join("wt");
        let worktree_arg = worktree.to_str().expect("the scratch path should be UTF-8");
        git(&repo, &["worktree", "add", "--quiet", worktree_arg]);
        (repo, worktree)
    }

    #[test]
    fn a_listed_root_is_trusted() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        assert!(scratch.store().add(&root).expect("adding should work"));

        assert_eq!(granted(&root, &scratch.env(None)), Ok(true));
    }

    #[test]
    fn an_unlisted_root_is_not_trusted() {
        let scratch = Scratch::new();
        let listed = scratch.mkdir("listed");
        let other = scratch.mkdir("other");
        assert!(scratch.store().add(&listed).expect("adding should work"));

        assert_eq!(granted(&other, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn a_linked_worktree_of_a_listed_repo_is_trusted() {
        let scratch = Scratch::new();
        let (repo, worktree) = repo_with_worktree(&scratch);
        assert!(
            scratch
                .store()
                .add(&repo.join("sub"))
                .expect("adding should work")
        );

        assert_eq!(granted(&worktree.join("sub"), &scratch.env(None)), Ok(true));
    }

    #[test]
    fn a_linked_worktree_at_a_different_relative_path_is_not_trusted() {
        let scratch = Scratch::new();
        let (repo, worktree) = repo_with_worktree(&scratch);
        assert!(
            scratch
                .store()
                .add(&repo.join("sub"))
                .expect("adding should work")
        );

        assert_eq!(granted(&worktree, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn a_forged_git_file_naming_a_listed_repo_is_not_trusted() {
        let scratch = Scratch::new();
        let (repo, worktree) = repo_with_worktree(&scratch);
        assert!(scratch.store().add(&repo).expect("adding should work"));
        let worktree_git_dir = repo.join(".git/worktrees/wt");
        let forged_common = scratch.mkdir("forged-common");
        let forged_worktree = scratch.mkdir("forged-worktree");
        fs::write(
            forged_common.join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .expect("the forged .git file should be writable");
        fs::write(
            forged_worktree.join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .expect("the forged .git file should be writable");

        assert_eq!(granted(&worktree, &scratch.env(None)), Ok(true));
        assert_eq!(granted(&forged_common, &scratch.env(None)), Ok(false));
        assert_eq!(granted(&forged_worktree, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn a_dot_git_symlink_to_a_real_worktree_is_not_trusted() {
        let scratch = Scratch::new();
        let (repo, worktree) = repo_with_worktree(&scratch);
        assert!(scratch.store().add(&repo).expect("adding should work"));
        let forged = scratch.mkdir("forged");
        fs::write(forged.join("build.rs"), "attacker code").expect("the file should be writable");
        symlink(worktree.join(".git"), forged.join(".git"))
            .expect("the symlink should be creatable");

        assert_eq!(granted(&worktree, &scratch.env(None)), Ok(true));
        assert_eq!(granted(&forged, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn a_self_consistent_forged_worktree_outside_the_common_worktrees_is_not_trusted() {
        let scratch = Scratch::new();
        let (repo, _) = repo_with_worktree(&scratch);
        assert!(scratch.store().add(&repo).expect("adding should work"));
        let forged = scratch.mkdir("forged");
        fs::create_dir(forged.join("g")).expect("the forged git dir should be creatable");
        fs::write(forged.join(".git"), "gitdir: g\n").expect("the .git file should be writable");
        fs::write(forged.join("g/gitdir"), "../.git\n").expect("gitdir should be writable");
        fs::write(
            forged.join("g/commondir"),
            format!("{}\n", repo.join(".git").display()),
        )
        .expect("commondir should be writable");

        assert_eq!(granted(&forged, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn a_listed_root_replaced_by_a_symlink_grants_no_worktree_trust() {
        let scratch = Scratch::new();
        let listed = scratch.mkdir("listed");
        assert!(scratch.store().add(&listed).expect("adding should work"));
        fs::remove_dir(&listed).expect("the listed root should be removable");
        let forged = scratch.mkdir("forged");
        fs::create_dir_all(forged.join("x/worktrees/w")).expect("the layout should be creatable");
        fs::write(forged.join(".git"), "gitdir: x/worktrees/w\n")
            .expect("the .git file should be writable");
        fs::write(forged.join("x/worktrees/w/gitdir"), "../../../.git\n")
            .expect("gitdir should be writable");
        fs::write(forged.join("x/worktrees/w/commondir"), "../..\n")
            .expect("commondir should be writable");
        symlink(&forged, &listed).expect("the symlink should be creatable");

        assert_eq!(granted(&forged, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn a_fifo_at_a_forged_gitdir_is_refused_without_blocking() {
        let scratch = Scratch::new();
        let (repo, _) = repo_with_worktree(&scratch);
        assert!(scratch.store().add(&repo).expect("adding should work"));
        let forged = scratch.mkdir("forged");
        fs::create_dir(forged.join("g")).expect("the forged git dir should be creatable");
        fs::write(forged.join(".git"), "gitdir: g\n").expect("the .git file should be writable");
        let fifo = forged.join("g/gitdir");
        let made = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo should be runnable");
        assert!(made.success(), "mkfifo failed for {}", fifo.display());
        let env = scratch.env(None);
        let (sender, receiver) = mpsc::channel();
        let judged = forged.clone();
        let _detached = thread::spawn(move || drop(sender.send(granted(&judged, &env))));

        let result = receiver.recv_timeout(Duration::from_secs(3));
        if result.is_err() {
            // Unblock the reader stuck in open, so the thread ends; no reader means no effect.
            drop(
                fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo),
            );
        }
        assert_eq!(
            result,
            Ok(Ok(false)),
            "granted() blocked on or misjudged a FIFO"
        );
    }

    #[test]
    fn revoke_removes_a_listed_path_that_became_a_symlink() {
        let scratch = Scratch::new();
        let store = scratch.store();
        let listed = scratch.mkdir("listed");
        let other = scratch.mkdir("other");
        assert!(store.add(&listed).expect("adding should work"));
        fs::remove_dir(&listed).expect("the listed root should be removable");
        symlink(&other, &listed).expect("the symlink should be creatable");

        assert_eq!(
            store.revoke(&listed).expect("revoking should work"),
            Some(listed.clone())
        );
        assert_eq!(
            fs::read_to_string(&store.file).expect("the store should be readable"),
            ""
        );
    }

    #[test]
    fn parallel_adds_all_land() {
        let scratch = Scratch::new();
        let roots: Vec<PathBuf> = (0..8).map(|n| scratch.mkdir(&format!("root{n}"))).collect();
        let barrier = Barrier::new(roots.len());

        thread::scope(|scope| {
            for root in &roots {
                let store = scratch.store();
                let barrier = &barrier;
                let _joined_at_scope_end = scope.spawn(move || {
                    let _leader = barrier.wait();
                    assert!(store.add(root).expect("adding should work"));
                });
            }
        });

        let listed = scratch
            .store()
            .read()
            .expect("the store should be readable");
        for root in &roots {
            assert!(
                listed.contains(root),
                "{} was lost from {listed:?}",
                root.display()
            );
        }
    }

    #[test]
    fn a_relative_xdg_state_home_falls_back_to_home() {
        let env = TrustEnv::from_vars(|name| match name {
            "XDG_STATE_HOME" => Some("relative".into()),
            "HOME" => Some("/home/someone".into()),
            _ => None,
        });
        let neither =
            TrustEnv::from_vars(|name| (name == "XDG_STATE_HOME").then(|| "relative".into()));

        assert_eq!(
            env.store().map(|store| store.file),
            Some(PathBuf::from(
                "/home/someone/.local/state/supdoc/trusted-roots"
            ))
        );
        assert_eq!(neither.store(), None);
    }

    #[test]
    fn a_missing_store_is_untrusted_not_an_error() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");

        assert_eq!(granted(&root, &scratch.env(None)), Ok(false));
        assert!(!scratch.store().file.exists());
    }

    #[test]
    fn non_absolute_lines_are_ignored_and_dropped_on_write() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        let other = scratch.mkdir("other");
        let store = scratch.store();
        fs::create_dir_all(
            store
                .file
                .parent()
                .expect("the store should have a directory"),
        )
        .expect("the store directory should be creatable");
        let relative = root
            .strip_prefix("/")
            .expect("the root should be absolute")
            .display();
        fs::write(&store.file, format!("{relative}\n\nroot\n"))
            .expect("the store should be writable");

        assert_eq!(granted(&root, &scratch.env(None)), Ok(false));
        assert!(store.add(&other).expect("adding should work"));
        assert_eq!(
            fs::read_to_string(&store.file).expect("the store should be readable"),
            format!("{}\n", other.display())
        );
    }

    #[test]
    fn revoke_of_an_unlisted_root_reports_false_and_leaves_the_file_alone() {
        let scratch = Scratch::new();
        let listed = scratch.mkdir("listed");
        let unlisted = scratch.mkdir("unlisted");
        let store = scratch.store();
        assert!(store.add(&listed).expect("adding should work"));
        fs::write(&store.file, format!("{}\nrelative\n", listed.display()))
            .expect("the store should be writable");

        assert_eq!(store.revoke(&unlisted).expect("revoking should work"), None);
        assert_eq!(
            fs::read_to_string(&store.file).expect("the store should be readable"),
            format!("{}\nrelative\n", listed.display())
        );
        assert_eq!(
            store.revoke(&listed).expect("revoking should work"),
            Some(listed.clone())
        );
        assert_eq!(granted(&listed, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn supdoc_trust_grants_for_1_and_not_for_0_empty_or_unset() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");

        assert_eq!(granted(&root, &scratch.env(Some("1"))), Ok(true));
        assert_eq!(granted(&root, &scratch.env(Some("0"))), Ok(false));
        assert_eq!(granted(&root, &scratch.env(Some(""))), Ok(false));
        assert_eq!(granted(&root, &scratch.env(None)), Ok(false));
    }

    #[test]
    fn supdoc_trust_yes_is_an_error_naming_the_accepted_values() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");

        let error = granted(&root, &scratch.env(Some("yes"))).expect_err("`yes` should be refused");

        assert_eq!(
            error.to_string(),
            "`SUPDOC_TRUST` is `yes`, which supdoc doesn't accept: set it to `1` to trust this \
             run, or to `0` or empty (or unset it) to leave trust to `supdoc trust`"
        );
    }

    #[test]
    fn the_store_is_created_0600_in_a_0700_directory() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        let store = scratch.store();
        assert!(store.add(&root).expect("adding should work"));

        let mode = |path: &Path| {
            fs::metadata(path)
                .expect("the store should exist")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&store.file), 0o600);
        assert_eq!(
            mode(
                store
                    .file
                    .parent()
                    .expect("the store should have a directory")
            ),
            0o700
        );
    }
}
