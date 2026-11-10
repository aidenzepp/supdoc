//! The trust store: the checkouts the operator trusts to run code they control.
//!
//! Trust is granted only from outside the checkout: by `SUPDOC_TRUST=1` for one process, or by
//! the root's real path in `$XDG_STATE_HOME/supdoc/trusted-roots` (under `$HOME/.local/state` when
//! `XDG_STATE_HOME` isn't an absolute path; on Windows, under `%LOCALAPPDATA%\supdoc` instead, with
//! `XDG_STATE_HOME` still winning when set), one absolute path per line. `SUPDOC_TRUST=0` turns
//! trust off for one process, whatever the store lists. A linked worktree of a listed repository
//! inherits its trust when it sits at the same path in its work tree. `/`, `HOME` and the
//! directories above `HOME` are too broad to trust: the store never lists them and ignores them
//! when listed by hand. No file in the checkout can grant trust: the only files read inside it are
//! the `.git` file or directory that locates its git directories.
//!
//! The store holds each path's bytes as the operating system encodes them. On Unix that is the raw
//! bytes, whatever they are. On Windows it is UTF-8: a line that isn't UTF-8 is skipped, one
//! trailing `\r` is dropped, and a root whose real path isn't valid Unicode or holds a `\r` can't
//! be listed. A
//! drive root such as `D:\` is too broad to trust, as `/` is on Unix.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read as _, Write as _};
#[cfg(all(unix, test))]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;
#[cfg(all(unix, test))]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::confine::{self, Admitted, Boundary};
use crate::path::{Component as NameComponent, Pointer, beneath, fixed, holds_git};
use crate::read::source::open_without_blocking;
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
    /// `LOCALAPPDATA`, when set. Only Windows reads it.
    local_app_data: Option<OsString>,
    /// `USERPROFILE`, when set. Only Windows reads it, as the stand-in for `HOME`.
    user_profile: Option<OsString>,
    /// `SUPDOC_TRUST`, when set.
    trust: Option<OsString>,
}

impl TrustEnv {
    /// Reads `XDG_STATE_HOME`, `HOME`, `LOCALAPPDATA`, `USERPROFILE` and `SUPDOC_TRUST` through
    /// `var`.
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            state_home: var("XDG_STATE_HOME"),
            home: var("HOME"),
            local_app_data: var("LOCALAPPDATA"),
            user_profile: var("USERPROFILE"),
            trust: var("SUPDOC_TRUST"),
        }
    }

    /// The trust store: `trusted-roots` under `XDG_STATE_HOME`'s `supdoc`, else on Windows under
    /// `LOCALAPPDATA`'s `supdoc` (a per-user directory, so the store inherits an owner-only ACL),
    /// else on Unix under `HOME`'s `.local/state/supdoc`. `None` when none is an absolute path.
    /// Every later write stays beneath where that directory really lies now, and is refused when
    /// the variable holds a `..` ([`Boundary::resolve`]).
    ///
    /// Effect: reads file metadata and symlinks along the store's directory.
    pub(crate) fn store(&self) -> Option<Store> {
        self.store_on(cfg!(windows))
    }

    /// [`TrustEnv::store`] as `windows` says: the platform is a parameter so both rules are tested
    /// on every OS.
    fn store_on(&self, windows: bool) -> Option<Store> {
        let home = self.home_on(windows);
        let (base, variable) = absolute(self.state_home.as_ref())
            .map(|state| (state, "XDG_STATE_HOME"))
            .or_else(|| {
                if windows {
                    absolute(self.local_app_data.as_ref()).map(|local| (local, "LOCALAPPDATA"))
                } else {
                    home.as_ref()
                        .map(|home| (beneath(home, &fixed::LOCAL_STATE), "HOME"))
                }
            })?;
        let dir = beneath(&base, &fixed::SUPDOC);
        Some(Store {
            boundary: Boundary::resolve(&dir),
            file: beneath(&dir, &fixed::TRUSTED_ROOTS),
            home,
            variable,
        })
    }

    /// The home directory when it is an absolute path: `USERPROFILE` on Windows, `HOME` elsewhere.
    fn home_on(&self, windows: bool) -> Option<PathBuf> {
        absolute(
            if windows {
                &self.user_profile
            } else {
                &self.home
            }
            .as_ref(),
        )
    }
}

/// The path `value` holds, when it is absolute.
fn absolute(value: Option<&OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// `SUPDOC_TRUST` holds a value other than `1`, `0` or empty.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`SUPDOC_TRUST` is `{}`, which supdoc doesn't accept: set it to `1` to trust this run, to `0` \
     to turn trust off for this run, or leave it empty (or unset it) to leave trust to \
     `supdoc trust`",
    .0.to_string_lossy()
)]
pub(crate) struct BadTrustValue(OsString);

