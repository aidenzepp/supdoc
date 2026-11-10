//! The hardened spawn of a tool on one block: stdin, a working directory, a timeout and an
//! output cap.
//!
//! A tool is spawned by the absolute path its lookup returned, never through a shell, `cargo`,
//! `python -m` or `npx`. Its environment holds only [`ToolEnv`]'s variables and the call's own
//! extras. It runs in its own process group, in an empty supdoc-owned directory unless its call
//! asks for the project root, and the block reaches it on stdin only.

use std::ffi::OsStr;
use std::io::{Read, Write as _};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(windows)]
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::tools::ToolEnv;
use crate::config::confine::{self, Boundary};
use crate::path::AbsolutePath;
use crate::process::{self, LiveGroup, Spawning, TERMINATING};

/// The most bytes supdoc reads from each of a tool's stdout and stderr, unless its call sets its
/// own cap.
pub(crate) const CAP: usize = 1024 * 1024;

/// The Windows process-creation flag that gives a tool its own process group.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How often a tool whose streams have closed is polled for its exit.
const POLL: Duration = Duration::from_millis(5);

/// Where a tool call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cwd<'call> {
    /// The empty supdoc-owned directory [`neutral_dir`] picks, outside the root.
    Neutral,
    /// The project root's real path.
    Root,
    /// A private directory supdoc made beside [`neutral_dir`] for this tool, such as the one
    /// holding the config tombi finds by walking up from where it runs. Accepted only when it
    /// passes [`private_dir`].
    Private(&'call Path),
}

/// One tool call: the binary, its arguments and extra variables, the block it reads, where it
/// runs and how much it may print.
#[derive(Debug)]
pub(crate) struct Call<'call> {
    /// The binary's absolute real path, as the lookup returned it.
    pub(crate) program: &'call Path,
    /// The tool's own name, passed as `argv[0]`, so a multi-call binary reached through a
    /// symlink (a rustup proxy) runs as the tool it was looked up as.
    pub(crate) name: &'call str,
    /// The arguments, after the program.
    pub(crate) args: &'call [&'call OsStr],
    /// Variables the call needs beyond [`ToolEnv`]'s.
    pub(crate) extra: &'call [(&'call str, &'call OsStr)],
    /// The bytes written to the tool's stdin, which is then closed.
    pub(crate) stdin: &'call [u8],
    /// How long the tool may run. `None` is no limit.
    pub(crate) timeout: Option<Duration>,
    /// The directory the tool runs in.
    pub(crate) cwd: Cwd<'call>,
    /// The most bytes read from each of stdout and stderr; [`CAP`] for a tool on a block.
    pub(crate) cap: usize,
}

/// What a tool that ran to its exit printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ran {
    /// Whether it exited with status 0.
    pub(crate) success: bool,
    /// Everything it wrote to stdout.
    pub(crate) stdout: Vec<u8>,
    /// Everything it wrote to stderr.
    pub(crate) stderr: Vec<u8>,
}

/// Why a tool didn't run to an exit supdoc can judge.
#[derive(Debug)]
pub(crate) enum Failure {
    /// It couldn't be started.
    Spawn(std::io::Error),
    /// It passed its timeout and was killed.
    TimedOut,
    /// It wrote more than its cap to a stream and was killed.
    OverCap {
        /// The stream: `stdout` or `stderr`.
        stream: &'static str,
        /// The cap it passed, in bytes.
        cap: usize,
    },
}

/// Runs `call` with `env`'s variables, from [`neutral_dir`] judged against `root` or from
/// `root`'s real path, as its [`Cwd`] says.
///
/// Effect: resolves or creates the working directory, spawns the tool and kills its process
/// group once it exits, passes its timeout or writes past its cap.
///
/// # Errors
///
/// [`Failure`] when the root doesn't resolve or the tool can't start, passes its timeout, or
/// writes past its cap.
pub(crate) fn run(call: &Call<'_>, env: &ToolEnv, root: &Path) -> Result<Ran, Failure> {
    let cwd = working_dir(call.cwd, env, root)?;
    let deadline = call.timeout.map(|timeout| Instant::now() + timeout);
    // No new tool once a termination signal has arrived, and until the group is registered the
    // signal thread can't copy the live groups, so it can't miss this tool.
    let (spawning, mut child) = spawn_tool(call, env, cwd).map_err(Failure::Spawn)?;
    // Held until this function returns, by which point every path has reaped the tool.
    let _live = LiveGroup::register(child.id());
    // Released before any wait on the tool, so the signal thread never waits on one.
    drop(spawning);
    // A signal may have landed after the check above. Its thread sets `TERMINATING`, then waits
    // for this spawn to end before it copies the groups it kills. So this group is in its copy,
    // and this load may also see the flag, in which case the group is killed here as well.
    if process::ends_by_signal(&TERMINATING) {
        end_by_signal(child);
    }
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = call.stdin.to_vec();
        drop(thread::spawn(move || {
            // A tool that exits without reading its stdin closes the pipe: nothing to report.
            let _closed = stdin.write_all(&bytes);
        }));
    }
    let cap = call.cap;
    let (sender, reads) = mpsc::channel();
    let streams: [(&'static str, Option<Box<dyn Read + Send>>); 2] = [
        (
            "stdout",
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
        (
            "stderr",
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
    ];
    for (name, stream) in streams {
        let sender = sender.clone();
        drop(thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(stream) = stream {
                let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
                let _partial = stream.take(limit).read_to_end(&mut bytes);
            }
            let _gone = sender.send((name, bytes));
        }));
    }
    drop(sender);
    let mut stdout = None;
    let mut stderr = None;
    while stdout.is_none() || stderr.is_none() {
        let received = match deadline {
            Some(deadline) => reads
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|error| matches!(error, mpsc::RecvTimeoutError::Timeout)),
            None => reads.recv().map_err(|_| false),
        };
        let (name, bytes) = match received {
            Ok(read) => read,
            Err(true) => return Err(kill(child, Failure::TimedOut)),
            Err(false) => break,
        };
        if bytes.len() > cap {
            return Err(kill(child, Failure::OverCap { stream: name, cap }));
        }
        if name == "stdout" {
            stdout = Some(bytes);
        } else {
            stderr = Some(bytes);
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Anything the tool left running in its group, holding no stream, dies with it.
                kill_group(child.id());
                break status;
            }
            Ok(None) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                return Err(kill(child, Failure::TimedOut));
            }
            Ok(None) => thread::sleep(POLL),
            Err(error) => return Err(kill(child, Failure::Spawn(error))),
        }
    };
    Ok(Ran {
        success: status.success(),
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    })
}

