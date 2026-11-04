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
use crate::process::LiveGroup;

/// The most bytes supdoc reads from each of a tool's stdout and stderr.
const CAP: usize = 1024 * 1024;

/// How often a tool whose streams have closed is polled for its exit.
const POLL: Duration = Duration::from_millis(5);

/// One tool call: the binary, its arguments and extra variables, and the block it reads.
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
/// Effect: spawns the tool and kills its process group once it exits, passes its timeout or
/// writes past the cap.
///
/// # Errors
///
/// [`Failure`] when the tool can't start, passes its timeout, or writes past the cap.
pub(crate) fn run(call: &Call<'_>, env: &ToolEnv, root: &Path) -> Result<Ran, Failure> {
    let deadline = call.timeout.map(|timeout| Instant::now() + timeout);
    let mut child = Command::new(call.program)
        .arg0(call.name)
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
    // Held until this function returns, by which point every path has reaped the tool.
    let _live = LiveGroup::register(child.id());
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

/// Kills `child` and, best effort, its whole process group, reaps it, and returns `failure`. The
/// stream readers are left to end on their own, never joined.
///
/// Effect: signals the tool's process group and waits for the tool itself to exit.
fn kill(mut child: Child, failure: Failure) -> Failure {
    let _gone = child.kill();
    kill_group(child.id());
    let _reaped = child.wait();
    failure
}

/// Best effort, sends `SIGKILL` to every process in the group `pgid` through `/bin/kill`. A group
/// already empty is not an error.
///
/// Effect: signals the process group.
fn kill_group(pgid: u32) {
    let _group = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
/// outside `root`. Nothing is created unless the deepest existing ancestor of `dir` is outside
/// `root`.
///
/// Effect: reads metadata along `dir`, then creates the directory and reads its entries.
fn usable(dir: &Path, root: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let Ok(real_root) = root.canonicalize() else {
        return false;
    };
    let existing = dir
        .ancestors()
        .find_map(|ancestor| ancestor.canonicalize().ok());
    if existing.is_none_or(|existing| existing.starts_with(&real_root)) {
        return false;
    }
    let _created = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    let Ok(real) = dir.canonicalize() else {
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
    use std::io::ErrorKind;
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

    /// How many times [`run_stub`] spawns a stub that Linux reports busy.
    const BUSY_ATTEMPTS: u32 = 5;

    /// Runs the fixture's stub on `stdin` with `extra` and `timeout`.
    ///
    /// On Linux a stub just written can fail to spawn with `ETXTBSY` while another test's fork
    /// still holds the write descriptor, so a busy spawn is retried a few times, 20 ms apart.
    fn run_stub(
        fixture: &Fixture,
        stdin: &[u8],
        extra: &[(&str, &OsStr)],
        timeout: Option<Duration>,
    ) -> Result<Ran, Failure> {
        let call = Call {
            program: &fixture.stub,
            name: "tool",
            args: &[],
            extra,
            stdin,
            timeout,
        };
        let mut attempt = 1;
        loop {
            match run(&call, &fixture.env, &fixture.root) {
                Err(Failure::Spawn(error))
                    if error.kind() == ErrorKind::ExecutableFileBusy && attempt < BUSY_ATTEMPTS =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                result => return result,
            }
        }
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
    fn a_home_inside_the_root_falls_back_to_slash_and_creates_nothing() {
        let fixture = fixture("true");
        let home = fixture.root.join("home");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.clone().into()));

        assert_eq!(neutral_dir(&env, &fixture.root), Path::new("/"));
        assert!(!home.exists(), "nothing should be created inside the root");
    }

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
        };

        let ran = run(&call, &fixture.env, &fixture.root).expect("sh should run");

        assert_eq!(String::from_utf8_lossy(&ran.stdout), "shfmt\n");
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

    /// Whether `/bin/kill -0` still reaches `target`, a pid or a negated process group, within
    /// two seconds of polling: `false` once it is gone.
    fn still_alive(target: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
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
        let ids = fs::read_to_string(file).expect("the stub should write its ids");
        let mut ids = ids.split_whitespace().map(str::to_owned);
        let sleep = ids.next().expect("the stub should write the sleep's pid");
        let group = ids.next().expect("the stub should write its own pid");
        (sleep, format!("-{group}"))
    }

    #[test]
    fn a_child_holding_stdout_dies_with_its_group_at_the_timeout() {
        let ids = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let file = ids.path().join("ids");
        let fixture = fixture(&format!(
            "/bin/sleep 30 &\necho \"$! $$\" > '{}'\nexit 0",
            file.display()
        ));
        let started = Instant::now();

        let result = run_stub(&fixture, b"", &[], Some(Duration::from_millis(200)));
        let elapsed = started.elapsed();
        let (sleep, group) = written_ids(&file);

        assert!(matches!(result, Err(Failure::TimedOut)), "{result:?}");
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
        assert!(
            !still_alive(&sleep),
            "the sleep {sleep} outlived the timeout"
        );
        assert!(
            !still_alive(&group),
            "the group {group} outlived the timeout"
        );
    }

    #[test]
    fn a_child_left_running_after_a_clean_exit_dies_with_its_group() {
        let ids = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let file = ids.path().join("ids");
        let fixture = fixture(&format!(
            "/bin/sleep 30 >/dev/null 2>&1 &\necho \"$! $$\" > '{}'\nexit 0",
            file.display()
        ));

        let ran = run_stub(&fixture, b"", &[], Some(Duration::from_secs(5)))
            .expect("the stub should exit cleanly");
        let (sleep, group) = written_ids(&file);

        assert!(ran.success);
        assert!(!still_alive(&sleep), "the sleep {sleep} outlived the tool");
        assert!(!still_alive(&group), "the group {group} outlived the tool");
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
