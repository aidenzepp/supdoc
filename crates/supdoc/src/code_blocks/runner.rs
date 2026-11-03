//! The hardened spawn of a tool on one block: stdin, a neutral working directory, a timeout and
//! an output cap.
//!
//! A tool is spawned by the absolute path its lookup returned, never through a shell, `cargo`,
//! `python -m` or `npx`. Its environment holds only [`ToolEnv`]'s variables and the call's own
//! extras. It runs in its own process group, in an empty supdoc-owned directory, and the block
//! reaches it on stdin only.

use std::ffi::OsStr;
use std::io::{Read, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::tools::ToolEnv;

/// The most bytes supdoc reads from each of a tool's stdout and stderr.
const CAP: usize = 1024 * 1024;

/// How often a tool whose streams have closed is polled for its exit.
const POLL: Duration = Duration::from_millis(5);

/// One tool call: the binary, its arguments and extra variables, and the block it reads.
#[derive(Debug)]
pub(crate) struct Call<'call> {
    /// The binary's absolute real path, as the lookup returned it.
    pub(crate) program: &'call Path,
    /// The arguments, after the program.
    pub(crate) args: &'call [&'call OsStr],
    /// Variables the call needs beyond [`ToolEnv`]'s.
    pub(crate) extra: &'call [(&'call str, &'call OsStr)],
    /// The bytes written to the tool's stdin, which is then closed.
    pub(crate) stdin: &'call [u8],
    /// How long the tool may run. `None` is no limit.
    pub(crate) timeout: Option<Duration>,
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
    /// It wrote more than 1 MiB to the named stream and was killed.
    OverCap(&'static str),
}

/// Runs `call` with `env`'s variables, from [`neutral_dir`] judged against `root`.
///
/// Effect: spawns the tool and, on a timeout or the cap, kills its process group.
///
/// # Errors
///
/// [`Failure`] when the tool can't start, passes its timeout, or writes past the cap.
pub(crate) fn run(call: &Call<'_>, env: &ToolEnv, root: &Path) -> Result<Ran, Failure> {
    let deadline = call.timeout.map(|timeout| Instant::now() + timeout);
    let mut child = Command::new(call.program)
        .args(call.args)
        .env_clear()
        .envs(env.vars())
        .envs(call.extra.iter().copied())
        .current_dir(neutral_dir(env, root))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(Failure::Spawn)?;
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = call.stdin.to_vec();
        drop(thread::spawn(move || {
            // A tool that exits without reading its stdin closes the pipe: nothing to report.
            let _closed = stdin.write_all(&bytes);
        }));
    }
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
                let limit = u64::try_from(CAP).unwrap_or(u64::MAX).saturating_add(1);
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
        if bytes.len() > CAP {
            return Err(kill(child, Failure::OverCap(name)));
        }
        if name == "stdout" {
            stdout = Some(bytes);
        } else {
            stderr = Some(bytes);
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
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

/// Kills `child` and, best effort, its whole process group, reaps it, and returns `failure`. The
/// stream readers are left to end on their own, never joined.
///
/// Effect: signals the tool's process group and waits for the tool itself to exit.
fn kill(mut child: Child, failure: Failure) -> Failure {
    let _gone = child.kill();
    let _group = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _reaped = child.wait();
    failure
}

/// The working directory a tool runs in: `cwd` under `$XDG_CACHE_HOME/supdoc/code-blocks`, or
/// under `$HOME/.cache/supdoc/code-blocks`, created mode 0700. `/` when neither is usable: not
/// absolute, inside `root`, not a directory only its owner reaches, or not empty.
///
/// Effect: creates the directory and reads its metadata and entries.
#[must_use]
pub(crate) fn neutral_dir(env: &ToolEnv, root: &Path) -> PathBuf {
    let var = |name: &str| {
        env.vars()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| PathBuf::from(value))
            .filter(|path| path.is_absolute())
    };
    let candidates = [
        var("XDG_CACHE_HOME"),
        var("HOME").map(|home| home.join(".cache")),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(|cache| cache.join("supdoc/code-blocks/cwd"))
        .find(|dir| usable(dir, root))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Whether `dir`, created mode 0700 when missing, is an empty directory only its owner reaches,
/// outside `root`.
///
/// Effect: creates the directory and reads its metadata and entries.
fn usable(dir: &Path, root: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let _created = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    let (Ok(real), Ok(real_root)) = (dir.canonicalize(), root.canonicalize()) else {
        return false;
    };
    meta.is_dir()
        && meta.permissions().mode().trailing_zeros() >= 6
        && !real.starts_with(&real_root)
        && std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{CAP, Call, Failure, Ran, neutral_dir, run};
    use crate::code_blocks::tools::ToolEnv;

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

    /// Runs the fixture's stub on `stdin` with `extra` and `timeout`.
    fn run_stub(
        fixture: &Fixture,
        stdin: &[u8],
        extra: &[(&str, &OsStr)],
        timeout: Option<Duration>,
    ) -> Result<Ran, Failure> {
        let call = Call {
            program: &fixture.stub,
            args: &[],
            extra,
            stdin,
            timeout,
        };
        run(&call, &fixture.env, &fixture.root)
    }

    #[test]
    fn stdin_arrives_byte_exact() {
        let fixture = fixture("cat");
        let block = b"echo  'hi'\r\n\tx\0y\n\xff";

        let ran =
            run_stub(&fixture, block, &[], Some(Duration::from_secs(5))).expect("cat should run");

        assert!(ran.success);
        assert_eq!(ran.stdout, block);
    }

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

    #[test]
    fn a_home_inside_the_root_falls_back_to_slash() {
        let fixture = fixture("true");
        let home = fixture.root.join("home");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.clone().into()));

        assert_eq!(neutral_dir(&env, &fixture.root), Path::new("/"));
    }

    #[test]
    fn output_past_the_cap_is_a_failure() {
        let fixture = fixture(&format!("head -c {} /dev/zero", CAP + 1));

        let result = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)));

        assert!(
            matches!(result, Err(Failure::OverCap("stdout"))),
            "{result:?}"
        );
    }

    #[test]
    fn output_at_the_cap_is_kept_whole() {
        let fixture = fixture(&format!("head -c {CAP} /dev/zero"));

        let ran = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)))
            .expect("output at the cap should pass");

        assert_eq!(ran.stdout.len(), CAP);
    }

    #[test]
    fn a_child_holding_stdout_dies_at_the_timeout() {
        let fixture = fixture("sleep 30 &\nexit 0");
        let started = Instant::now();

        let result = run_stub(&fixture, b"", &[], Some(Duration::from_millis(200)));

        assert!(matches!(result, Err(Failure::TimedOut)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_non_zero_exit_keeps_its_stderr() {
        let fixture = fixture("echo bad >&2\nexit 3");

        let ran = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)))
            .expect("the stub should run");

        assert!(!ran.success);
        assert_eq!(ran.stderr, b"bad\n");
    }
}