/// The directory a call with `cwd` runs in: [`neutral_dir`] judged against `root`, `root`'s
/// real path, or a private directory's real path once [`private_dir`] accepts it.
///
/// Effect: creates the neutral directory, or resolves `root` or the private directory.
///
/// # Errors
///
/// [`Failure::Spawn`] when `cwd` is [`Cwd::Root`] and `root` doesn't resolve, or is a
/// [`Cwd::Private`] directory [`private_dir`] refuses.
fn working_dir(cwd: Cwd<'_>, env: &ToolEnv, root: &Path) -> Result<PathBuf, Failure> {
    match cwd {
        Cwd::Neutral => Ok(neutral_dir(env, root)),
        Cwd::Root => root.canonicalize().map_err(Failure::Spawn),
        Cwd::Private(dir) => private_dir(env, root, dir).ok_or_else(|| {
            Failure::Spawn(std::io::Error::other(
                "its working directory isn't a private directory supdoc made",
            ))
        }),
    }
}

/// The real path of `dir` when supdoc may run a tool there: a directory under the real path of
/// supdoc's code-block cache directory (the one [`neutral_dir`] sits in), outside `root`, that
/// only its owner reaches (mode 0700 or narrower) and that the neutral directory's owner owns.
/// `None` otherwise, and when there is no neutral directory.
///
/// On Windows the mode and owner checks are skipped, as std reads neither; every other check
/// stays. The cache directory's per-user location is the defence there, so a cache the operator
/// points at a directory other users can write has no ownership check.
///
/// Effect: creates the neutral directory and reads metadata along both paths.
#[must_use]
pub(crate) fn private_dir(env: &ToolEnv, root: &Path, dir: &Path) -> Option<PathBuf> {
    let neutral = neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let cache = neutral.canonicalize().ok()?.parent()?.to_path_buf();
    let real = dir.canonicalize().ok()?;
    let real_root = root.canonicalize().ok()?;
    let meta = std::fs::symlink_metadata(&real).ok()?;
    let owned = reaches_only_owner(&meta, &std::fs::metadata(&cache).ok()?);
    let private = real.starts_with(&cache)
        && real != cache
        && !real.starts_with(&real_root)
        && meta.is_dir()
        && owned;
    private.then_some(real)
}

/// Starts the tool `call` names in `cwd`, as the leader of its own process group, and returns the
/// [`Spawning`] that covers the spawn with it. The caller drops that once the group is registered.
///
/// # Errors
///
/// The spawn failure, such as a program that does not exist.
///
/// Effect: spawns the tool, sleeping between tries of a busy executable.
fn spawn_tool(call: &Call<'_>, env: &ToolEnv, cwd: PathBuf) -> std::io::Result<(Spawning, Child)> {
    let mut command = Command::new(call.program);
    // Unix runs a multi-call binary reached through a symlink as the tool it was looked up as.
    // Windows has no `arg0`: its proxies are hard links named per tool, so the program path
    // already carries the tool name.
    #[cfg(unix)]
    let _same = command.arg0(call.name);
    let _same = command
        .args(call.args)
        .env_clear()
        .envs(env.vars())
        .envs(call.extra.iter().copied())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The tool leads its own group, so killing the group reaches its children and the console's
    // Ctrl+C does not: supdoc kills it.
    #[cfg(unix)]
    let _same = command.process_group(0);
    #[cfg(windows)]
    let _same = command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    process::retry_busy(
        Spawning::begin,
        || command.spawn(),
        || thread::sleep(process::BUSY_DELAY),
    )
}