/// What the operator granted a root: trust, none, none because `SUPDOC_TRUST=0` turned it off, or
/// none because the trust store couldn't be read for a reason the operator can fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grant {
    /// `SUPDOC_TRUST` is `1`, or it is unset or empty and the trust store trusts the root.
    Trusted,
    /// `SUPDOC_TRUST` is unset or empty and the trust store doesn't trust the root.
    Untrusted,
    /// `SUPDOC_TRUST` is `0`: trust is off for this run, whatever the store lists.
    TurnedOff,
    /// `SUPDOC_TRUST` is unset or empty and the trust store wasn't read, for this reason: nothing
    /// is trusted.
    Unread(Unread),
}

/// Why the trust store wasn't read, with the variable it is located through,
/// `XDG_STATE_HOME`, `LOCALAPPDATA` or `HOME`. Reads refuse what writes refuse, so `supdoc trust` can't fix it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unread {
    /// The variable holds a `..`.
    Climbs(&'static str),
    /// The store file leads outside the store's directory once symlinks are followed.
    LeadsOutside(&'static str),
    /// The store file isn't a regular file, such as a FIFO, a socket or a directory.
    NotAFile(&'static str),
    /// The store file exists but couldn't be read, failing with this kind of error.
    Unreadable(&'static str, io::ErrorKind),
}

impl Unread {
    /// Why the store wasn't read, as a clause naming the variable.
    pub(crate) fn reason(self) -> String {
        match self {
            Self::Climbs(variable) => {
                format!("`{variable}` holds `..`, which the trust store's path must not contain")
            }
            Self::LeadsOutside(variable) => format!(
                "the trust store file under `{variable}` leads outside its directory once \
                 symlinks are followed"
            ),
            Self::NotAFile(variable) => {
                format!("the trust store file under `{variable}` isn't a regular file")
            }
            Self::Unreadable(variable, kind) => {
                format!("the trust store file under `{variable}` can't be read: {kind}")
            }
        }
    }

    /// What lets the store be read again, as a clause starting lowercase.
    pub(crate) fn fix(self) -> String {
        match self {
            Self::Climbs(variable) => format!("set `{variable}` to a path without `..`"),
            Self::LeadsOutside(variable) | Self::NotAFile(variable) => format!(
                "make `supdoc/trusted-roots` under `{variable}` a regular file in that directory, \
                 not a symlink out of it"
            ),
            Self::Unreadable(variable, _) => {
                format!("make `supdoc/trusted-roots` under `{variable}` readable by you")
            }
        }
    }

    /// The error a read or write of the store at `path` fails with for this reason, carrying
    /// it for [`unread_of`]: [`io::ErrorKind::PermissionDenied`] for a `..` or a path leading
    /// outside, [`io::ErrorKind::InvalidData`] for a store file that isn't a regular file, and the
    /// read's own kind for one that couldn't be read.
    fn error(self, path: &Path) -> io::Error {
        let (kind, message) = match self {
            Self::Climbs(variable) => (
                io::ErrorKind::PermissionDenied,
                format!(
                    "`{variable}` holds `..`, which the trust store's path must not contain: set \
                     it to a path without `..`"
                ),
            ),
            Self::LeadsOutside(_) => (
                io::ErrorKind::PermissionDenied,
                format!(
                    "`{}` lies outside the trust store's directory once symlinks are followed",
                    path.display()
                ),
            ),
            Self::NotAFile(_) => (
                io::ErrorKind::InvalidData,
                format!("`{}` isn't a regular file", path.display()),
            ),
            Self::Unreadable(_, kind) => {
                (kind, format!("`{}` can't be read: {kind}", path.display()))
            }
        };
        io::Error::new(
            kind,
            Refusal {
                unread: self,
                message,
            },
        )
    }
}

/// An [`Unread`] carried inside the [`io::Error`] a store read or write fails with.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct Refusal {
    /// Why the store was refused.
    unread: Unread,
    /// What the error says.
    message: String,
}

/// Why the trust store refused the read or write that failed with `error`, when it was refused
/// for a reason the operator can fix rather than a plain I/O failure.
pub(crate) fn unread_of(error: &io::Error) -> Option<Unread> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Refusal>())
        .map(|refusal| refusal.unread)
}

/// What `root` is granted: [`Grant::Trusted`] when `SUPDOC_TRUST` is `1`, or it is unset or empty
/// and the trust store trusts it ([`Store::grant`]); [`Grant::TurnedOff`] when `SUPDOC_TRUST` is
/// `0`, whatever the store lists; [`Grant::Unread`] when the store wasn't read for a reason the
/// operator can fix; else [`Grant::Untrusted`]. A store that is missing, unreadable or can't be
/// located trusts nothing.
///
/// Effect: reads the trust store and, for the worktree rule, the `.git` files and git directories
/// of `root` and of each listed root.
///
/// # Errors
///
/// Fails when `SUPDOC_TRUST` is set to anything but `1`, `0` or empty.
pub(crate) fn granted(root: &Path, env: &TrustEnv) -> Result<Grant, BadTrustValue> {
    match env.trust.as_deref().map(OsStr::as_encoded_bytes) {
        Some(b"1") => Ok(Grant::Trusted),
        Some(b"0") => Ok(Grant::TurnedOff),
        None | Some(b"") => Ok(env
            .store()
            .map_or(Grant::Untrusted, |store| store.grant(root))),
        Some(_) => Err(BadTrustValue(env.trust.clone().unwrap_or_default())),
    }
}

/// Why [`Store::add`] didn't list a root.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AddError {
    /// The root's real path is `/`, `HOME`'s real path or a directory above it: trusting it would
    /// cover every directory under it that has no config of its own.
    #[error("`{}` is too broad to trust", .0.display())]
    Broad(PathBuf),
    /// The root couldn't be resolved, or the store or its lock couldn't be read or written.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The trust store file.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    /// The `trusted-roots` file.
    file: PathBuf,
    /// `HOME`, when it is an absolute path: it and the directories above it are too broad to trust.
    home: Option<PathBuf>,
    /// The store's directory, where it really lay when the store was located: every create,
    /// write, rename and remove is admitted against it.
    boundary: Boundary,
    /// The variable the store's directory comes from, `XDG_STATE_HOME`, `LOCALAPPDATA` or `HOME`, for messages.
    variable: &'static str,
}

impl PartialEq for Store {
    /// Stores are equal when they name the same file and `HOME`. The boundary and the variable
    /// are left out: they follow from the file's directory when each store was located.
    fn eq(&self, other: &Self) -> bool {
        self.file == other.file && self.home == other.home
    }
}

impl Eq for Store {}

impl Store {
    /// Lists `root`'s real path, unless it is already listed. Returns whether it was added.
    ///
    /// Effect: resolves `root` and `HOME`, creates the store's directory mode 0700, and under the
    /// store's lock ([`Store::lock`]) reads the store and, when adding, writes it atomically, mode
    /// 0600. A refused root touches nothing.
    ///
    /// # Errors
    ///
    /// Returns [`AddError::Broad`] when `root`'s real path is too broad ([`Store::too_broad`]), and
    /// [`AddError::Io`] when `root` can't be resolved, its real path holds a newline
    /// ([`io::ErrorKind::InvalidInput`]), on Windows isn't valid Unicode or holds a `\r`
    /// (both [`io::ErrorKind::InvalidInput`]), or the store or its lock can't be read or written.
    pub(crate) fn add(&self, root: &Path) -> Result<bool, AddError> {
        let real = root.canonicalize()?;
        #[cfg(windows)]
        if real.to_str().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root's real path isn't valid Unicode, which the trust store on Windows can't list",
            )
            .into());
        }
        #[cfg(windows)]
        if real.as_os_str().as_encoded_bytes().contains(&b'\r') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root's real path holds a carriage return, which the trust store on Windows can't list",
            )
            .into());
        }
        if real.as_os_str().as_encoded_bytes().contains(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the root's real path holds a newline, which the trust store can't list",
            )
            .into());
        }
        if Self::too_broad(&self.broad(), &real) {
            return Err(AddError::Broad(real));
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
        confine::create_dir_all(&self.admit(dir)?, 0o700)?;
        let path = self.admit(&beneath(dir, &fixed::TRUSTED_ROOTS_LOCK))?;
        let file = match confine::create_new(&path, 0o600) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                confine::open_existing(&self.admit(path.as_path())?)?
            }
            created => created?,
        };
        file.lock()?;
        Ok(file)
    }

    /// `path`, once [`Boundary::admit`] finds it inside the store's directory both as written
    /// and once every symlink along it is followed. Call it immediately before the write it
    /// guards.
    ///
    /// Effect: reads file metadata and symlinks along `path`.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::PermissionDenied`] when the variable the store's directory
    /// comes from holds a `..`, or `path` lies outside the store's directory, such as when that
    /// directory has been replaced by a symlink to elsewhere.
    fn admit(&self, path: &Path) -> io::Result<Admitted<'_>> {
        self.boundary
            .admit(path)
            .map_err(|_| self.refused().error(path))
    }

    /// Why [`Store::admit`] refuses a path: the variable holds a `..`, else the path leads
    /// outside the store's directory.
    fn refused(&self) -> Unread {
        self.climbing()
            .map_or(Unread::LeadsOutside(self.variable), Unread::Climbs)
    }

    /// The variable the store's directory comes from, `XDG_STATE_HOME`, `LOCALAPPDATA` or `HOME`, when its value
    /// holds a `..`: every read and write of the store is then refused.
    pub(crate) fn climbing(&self) -> Option<&'static str> {
        self.file
            .components()
            .any(|component| component == Component::ParentDir)
            .then_some(self.variable)
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

    /// The roots too broad to trust: `/`, and `HOME`'s real path with every directory above it.
    /// Only `/` when `HOME` is unset, isn't absolute or doesn't resolve.
    ///
    /// Effect: resolves `HOME`.
    fn broad(&self) -> Vec<PathBuf> {
        let home = self
            .home
            .as_deref()
            .and_then(|home| home.canonicalize().ok());
        std::iter::once(PathBuf::from("/"))
            .chain(
                home.iter()
                    .flat_map(|home| home.ancestors().map(Path::to_path_buf)),
            )
            .collect()
    }

    /// Whether `path` is too broad to trust: it is in `broad` ([`Store::broad`]), or it has no
    /// parent, as `/` on Unix and a drive root on Windows.
    fn too_broad(broad: &[PathBuf], path: &Path) -> bool {
        path.parent().is_none() || broad.contains(&path.to_path_buf())
    }

    /// What the store grants `root`: [`Grant::Trusted`] when its real path is listed, or it sits in a linked worktree
    /// ([`Place::linked`]) whose common git directory and path in its work tree equal a listed
    /// root's. A listed root counts for that only while it still resolves to itself: the store
    /// holds real paths, so one that resolves elsewhere has been replaced since it was listed. A
    /// root too broad to trust ([`Store::too_broad`]) is never trusted by either rule, and a listed one
    /// counts for nothing. A store [`Store::read_roots`] refuses grants [`Grant::Unread`]; a
    /// missing one, [`Grant::Untrusted`].
    ///
    /// Effect: reads the store, resolves `root` and `HOME`, and reads the `.git` files and git
    /// directories of `root` and of each listed root.
    fn grant(&self, root: &Path) -> Grant {
        let Ok(real) = root.canonicalize() else {
            return Grant::Untrusted;
        };
        let broad = self.broad();
        if Self::too_broad(&broad, &real) {
            return Grant::Untrusted;
        }
        let mut roots = match self.read_roots() {
            Ok(roots) => roots,
            Err(unread) => return Grant::Unread(unread),
        };
        roots.retain(|listed| !Self::too_broad(&broad, listed));
        let trusted = roots.contains(&real)
            || Place::linked(&real).is_some_and(|place| {
                roots.iter().any(|listed| {
                    listed.canonicalize().is_ok_and(|real| real == *listed)
                        && Place::of(listed).is_some_and(|listed| listed == place)
                })
            });
        if trusted {
            Grant::Trusted
        } else {
            Grant::Untrusted
        }
    }

    /// [`Store::read_roots`], with a refusal as the error a write of the store would give.
    ///
    /// Effect: reads file metadata and symlinks along the store's path, then the store.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::PermissionDenied`] when the variable the store's directory
    /// comes from holds a `..` or the store lies outside its directory, with
    /// [`io::ErrorKind::InvalidData`] when the store isn't a regular file, and otherwise when the
    /// store exists but can't be read.
    fn read(&self) -> io::Result<Vec<PathBuf>> {
        self.read_roots().map_err(|unread| unread.error(&self.file))
    }

    /// The listed roots, in order: each line that is an absolute path. None when the file is
    /// missing. The store is admitted ([`Store::admit`]) before it is read, so reads refuse
    /// exactly what writes refuse. Its type is checked before it is opened, and again once open,
    /// and it is opened without blocking, so a FIFO or socket there never hangs or slips past.
    /// Its symlinks are followed for the type: the admitted path already lies inside the store's
    /// directory once they are.
    ///
    /// Effect: reads file metadata and symlinks along the store's path, then the store.
    ///
    /// # Errors
    ///
    /// The [`Unread`] reason when the variable the store's directory comes from holds a `..`,
    /// the store leads outside its directory, it isn't a regular file, or it exists but can't be
    /// read.
    fn read_roots(&self) -> Result<Vec<PathBuf>, Unread> {
        let path = self
            .boundary
            .admit(&self.file)
            .map_err(|_| self.refused())?;
        let unreadable = |error: io::Error| Unread::Unreadable(self.variable, error.kind());
        match fs::metadata(path.as_path()) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Err(Unread::NotAFile(self.variable)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(unreadable(error)),
        }
        let mut file = match open_without_blocking(path.as_path()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(unreadable(error)),
        };
        if !file.metadata().map_err(unreadable)?.is_file() {
            return Err(Unread::NotAFile(self.variable));
        }
        let mut bytes = Vec::new();
        let _read = file.read_to_end(&mut bytes).map_err(unreadable)?;
        Ok(decode_lines(&bytes))
    }

    /// Replaces the store with `roots`, one per line: a temporary file in the same directory,
    /// created mode 0600, renamed over it. The caller holds the lock, which made the directory.
    /// The temporary file and the store are each admitted ([`Store::admit`]) just before they are
    /// created, renamed or removed.
    ///
    /// Effect: writes, then renames, a temporary file in the store's directory.
    ///
    /// # Errors
    ///
    /// Fails when the temporary file or the store lies outside the store's directory, or the file
    /// can't be written or renamed; a temporary file it made is then removed and the store left
    /// as it was.
    fn write(&self, roots: &[PathBuf]) -> io::Result<()> {
        let dir = self.dir()?;
        let name = format!(
            ".trusted-roots.{}.{}.tmp",
            std::process::id(),
            WRITES.fetch_add(1, Ordering::Relaxed)
        );
        let name = NameComponent::new(&name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the trust store's temporary file name is not one path component",
            )
        })?;
        let temp = beneath(dir, &name);
        let mut file = confine::create_new(&self.admit(&temp)?, 0o600)?;
        let written = (|| {
            for root in roots {
                file.write_all(root.as_os_str().as_encoded_bytes())?;
                file.write_all(b"\n")?;
            }
            file.sync_all()?;
            confine::rename_into(&self.admit(&temp)?, &self.admit(&self.file)?)
        })();
        if written.is_err() {
            drop(
                self.admit(&temp)
                    .and_then(|temp| confine::remove_file(&temp)),
            );
        }
        written
    }
}