/// Whether `meta` is mode 0700 or narrower and owned by whoever owns `cache`.
#[cfg(unix)]
fn reaches_only_owner(meta: &std::fs::Metadata, cache: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    meta.permissions().mode().trailing_zeros() >= 6 && meta.uid() == cache.uid()
}

/// Always true: std reads no mode or owner on this platform.
#[cfg(not(unix))]
fn reaches_only_owner(_meta: &std::fs::Metadata, _cache: &std::fs::Metadata) -> bool {
    true
}

/// Whether `meta` is mode 0700 or narrower.
#[cfg(unix)]
fn mode_is_private(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode().trailing_zeros() >= 6
}

/// Always true: std reads no mode on this platform.
#[cfg(not(unix))]
fn mode_is_private(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Kills `child`, a tool spawned as a termination signal landed, with its process group, then
/// waits for the signal to end supdoc.
///
/// Effect: signals the tool's process group, reaps the tool and blocks for good.
fn end_by_signal(mut child: Child) -> ! {
    kill_tool_tree(&mut child);
    let _reaped = child.wait();
    process::wait_to_be_ended()
}

/// Kills `child` and, best effort, its whole process group, reaps it, and returns `failure`. The
/// stream readers are left to end on their own, never joined.
///
/// Effect: signals the tool's process group and waits for the tool itself to exit.
fn kill(mut child: Child, failure: Failure) -> Failure {
    kill_tool_tree(&mut child);
    let _reaped = child.wait();
    failure
}

/// Kills `child` and its process tree. On Windows the tree kill runs first, while `child` is
/// alive, because `taskkill /T` walks from a live parent; on Unix the group kill reaches the
/// group after the leader is gone.
///
/// Effect: signals the tool and its process group.
fn kill_tool_tree(child: &mut Child) {
    #[cfg(windows)]
    kill_group(child.id());
    let _gone = child.kill();
    #[cfg(unix)]
    kill_group(child.id());
}

/// Best effort, kills the tool `pgid` and its descendants. A group already empty is not an error.
///
/// Effect: signals the process group.
fn kill_group(pgid: u32) {
    process::kill_tree(pgid);
}

/// The cache bases `env` names, in order: `$XDG_CACHE_HOME`, then on Windows
/// `%LOCALAPPDATA%` (a per-user directory), else on Unix `$HOME/.cache`. A value that isn't
/// absolute or holds a `..` is dropped: the kernel resolves a `..` after symlinks but a lexical
/// join folds it away, so a check and a later write could name two places.
pub(crate) fn cache_bases(env: &ToolEnv) -> impl Iterator<Item = PathBuf> {
    let windows = cfg!(windows);
    let var = |name: &str| {
        env.var(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && !holds_parent(path))
    };
    [
        var("XDG_CACHE_HOME"),
        if windows {
            var("LOCALAPPDATA")
        } else {
            var("HOME").map(|home| home.join(".cache"))
        },
    ]
    .into_iter()
    .flatten()
}

/// The working directory a tool runs in: `cwd` under the first of [`cache_bases`] where it is
/// usable, beneath `supdoc/code-blocks`, created mode 0700. `/` when none is usable: inside
/// `root`, not a directory only its owner reaches, or not empty.
///
/// Effect: creates the directory and reads its metadata and entries.
#[must_use]
pub(crate) fn neutral_dir(env: &ToolEnv, root: &Path) -> PathBuf {
    cache_bases(env)
        .map(|cache| (cache.join("supdoc/code-blocks/cwd"), cache))
        .find(|(dir, cache)| usable(cache, dir, root))
        .map_or_else(|| PathBuf::from("/"), |(dir, _)| dir)
}

/// Whether `path` has a `..` component.
fn holds_parent(path: &Path) -> bool {
    path.components()
        .any(|component| component == std::path::Component::ParentDir)
}

/// Whether `dir`, created mode 0700 with its missing parents admitted beneath `cache`, is an empty
/// directory only its owner reaches, outside `root`. Nothing is created unless the deepest
/// existing ancestor of `dir` is outside `root`.
///
/// Effect: reads metadata along `dir`, then creates the directory and reads its entries.
fn usable(cache: &Path, dir: &Path, root: &Path) -> bool {
    let Ok(real_root) = root.canonicalize() else {
        return false;
    };
    let existing = dir
        .ancestors()
        .find_map(|ancestor| ancestor.canonicalize().ok());
    if existing.is_none_or(|existing| existing.starts_with(&real_root)) {
        return false;
    }
    let (Some(cache), Some(given)) = (exact(cache), exact(dir)) else {
        return false;
    };
    if create_private(&Boundary::resolve(&cache), &given).is_none() {
        return false;
    }
    let Ok(real) = dir.canonicalize() else {
        return false;
    };
    !real.starts_with(&real_root)
        && std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

/// `path` as an [`AbsolutePath`] when normalizing doesn't move it: it is absolute and holds no
/// `..`, so a boundary built from it names the place that was checked. `None` otherwise.
#[must_use]
pub(crate) fn exact(path: &Path) -> Option<AbsolutePath> {
    AbsolutePath::new(path).filter(|exact| exact.as_path() == path)
}

/// `dir` as a boundary, once it is created mode 0700 with its missing parents, admitted beneath
/// `parent`, and found to be a directory (not a link) only its owner reaches. `None` when it isn't
/// admitted, can't be created, or isn't such a directory.
///
/// On Windows the mode check is skipped, as std reads no mode; the directory inherits its
/// parent's ACL and the real-directory check stays.
///
/// Effect: creates the directory and reads its metadata.
#[must_use]
pub(crate) fn create_private(parent: &Boundary, dir: &AbsolutePath) -> Option<Boundary> {
    let admitted = parent.admit(dir).map_err(|e| eprintln!("DIAG create_private admit {:?}: {e:?}", dir.as_path())).ok()?;
    confine::create_dir_all(&admitted, 0o700).map_err(|e| diag("create_dir_all", dir.as_path(), &e)).ok()?;
    let meta = std::fs::symlink_metadata(dir.as_path()).map_err(|e| diag("symlink_metadata", dir.as_path(), &e)).ok()?;
    if !(meta.is_dir() && mode_is_private(&meta)) {
        eprintln!("DIAG create_private not a private dir {:?}: is_dir={} private={}", dir.as_path(), meta.is_dir(), mode_is_private(&meta));
    }
    (meta.is_dir() && mode_is_private(&meta)).then(|| Boundary::resolve(dir))
}

/// Writes `text` to `file`, admitted beneath the private directory `dir`, mode 0600, unless it is
/// already a file holding exactly that. Anything else at the name is replaced through a temporary
/// file beside it renamed into place, so concurrent runs never see half a file. `None` when it
/// isn't admitted or can't be written.
///
/// On Windows the file gets no explicit permissions and inherits the private directory's ACL.
///
/// Effect: may create a temporary file and replace `file`.
#[must_use]
pub(crate) fn write_private(dir: &Boundary, file: &AbsolutePath, text: &str) -> Option<()> {
    let current = std::fs::symlink_metadata(file.as_path())
        .ok()
        .filter(std::fs::Metadata::is_file)
        .and_then(|_| std::fs::read_to_string(file.as_path()).ok());
    if current.as_deref() == Some(text) {
        return Some(());
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;
        Some(std::fs::Permissions::from_mode(0o600))
    };
    #[cfg(not(unix))]
    let mode: Option<std::fs::Permissions> = None;
    let admitted = dir.admit(file).map_err(|e| eprintln!("DIAG write_private admit {:?}: {e:?}", file.as_path())).ok()?;
    confine::write_atomically(&admitted, text.as_bytes(), mode.as_ref()).map_err(|e| diag("write_atomically", file.as_path(), &e)).ok()
}

/// Prints a dropped `io::Error` in full. Diagnostic only.
fn diag(call: &str, path: &Path, error: &std::io::Error) {
    eprintln!(
        "DIAG {call} failed: kind={:?} raw_os_error={:?} message={error} path={path:?}",
        error.kind(),
        error.raw_os_error()
    );
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io::ErrorKind;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{
        CAP, Call, Cwd, Failure, Ran, create_private, exact, neutral_dir, private_dir, run,
        write_private,
    };
    use crate::code_blocks::tools::ToolEnv;
    use crate::config::confine::Boundary;
    use crate::process::{BUSY_ATTEMPTS, retry_busy};

    /// A fresh directory holding a `root` project, a `home` and a `bin` with the stub `script`.
    struct Fixture {
        /// Keeps the directory alive.
        _dir: tempfile::TempDir,
        /// The project root.
        root: PathBuf,
        /// The stub's path.
        stub: PathBuf,
        /// The environment the tool gets: `PATH` and `HOME`.
        env: ToolEnv,
    }

    /// Writes `script` as an executable `#!/bin/sh` stub outside a fresh root.
    #[cfg(unix)]
    fn fixture(script: &str) -> Fixture {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = dir.path().join("root");
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        for path in [&root, &home, &bin] {
            fs::create_dir_all(path).expect("the fixture directory should be creatable");
        }
        let stub = bin.join("tool");
        fs::write(&stub, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        let vars: [(&str, OsString); 2] = [
            ("PATH", OsString::from("/bin:/usr/bin")),
            ("HOME", home.into_os_string()),
        ];
        let env = ToolEnv::from_vars(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        });
        Fixture {
            _dir: dir,
            root,
            stub,
            env,
        }
    }

    /// Runs the fixture's stub on `stdin` with `extra` and `timeout`, from the neutral directory
    /// with the default cap.
    fn run_stub(
        fixture: &Fixture,
        stdin: &[u8],
        extra: &[(&str, &OsStr)],
        timeout: Option<Duration>,
    ) -> Result<Ran, Failure> {
        run_stub_with(fixture, stdin, extra, timeout, Cwd::Neutral, CAP)
    }

    /// Runs the fixture's stub on `stdin` with `extra`, `timeout`, `cwd` and `cap`.
    fn run_stub_with(
        fixture: &Fixture,
        stdin: &[u8],
        extra: &[(&str, &OsStr)],
        timeout: Option<Duration>,
        cwd: Cwd<'_>,
        cap: usize,
    ) -> Result<Ran, Failure> {
        let call = Call {
            program: &fixture.stub,
            name: "tool",
            args: &[],
            extra,
            stdin,
            timeout,
            cwd,
            cap,
        };
        run(&call, &fixture.env, &fixture.root)
    }

    fn busy() -> std::io::Error {
        std::io::Error::from(ErrorKind::ExecutableFileBusy)
    }

    #[test]
    fn a_busy_spawn_is_retried_until_it_succeeds() {
        let mut tries = 0;

        let result = retry_busy(
            || (),
            || {
                tries += 1;
                if tries <= 2 { Err(busy()) } else { Ok(tries) }
            },
            || (),
        );

        assert_eq!(result.expect("the third try succeeds").1, 3);
        assert_eq!(tries, 3, "busy twice then Ok is three tries");
    }

    #[test]
    fn a_spawn_busy_past_the_limit_returns_the_busy_error() {
        let mut tries = 0;

        let result: std::io::Result<((), ())> = retry_busy(
            || (),
            || {
                tries += 1;
                Err(busy())
            },
            || (),
        );

        let error = result.expect_err("every try is busy");
        assert_eq!(error.kind(), ErrorKind::ExecutableFileBusy);
        assert_eq!(tries, BUSY_ATTEMPTS, "the tries stop at the limit");
    }

    #[test]
    fn any_other_spawn_error_returns_without_a_retry() {
        let mut tries = 0;

        let result: std::io::Result<((), ())> = retry_busy(
            || (),
            || {
                tries += 1;
                Err(std::io::Error::from(ErrorKind::NotFound))
            },
            || (),
        );

        let error = result.expect_err("not found is not retried");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(tries, 1, "a non-busy error is tried once");
    }

    /// A guard that counts how many are alive.
    struct Counted<'live>(&'live std::cell::Cell<u32>);

    impl<'live> Counted<'live> {
        fn begin(live: &'live std::cell::Cell<u32>) -> Self {
            live.set(live.get() + 1);
            Self(live)
        }
    }

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    #[test]
    fn no_guard_is_alive_while_a_busy_spawn_pauses() {
        let live = std::cell::Cell::new(0);
        let mut pauses = 0;
        let mut tries = 0;

        let result = retry_busy(
            || Counted::begin(&live),
            || {
                tries += 1;
                assert_eq!(live.get(), 1, "each try runs under exactly one guard");
                if tries <= 2 { Err(busy()) } else { Ok(()) }
            },
            || {
                pauses += 1;
                assert_eq!(live.get(), 0, "a pause must not run under a guard");
            },
        );

        let (guard, ()) = result.expect("the third try succeeds");
        assert_eq!(pauses, 2, "two busy tries pause twice");
        assert_eq!(
            live.get(),
            1,
            "the successful try's guard is returned alive"
        );
        drop(guard);
        assert_eq!(live.get(), 0, "dropping it ends the last guard");
    }

    #[cfg(unix)]
    #[test]
    fn stdin_arrives_byte_exact() {
        let fixture = fixture("cat");
        let block = b"echo  'hi'\r\n\tx\0y\n\xff";

        let ran =
            run_stub(&fixture, block, &[], Some(Duration::from_secs(5))).expect("cat should run");

        assert!(ran.success);
        assert_eq!(ran.stdout, block);
    }

    #[cfg(unix)]
    #[test]
    fn only_the_tool_env_and_the_extras_reach_the_tool() {
        // `cargo test` sets CARGO_PKG_NAME for this process; the stub must not see it.
        let fixture = fixture("env");
        let leaked = std::env::var_os("CARGO_PKG_NAME").is_some();

        let ran = run_stub(
            &fixture,
            b"",
            &[("SUPDOC_EXTRA", OsStr::new("1"))],
            Some(Duration::from_secs(5)),
        )
        .expect("env should run");
        let printed = String::from_utf8_lossy(&ran.stdout);
        let mut names: Vec<&str> = printed
            .lines()
            .filter_map(|line| line.split('=').next())
            .filter(|name| *name != "PWD" && *name != "SHLVL" && *name != "_")
            .collect();
        names.sort_unstable();

        assert!(
            leaked,
            "the test process should have CARGO_PKG_NAME to leak"
        );
        assert_eq!(names, ["HOME", "PATH", "SUPDOC_EXTRA"], "{printed}");
    }

    #[test]
    fn a_cache_base_holding_dot_dot_after_a_link_is_refused_and_nothing_is_created() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let place = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let deep = place.path().join("elsewhere/deep");
        fs::create_dir_all(&deep).expect("the link's target");
        crate::cli::testing::symlink(&deep, place.path().join("link")).expect("the planted link");
        let base = place.path().join("link/..");
        let env =
            ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| base.clone().into()));

        let neutral = neutral_dir(&env, root.path());

        assert_eq!(neutral, PathBuf::from("/"));
        for at in [
            place.path().join("supdoc"),
            place.path().join("elsewhere/supdoc"),
        ] {
            assert!(!at.exists(), "nothing should be created at {at:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_cache_base_is_under_localappdata_not_home() {
        let base = std::env::temp_dir();
        let env = ToolEnv::from_vars(|name| match name {
            "LOCALAPPDATA" | "HOME" => Some(base.clone().into()),
            _ => None,
        });

        let bases: Vec<_> = super::cache_bases(&env).collect();

        assert_eq!(bases, [base]);
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_cache_base_is_under_home_and_ignores_localappdata() {
        let base = std::env::temp_dir();
        let env = ToolEnv::from_vars(|name| match name {
            "LOCALAPPDATA" | "HOME" => Some(base.clone().into()),
            _ => None,
        });

        let bases: Vec<_> = super::cache_bases(&env).collect();

        assert_eq!(bases, [base.join(".cache")]);
    }

    /// A scratch place holding `real` (a plain directory), `elsewhere/deep` and `link` (a link
    /// to it), with the two hostile `HOME` values `real/..` and `link/..`.
    fn hostile_homes() -> (tempfile::TempDir, [PathBuf; 2]) {
        let place = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir(place.path().join("real")).expect("the plain directory");
        fs::create_dir_all(place.path().join("elsewhere/deep")).expect("the link's target");
        crate::cli::testing::symlink(
            place.path().join("elsewhere/deep"),
            place.path().join("link"),
        )
        .expect("the planted link");
        let homes = [place.path().join("real/.."), place.path().join("link/..")];
        (place, homes)
    }

    /// Every path beneath `dir`, without following links.
    fn every_path(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in fs::read_dir(&next).expect("the scratch place should be listable") {
                let path = entry.expect("the scratch entry should be readable").path();
                if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                    pending.push(path.clone());
                }
                found.push(path);
            }
        }
        found.sort();
        found
    }

    #[test]
    fn a_home_holding_dot_dot_gives_no_neutral_or_private_directory_and_creates_nothing() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let (place, homes) = hostile_homes();
        let before = every_path(place.path());
        for home in homes {
            let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.clone().into()));

            let neutral = neutral_dir(&env, root.path());
            let private = private_dir(&env, root.path(), &place.path().join("real"));

            assert_eq!(neutral, PathBuf::from("/"), "HOME={}", home.display());
            assert_eq!(private, None, "HOME={}", home.display());
            assert_eq!(
                every_path(place.path()),
                before,
                "HOME={} created something",
                home.display()
            );
        }
    }

    #[test]
    fn a_plain_cache_base_gives_a_neutral_directory_under_it() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let base = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let path = base.path().to_path_buf();
        let env =
            ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| path.clone().into()));

        let neutral = neutral_dir(&env, root.path());

        assert_eq!(neutral, base.path().join("supdoc/code-blocks/cwd"));
    }

    #[cfg(unix)]
    #[test]
    fn the_working_directory_is_neutral_and_outside_the_root() {
        let fixture = fixture("pwd -P");

        let ran =
            run_stub(&fixture, b"", &[], Some(Duration::from_secs(5))).expect("pwd should run");
        let cwd = PathBuf::from(String::from_utf8_lossy(&ran.stdout).trim());
        let root = fixture
            .root
            .canonicalize()
            .expect("the root should resolve");

        assert!(!cwd.starts_with(&root), "{cwd:?} is inside {root:?}");
        assert!(cwd.ends_with("supdoc/code-blocks/cwd"), "{cwd:?}");
        let mode = fs::metadata(&cwd)
            .expect("the cwd should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_root_call_runs_from_the_roots_real_path() {
        let fixture = fixture("pwd -P");

        let ran = run_stub_with(
            &fixture,
            b"",
            &[],
            Some(Duration::from_secs(5)),
            Cwd::Root,
            CAP,
        )
        .expect("pwd should run");
        let cwd = PathBuf::from(String::from_utf8_lossy(&ran.stdout).trim());
        let root = fixture
            .root
            .canonicalize()
            .expect("the root should resolve");

        assert_eq!(cwd, root);
    }

    /// A 0700 directory `name` beside the fixture's neutral directory, made as supdoc makes one.
    #[cfg(unix)]
    fn private_beside_neutral(fixture: &Fixture, name: &str) -> PathBuf {
        use std::os::unix::fs::DirBuilderExt as _;
        let neutral = neutral_dir(&fixture.env, &fixture.root);
        let dir = neutral
            .parent()
            .expect("the neutral directory has a parent")
            .join(name);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("the private directory should be creatable");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_private_call_runs_from_its_directory_beside_the_neutral_one() {
        let fixture = fixture("pwd -P");
        let dir = private_beside_neutral(&fixture, "tombi-config/abc");

        let ran = run_stub_with(
            &fixture,
            b"",
            &[],
            Some(Duration::from_secs(5)),
            Cwd::Private(&dir),
            CAP,
        )
        .expect("pwd should run");

        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&ran.stdout).trim()),
            dir.canonicalize().expect("the directory should resolve")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_private_directory_outside_the_cache_open_to_others_or_in_the_root_is_refused() {
        let fixture = fixture("pwd -P");
        let open = private_beside_neutral(&fixture, "open");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755))
            .expect("the mode should be settable");
        let elsewhere = fixture
            .root
            .parent()
            .expect("the root has a parent")
            .join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("the directory should be creatable");
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700))
            .expect("the mode should be settable");

        for dir in [&open, &elsewhere, &fixture.root] {
            let ran = run_stub_with(
                &fixture,
                b"",
                &[],
                Some(Duration::from_secs(5)),
                Cwd::Private(dir),
                CAP,
            );

            assert!(
                matches!(ran, Err(Failure::Spawn(_))),
                "{dir:?} was accepted: {ran:?}"
            );
            assert_eq!(private_dir(&fixture.env, &fixture.root, dir), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_home_inside_the_root_falls_back_to_slash_and_creates_nothing() {
        let fixture = fixture("true");
        let home = fixture.root.join("home");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.clone().into()));

        assert_eq!(neutral_dir(&env, &fixture.root), Path::new("/"));
        assert!(!home.exists(), "nothing should be created inside the root");
    }

    #[cfg(unix)]
    #[test]
    fn the_tool_runs_with_its_own_name_as_argv0() {
        let fixture = fixture("true");
        // `/bin/sh` reading a script from stdin reports its own argv[0] as `$0`.
        let call = Call {
            program: Path::new("/bin/sh"),
            name: "shfmt",
            args: &[],
            extra: &[],
            stdin: b"echo \"$0\"\n",
            timeout: Some(Duration::from_secs(5)),
            cwd: Cwd::Neutral,
            cap: CAP,
        };

        let ran = run(&call, &fixture.env, &fixture.root).expect("sh should run");

        assert_eq!(String::from_utf8_lossy(&ran.stdout), "shfmt\n");
    }

    #[cfg(unix)]
    #[test]
    fn output_past_the_cap_is_a_failure() {
        let fixture = fixture(&format!("head -c {} /dev/zero", CAP + 1));

        let result = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)));

        assert!(
            matches!(
                result,
                Err(Failure::OverCap {
                    stream: "stdout",
                    cap: CAP
                })
            ),
            "{result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_at_the_cap_is_kept_whole() {
        let fixture = fixture(&format!("head -c {CAP} /dev/zero"));

        let ran = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)))
            .expect("output at the cap should pass");

        assert_eq!(ran.stdout.len(), CAP);
    }

    /// The small cap the call-cap tests set, well under [`CAP`].
    const SMALL_CAP: usize = 2 * 1024;

    #[cfg(unix)]
    #[test]
    fn output_past_a_calls_own_cap_is_a_failure() {
        let fixture = fixture(&format!("head -c {} /dev/zero >&2", SMALL_CAP + 1));

        let result = run_stub_with(
            &fixture,
            b"",
            &[],
            Some(Duration::from_secs(5)),
            Cwd::Neutral,
            SMALL_CAP,
        );

        assert!(
            matches!(
                result,
                Err(Failure::OverCap {
                    stream: "stderr",
                    cap: SMALL_CAP
                })
            ),
            "{result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_at_a_calls_own_cap_is_kept_whole() {
        let fixture = fixture(&format!("head -c {SMALL_CAP} /dev/zero"));

        let ran = run_stub_with(
            &fixture,
            b"",
            &[],
            Some(Duration::from_secs(5)),
            Cwd::Neutral,
            SMALL_CAP,
        )
        .expect("output at the call's cap should pass");

        assert_eq!(ran.stdout.len(), SMALL_CAP);
    }

    #[cfg(unix)]
    #[test]
    fn output_under_a_cap_above_the_default_is_kept_whole() {
        // Over the default cap, under the call's: the read limit must follow the call's cap.
        let fixture = fixture(&format!("head -c {} /dev/zero", 2 * CAP));

        let ran = run_stub_with(
            &fixture,
            b"",
            &[],
            Some(Duration::from_secs(5)),
            Cwd::Neutral,
            4 * CAP,
        )
        .expect("output under the call's cap should pass");

        assert_eq!(ran.stdout.len(), 2 * CAP);
    }

    /// How many seconds a stub's background sleep holds on, far past any timeout these tests set.
    const HOLD_SECS: u64 = 600;

    /// Whether `/bin/kill -0` still reaches `target`, a pid or a negated process group, after
    /// polling for half of [`HOLD_SECS`]: `false` as soon as it is gone. The poll is that long
    /// because a killed orphan is reaped by init, which a loaded machine can delay for seconds.
    #[cfg(unix)]
    fn still_alive(target: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(HOLD_SECS / 2);
        while Instant::now() < deadline {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", "--", target])
                .stderr(std::process::Stdio::null())
                .status()
                .expect("/bin/kill should run")
                .success();
            if !alive {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// The `(pid, pgid)` a stub wrote to `file` as `<sleep pid> <its own pid>`.
    fn written_ids(file: &Path) -> (String, String) {
        ids_in(file).expect("the stub should write its ids")
    }

    /// The `(pid, pgid)` in `file` as [`written_ids`] reads them, or `None` when the stub didn't
    /// get as far as writing both.
    fn ids_in(file: &Path) -> Option<(String, String)> {
        let ids = fs::read_to_string(file).ok()?;
        let mut ids = ids.split_whitespace().map(str::to_owned);
        let sleep = ids.next()?;
        let group = ids.next()?;
        Some((sleep, format!("-{group}")))
    }

    #[cfg(unix)]
    #[test]
    fn a_child_holding_stdout_dies_with_its_group_at_the_timeout() {
        let ids = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let file = ids.path().join("ids");
        let fixture = fixture(&format!(
            "/bin/sleep {HOLD_SECS} &\necho \"$! $$\" > '{}'\nexit 0",
            file.display()
        ));
        // A loaded machine can reach the timeout before the stub writes its ids, so each attempt
        // that does doubles the timeout until one gets that far, however slow the machine is.
        let mut timeout = Duration::from_millis(200);
        let (sleep, group) = loop {
            let _absent = fs::remove_file(&file);
            let started = Instant::now();

            let result = run_stub(&fixture, b"", &[], Some(timeout));
            let elapsed = started.elapsed();

            assert!(matches!(result, Err(Failure::TimedOut)), "{result:?}");
            assert!(
                elapsed < Duration::from_secs(HOLD_SECS),
                "took {elapsed:?}, so it waited for the sleep holding stdout"
            );
            if let Some(ids) = ids_in(&file) {
                break ids;
            }
            timeout = timeout.saturating_mul(2);
            assert!(
                timeout < Duration::from_secs(HOLD_SECS / 4),
                "the stub never wrote its ids before a timeout of {timeout:?}"
            );
        };

        assert!(
            !still_alive(&sleep),
            "the sleep {sleep} outlived the timeout"
        );
        assert!(
            !still_alive(&group),
            "the group {group} outlived the timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_left_running_after_a_clean_exit_dies_with_its_group() {
        let ids = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let file = ids.path().join("ids");
        let fixture = fixture(&format!(
            "/bin/sleep {HOLD_SECS} >/dev/null 2>&1 &\necho \"$! $$\" > '{}'\nexit 0",
            file.display()
        ));

        let ran = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)))
            .expect("the stub should exit cleanly");
        let (sleep, group) = written_ids(&file);

        assert!(ran.success);
        assert!(!still_alive(&sleep), "the sleep {sleep} outlived the tool");
        assert!(!still_alive(&group), "the group {group} outlived the tool");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_zero_exit_keeps_its_stderr() {
        let fixture = fixture("echo bad >&2\nexit 3");

        let ran = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)))
            .expect("the stub should run");

        assert!(!ran.success);
        assert_eq!(ran.stderr, b"bad\n");
    }

    #[test]
    fn a_cache_directory_swapped_for_a_link_out_gives_no_neutral_directory_and_creates_nothing_there()
     {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let outside = tempfile::TempDir::new().expect("a temp dir should be creatable");
        crate::cli::testing::symlink(outside.path(), cache.path().join("supdoc"))
            .expect("the planted link");
        let path = cache.path().to_path_buf();
        let env =
            ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| path.clone().into()));

        let neutral = neutral_dir(&env, root.path());

        assert_eq!(neutral, PathBuf::from("/"));
        let left: Vec<_> = fs::read_dir(outside.path())
            .expect("the outside dir should be listable")
            .collect();
        assert!(
            left.is_empty(),
            "nothing should be created outside: {left:?}"
        );
    }

    #[test]
    fn a_private_file_planted_as_a_link_out_is_not_written_and_what_it_leads_to_is_untouched() {
        let dir = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let outside = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = exact(dir.path()).expect("a temp dir is a plain absolute path");
        let private = exact(&dir.path().join("private")).expect("a plain child");
        let own = create_private(&Boundary::resolve(&cache), &private)
            .expect("the private directory should be creatable");
        let pointee = outside.path().join("kept");
        fs::write(&pointee, "kept").expect("the outside file");
        let file = private.as_path().join("config.toml");
        crate::cli::testing::symlink(&pointee, &file).expect("the planted link");

        let written = write_private(&own, &exact(&file).expect("a plain child"), "replaced");

        assert_eq!(written, None);
        assert_eq!(fs::read_to_string(&pointee).ok().as_deref(), Some("kept"));
    }

    #[test]
    fn a_cache_base_that_is_a_link_to_another_dir_gives_a_neutral_directory_under_where_it_leads() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let place = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let disk = place.path().join("disk");
        fs::create_dir(&disk).expect("the link's target");
        let base = place.path().join("cache");
        crate::cli::testing::symlink(&disk, &base).expect("the cache base link");
        let path = base.clone();
        let env =
            ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| path.clone().into()));

        let neutral = neutral_dir(&env, root.path());

        assert_eq!(neutral, base.join("supdoc/code-blocks/cwd"));
        assert!(
            disk.join("supdoc/code-blocks/cwd").is_dir(),
            "the neutral directory should be created where the base leads"
        );
    }
}