/// The absolute paths `bytes` lists, one per line. On Unix each line's bytes are the path,
/// whatever they are.
#[cfg(unix)]
fn decode_lines(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|byte| *byte == b'\n')
        .map(|line| PathBuf::from(OsString::from_vec(line.to_vec())))
        .filter(|path| path.is_absolute())
        .collect()
}

/// The absolute paths `bytes` lists, one per line. A line that isn't UTF-8 is skipped, and a
/// trailing `\r`, which a Windows editor writes before `\n`, is dropped.
#[cfg(not(unix))]
fn decode_lines(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| std::str::from_utf8(line.strip_suffix(b"\r").unwrap_or(line)).ok())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect()
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
        let dot_git = beneath(top, &fixed::DOT_GIT);
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
        let dot_git = beneath(top, &fixed::DOT_GIT);
        if !fs::symlink_metadata(&dot_git).ok()?.is_file() {
            return None;
        }
        let line = first_line(&dot_git)?;
        let git_dir = Pointer::new(line.strip_prefix("gitdir: ")?)?
            .resolve_from(top)
            .canonicalize()
            .ok()?;
        let place = Self::of(real)?;
        let back =
            Pointer::new(&first_line(&beneath(&git_dir, &fixed::GITDIR))?)?.resolve_from(&git_dir);
        let names_back = back.canonicalize().ok()? == dot_git.canonicalize().ok()?;
        let in_worktrees =
            git_dir.parent() == Some(beneath(&place.common, &fixed::WORKTREES).as_path());
        (names_back && in_worktrees).then_some(place)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;
    use std::sync::Barrier;
    #[cfg(unix)]
    use std::sync::mpsc;
    use std::thread;
    #[cfg(unix)]
    use std::time::Duration;

    use super::*;
    use crate::cli::testing::symlink;

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
                home: Some(self.home().into()),
                local_app_data: None,
                user_profile: Some(self.home().into()),
                trust: trust.map(OsString::from),
            }
        }

        /// The `HOME` of [`Scratch::env`], which doesn't exist until a test makes it.
        fn home(&self) -> PathBuf {
            self.real().join("home")
        }

        /// The store of [`Scratch::env`].
        fn store(&self) -> Store {
            self.env(None)
                .store()
                .expect("an absolute XDG_STATE_HOME should locate the store")
        }
    }

    /// The filesystem root of `path`: `/` on Unix, the drive root on Windows.
    fn filesystem_root(path: &Path) -> PathBuf {
        path.ancestors()
            .last()
            .expect("a path should have an ancestor")
            .to_path_buf()
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
        // A relative path, because git rejects the `\\?\` verbatim form `canonicalize` gives on Windows.
        git(&repo, &["worktree", "add", "--quiet", "../wt"]);
        (repo, scratch.real().join("wt"))
    }

    #[test]
    fn a_listed_root_is_trusted() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        assert!(scratch.store().add(&root).expect("adding should work"));

        assert_eq!(granted(&root, &scratch.env(None)), Ok(Grant::Trusted));
    }

    #[test]
    fn an_unlisted_root_is_not_trusted() {
        let scratch = Scratch::new();
        let listed = scratch.mkdir("listed");
        let other = scratch.mkdir("other");
        assert!(scratch.store().add(&listed).expect("adding should work"));

        assert_eq!(granted(&other, &scratch.env(None)), Ok(Grant::Untrusted));
    }

    #[cfg(unix)]
    #[test]
    fn a_store_listing_the_root_reached_through_a_parent_component_grants_nothing() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        let dir = scratch.mkdir("state").join("supdoc");
        fs::create_dir_all(&dir).expect("the store's directory should be creatable");
        let mut listed = root.into_os_string();
        listed.push("\n");
        fs::write(dir.join("trusted-roots"), listed.as_bytes())
            .expect("the store should be writable by hand");
        let root = scratch.real().join("root");
        let climbing = TrustEnv {
            state_home: Some(scratch.mkdir("detour").join("..").join("state").into()),
            ..scratch.env(None)
        };

        assert_eq!(
            granted(&root, &scratch.env(None)),
            Ok(Grant::Trusted),
            "the same store reached without `..` should trust the root"
        );
        assert_eq!(
            granted(&root, &climbing),
            Ok(Grant::Unread(Unread::Climbs("XDG_STATE_HOME"))),
            "a store reached through `..` should grant nothing, naming the variable"
        );
    }

    #[test]
    fn a_store_file_symlinked_outside_its_directory_grants_nothing() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        let outside = scratch.mkdir("dotfiles").join("trusted-roots");
        fs::write(&outside, format!("{}\n", root.display()))
            .expect("the outside store should be writable");
        let dir = scratch.mkdir("state/supdoc");
        symlink(&outside, dir.join("trusted-roots")).expect("the store link should be creatable");

        assert_eq!(
            granted(&root, &scratch.env(None)),
            Ok(Grant::Unread(Unread::LeadsOutside("XDG_STATE_HOME"))),
            "a store file leading outside its directory should grant nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_store_file_grants_nothing_without_hanging() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        let fifo = scratch.mkdir("state/supdoc").join("trusted-roots");
        let made = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo should be runnable");
        assert!(made.success(), "mkfifo should make the store a FIFO");
        let env = scratch.env(None);
        let (sender, receiver) = mpsc::channel();

        let _reader = thread::spawn(move || sender.send(granted(&root, &env)));

        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("reading a FIFO store should not hang"),
            Ok(Grant::Unread(Unread::NotAFile("XDG_STATE_HOME")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_store_file_is_not_a_regular_file_and_never_hangs() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        let socket = scratch.mkdir("state/supdoc").join("trusted-roots");
        let _listener = std::os::unix::net::UnixListener::bind(&socket)
            .expect("a socket should be bindable at the store's path");
        let env = scratch.env(None);
        let (sender, receiver) = mpsc::channel();

        let _reader = thread::spawn(move || sender.send(granted(&root, &env)));

        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("reading a socket store should not hang"),
            Ok(Grant::Unread(Unread::NotAFile("XDG_STATE_HOME")))
        );
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

        assert_eq!(
            granted(&worktree.join("sub"), &scratch.env(None)),
            Ok(Grant::Trusted)
        );
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

        assert_eq!(granted(&worktree, &scratch.env(None)), Ok(Grant::Untrusted));
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

        assert_eq!(granted(&worktree, &scratch.env(None)), Ok(Grant::Trusted));
        assert_eq!(
            granted(&forged_common, &scratch.env(None)),
            Ok(Grant::Untrusted)
        );
        assert_eq!(
            granted(&forged_worktree, &scratch.env(None)),
            Ok(Grant::Untrusted)
        );
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

        assert_eq!(granted(&worktree, &scratch.env(None)), Ok(Grant::Trusted));
        assert_eq!(granted(&forged, &scratch.env(None)), Ok(Grant::Untrusted));
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

        assert_eq!(granted(&forged, &scratch.env(None)), Ok(Grant::Untrusted));
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

        assert_eq!(granted(&forged, &scratch.env(None)), Ok(Grant::Untrusted));
    }

    #[cfg(unix)]
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
            Ok(Ok(Grant::Untrusted)),
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

    // Windows has no `HOME` fallback and `/home/someone` isn't absolute there; its relative-variable
    // rule is `windows_ignores_a_relative_localappdata_and_unix_ignores_it_entirely`.
    #[cfg(unix)]
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
    fn windows_locates_the_store_under_localappdata_and_homes_at_userprofile() {
        let base = std::env::temp_dir();
        let env = TrustEnv::from_vars(|name| match name {
            "LOCALAPPDATA" => Some(base.join("local").into()),
            "USERPROFILE" => Some(base.join("profile").into()),
            "HOME" => Some(base.join("unix-home").into()),
            _ => None,
        });

        let store = env
            .store_on(true)
            .expect("an absolute LOCALAPPDATA should locate the store");

        assert_eq!(store.file, base.join("local/supdoc/trusted-roots"));
        assert_eq!(store.home, Some(base.join("profile")));
        assert_eq!(store.variable, "LOCALAPPDATA");
    }

    #[test]
    fn windows_does_not_fall_back_to_home_without_localappdata() {
        let base = std::env::temp_dir();
        let env = TrustEnv::from_vars(|name| (name == "HOME").then(|| base.clone().into()));

        assert_eq!(env.store_on(true), None);
    }

    #[test]
    fn windows_ignores_a_relative_localappdata_and_unix_ignores_it_entirely() {
        let base = std::env::temp_dir();
        let relative = TrustEnv::from_vars(|name| (name == "LOCALAPPDATA").then(|| "rel".into()));
        let absolute_only =
            TrustEnv::from_vars(|name| (name == "LOCALAPPDATA").then(|| base.join("local").into()));

        assert_eq!(relative.store_on(true), None);
        assert_eq!(absolute_only.store_on(false), None);
    }

    #[test]
    fn a_missing_store_is_untrusted_not_an_error() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");

        assert_eq!(granted(&root, &scratch.env(None)), Ok(Grant::Untrusted));
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
            .components()
            .skip_while(|component| !matches!(component, Component::Normal(_)))
            .collect::<PathBuf>()
            .display()
            .to_string();
        fs::write(&store.file, format!("{relative}\n\nroot\n"))
            .expect("the store should be writable");

        assert_eq!(granted(&root, &scratch.env(None)), Ok(Grant::Untrusted));
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
        assert_eq!(granted(&listed, &scratch.env(None)), Ok(Grant::Untrusted));
    }

    #[test]
    fn supdoc_trust_grants_for_1_and_not_for_0_empty_or_unset() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");

        assert_eq!(granted(&root, &scratch.env(Some("1"))), Ok(Grant::Trusted));
        assert_eq!(
            granted(&root, &scratch.env(Some("0"))),
            Ok(Grant::TurnedOff)
        );
        assert_eq!(granted(&root, &scratch.env(Some(""))), Ok(Grant::Untrusted));
        assert_eq!(granted(&root, &scratch.env(None)), Ok(Grant::Untrusted));
    }

    #[test]
    fn supdoc_trust_0_overrides_a_listed_root_and_empty_or_unset_consult_the_store() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        assert!(scratch.store().add(&root).expect("adding should work"));

        assert_eq!(
            granted(&root, &scratch.env(Some("0"))),
            Ok(Grant::TurnedOff)
        );
        assert_eq!(granted(&root, &scratch.env(Some(""))), Ok(Grant::Trusted));
        assert_eq!(granted(&root, &scratch.env(None)), Ok(Grant::Trusted));
        assert_eq!(granted(&root, &scratch.env(Some("1"))), Ok(Grant::Trusted));
    }

    #[test]
    fn adding_home_an_ancestor_of_home_or_the_filesystem_root_is_refused() {
        let scratch = Scratch::new();
        let home = scratch.mkdir("home");
        let store = scratch.store();

        let drive_root = filesystem_root(&scratch.real());
        for broad in [home.clone(), scratch.real(), drive_root] {
            let refused = store.add(&broad);
            assert!(
                matches!(&refused, Err(AddError::Broad(real)) if *real == broad),
                "{} should be refused as too broad: {refused:?}",
                broad.display()
            );
        }
        assert!(
            !store
                .file
                .parent()
                .expect("the store should have a directory")
                .exists(),
            "a refused root should leave no store behind"
        );
        // A verbatim path on Windows doesn't treat `..` as a parent.
        #[cfg(unix)]
        assert!(store.add(&home.join("..").join("home")).is_err());
        assert!(
            store
                .add(&scratch.mkdir("home/project"))
                .expect("adding should work")
        );
    }

    #[test]
    fn without_a_usable_home_only_the_filesystem_root_is_refused() {
        let scratch = Scratch::new();
        let unresolved = scratch.store();
        let mut unset = scratch.store();
        unset.home = None;

        let root = filesystem_root(&scratch.real());
        assert!(matches!(
            unset.add(&root),
            Err(AddError::Broad(real)) if real == root
        ));
        assert_eq!(unset.broad(), [PathBuf::from("/")]);
        assert_eq!(unresolved.broad(), [PathBuf::from("/")]);
        assert!(unresolved.add(&scratch.real()).expect("adding should work"));
    }

    #[test]
    fn a_home_that_is_a_linked_worktree_of_a_trusted_repo_is_not_trusted() {
        let scratch = Scratch::new();
        let (repo, worktree) = repo_with_worktree(&scratch);
        assert!(scratch.store().add(&repo).expect("adding should work"));
        let env = TrustEnv {
            home: Some(worktree.clone().into()),
            user_profile: Some(worktree.clone().into()),
            ..scratch.env(None)
        };

        assert_eq!(granted(&repo, &env), Ok(Grant::Trusted));
        assert_eq!(
            granted(&worktree, &env),
            Ok(Grant::Untrusted),
            "HOME should not inherit its repository's trust as a linked worktree"
        );
    }

    #[test]
    fn a_hand_listed_home_or_filesystem_root_grants_nothing() {
        let scratch = Scratch::new();
        let home = scratch.mkdir("home");
        let project = scratch.mkdir("home/project");
        let store = scratch.store();
        fs::create_dir_all(
            store
                .file
                .parent()
                .expect("the store should have a directory"),
        )
        .expect("the store directory should be creatable");
        fs::write(
            &store.file,
            format!("{}\n{}\n/\n", home.display(), scratch.real().display()),
        )
        .expect("the store should be writable");

        assert_eq!(granted(&home, &scratch.env(None)), Ok(Grant::Untrusted));
        assert_eq!(
            granted(&scratch.real(), &scratch.env(None)),
            Ok(Grant::Untrusted)
        );
        assert_eq!(
            granted(Path::new("/"), &scratch.env(None)),
            Ok(Grant::Untrusted)
        );
        assert_eq!(granted(&project, &scratch.env(None)), Ok(Grant::Untrusted));
    }

    #[test]
    fn supdoc_trust_yes_is_an_error_naming_the_accepted_values() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");

        let error = granted(&root, &scratch.env(Some("yes"))).expect_err("`yes` should be refused");

        assert_eq!(
            error.to_string(),
            "`SUPDOC_TRUST` is `yes`, which supdoc doesn't accept: set it to `1` to trust this \
             run, to `0` to turn trust off for this run, or leave it empty (or unset it) to leave \
             trust to `supdoc trust`"
        );
    }

    #[test]
    fn a_store_directory_replaced_by_a_symlink_after_locating_refuses_to_add_and_writes_nothing_there()
     {
        let scratch = Scratch::new();
        let first = scratch.mkdir("first");
        let root = scratch.mkdir("root");
        let elsewhere = scratch.mkdir("elsewhere");
        let store = scratch.store();
        assert!(store.add(&first).expect("adding should work"));
        let dir = scratch.real().join("state/supdoc");
        fs::rename(&dir, scratch.real().join("state/moved"))
            .expect("the store directory should move aside");
        symlink(&elsewhere, &dir).expect("the swapped-in symlink should be creatable");

        let error = store
            .add(&root)
            .expect_err("adding through a swapped-in symlink should fail");

        assert!(
            matches!(&error, AddError::Io(io) if io.kind() == io::ErrorKind::PermissionDenied),
            "{error:?}"
        );
        let written: Vec<_> = fs::read_dir(&elsewhere)
            .expect("the symlink's target should be listable")
            .collect();
        assert!(
            written.is_empty(),
            "nothing should be written where the symlink leads: {written:?}"
        );
    }

    // A verbatim path on Windows doesn't treat `..` as a parent, so there is no `..` after a symlink.
    #[cfg(unix)]
    #[test]
    fn a_state_home_with_dot_dot_after_a_symlink_refuses_to_revoke_rather_than_report_it_done() {
        let scratch = Scratch::new();
        let project = scratch.mkdir("proj");
        let deep = scratch.mkdir("deep/x");
        let kernel_store = scratch.mkdir("deep/state/supdoc").join("trusted-roots");
        fs::write(&kernel_store, format!("{}\n", project.display()))
            .expect("the store should be writable");
        symlink(&deep, scratch.real().join("link")).expect("the link should be creatable");
        let env = TrustEnv {
            state_home: Some(scratch.real().join("link/../state").into()),
            ..scratch.env(None)
        };
        let store = env
            .store()
            .expect("an absolute XDG_STATE_HOME should locate the store");

        let revoked = store.revoke(&project);
        let added = store.add(&scratch.mkdir("other"));

        let error = revoked.expect_err("revoking through `..` should fail, never report done");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            crate::cli::cause(&error),
            "`XDG_STATE_HOME` holds `..`, which the trust store's path must not contain: set it \
             to a path without `..`"
        );
        assert!(matches!(added, Err(AddError::Io(_))), "{added:?}");
        assert_eq!(
            granted(&project, &env),
            Ok(Grant::Unread(Unread::Climbs("XDG_STATE_HOME"))),
            "reading through `..` should grant nothing, as writing through it is refused"
        );
        assert_eq!(
            fs::read_to_string(&kernel_store).expect("the store should be readable"),
            format!("{}\n", project.display())
        );
        assert!(!scratch.real().join("state").exists());
    }

    #[test]
    fn a_refusal_names_the_store_path_with_its_case_kept() {
        let scratch = Scratch::new();
        let root = scratch.mkdir("root");
        let outside = scratch.mkdir("outside").join("roots");
        let dir = scratch.mkdir("MixedCase/state/supdoc");
        symlink(&outside, dir.join("trusted-roots"))
            .expect("the store symlink should be creatable");
        let env = TrustEnv {
            state_home: Some(scratch.real().join("MixedCase/state").into()),
            ..scratch.env(None)
        };
        let store = env
            .store()
            .expect("an absolute XDG_STATE_HOME should locate the store");

        let error = match store.add(&root) {
            Err(AddError::Io(error)) => error,
            other => panic!("a store symlinked outside should be refused: {other:?}"),
        };

        assert_eq!(
            crate::cli::cause(&error),
            format!(
                "`{}` lies outside the trust store's directory once symlinks are followed",
                dir.join("trusted-roots").display()
            )
        );
        assert!(
            !outside.exists(),
            "nothing should be written where the symlink leads"
        );
    }

    #[cfg(unix)]
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
