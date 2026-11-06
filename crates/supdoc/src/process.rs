//! The process boundary: the code that reads the process environment and sets its exit code.
//!
//! `exit_code` reads the environment variables the commands need and hands them down. Clap reads
//! the `SUPDOC_*` flags from the same lookup.
//!
//! It also installs the one termination handler: on SIGINT, SIGTERM or SIGHUP every tool process
//! group still registered as live is killed, and supdoc then ends as that signal's default action
//! would have ended it. Once a signal has arrived, supdoc ends by that signal and no other way:
//! the main thread waits for the re-raise instead of printing anything or returning a code.
//!
//! On Windows only SIGINT and SIGTERM are handled, through signal-hook's flag registration and a
//! thread that polls the flags every [`TERMINATION_POLL`]. SIGHUP does not exist there. Ctrl+Break,
//! console close, logoff and shutdown are not caught, so Windows ends supdoc by its default action;
//! tools in their own process group still receive the console close event, so they end too. The
//! C runtime's default SIGINT action exits with code 3, not 130.

use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::path::{MAIN_SEPARATOR_STR, Path};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, PoisonError, RwLock, RwLockReadGuard};
use std::time::Duration;

use crate::cli::args::{Cli, GlobalArgs};
use crate::cli::parse::{Parsed, Printout, Stream, parse_from};
use crate::cli::skill::SkillDirs;
use crate::cli::{CliError, Context, run};
use crate::exit::ExitStatus;
use crate::finding::{Finding, Severity};
use crate::output::format::supports_unicode;
use crate::output::{Output, OutputSettings, PathStyle, Verbosity};
use crate::path::AbsolutePath;
use crate::scan::walk::GlobalExcludes;
use clap::ColorChoice;

/// Terminal width when `COLUMNS` says nothing usable.
const DEFAULT_WIDTH: usize = 80;

/// The process group ids of the tools running now, each registered by a [`LiveGroup`].
static LIVE_GROUPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Held for reading by each tool spawn from its check of [`TERMINATING`] until its group is
/// registered, and for writing by the signal thread while it copies [`LIVE_GROUPS`]. So the copy
/// waits out every spawn in progress, and no tool can exist yet be missing from it.
static SPAWNS: RwLock<()> = RwLock::new(());

/// The termination signal that arrived, or 0 while none has. Set before any tool group is killed.
pub(crate) static TERMINATING: AtomicI32 = AtomicI32::new(0);

/// How long the main thread sleeps between checks while it waits for the signal thread's re-raise.
const TERMINATION_POLL: Duration = Duration::from_millis(50);

/// A tool's process group, registered as live while this value exists, so a termination signal
/// kills it. Dropped once the tool is reaped.
#[derive(Debug)]
pub(crate) struct LiveGroup(u32);

impl LiveGroup {
    /// Registers `pgid` as live until the returned value is dropped.
    #[must_use]
    pub(crate) fn register(pgid: u32) -> Self {
        LIVE_GROUPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pgid);
        Self(pgid)
    }
}

impl Drop for LiveGroup {
    fn drop(&mut self) {
        let mut live = LIVE_GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = live.iter().position(|pgid| *pgid == self.0) {
            let _removed = live.swap_remove(at);
        }
    }
}

/// One tool spawn in progress, from the check of [`TERMINATING`] until its group is registered.
///
/// While it exists the signal thread can't copy the live groups. It is never held while waiting
/// on a tool, writing output or sleeping, so the signal thread waits only for a spawn and a push.
#[derive(Debug)]
pub(crate) struct Spawning {
    /// The read side of [`SPAWNS`], held only to keep the signal thread's copy waiting.
    _guard: RwLockReadGuard<'static, ()>,
}

impl Spawning {
    /// Starts a spawn, or blocks for good, holding nothing, if a termination signal has arrived.
    ///
    /// The signal thread sets [`TERMINATING`] before it takes [`SPAWNS`] to copy the live groups.
    /// So a spawn that starts after the copy sees the flag here, and one that started before it
    /// is registered before the copy is taken.
    ///
    /// Effect: sleeps the calling thread for good once a signal has arrived.
    #[must_use]
    pub(crate) fn begin() -> Self {
        let guard = SPAWNS.read().unwrap_or_else(PoisonError::into_inner);
        if ends_by_signal(&TERMINATING) {
            // Released first: the signal thread takes `SPAWNS` before it can end the process.
            drop(guard);
            wait_to_be_ended();
        }
        Self { _guard: guard }
    }
}

/// The live groups, copied once every spawn in progress has registered its group.
///
/// Effect: waits for each [`Spawning`] that exists to be dropped.
fn live_groups_once_spawned() -> Vec<u32> {
    let _spawns = SPAWNS.write().unwrap_or_else(PoisonError::into_inner);
    LIVE_GROUPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Whether `pgid` is registered as live.
#[cfg(test)]
fn is_live(pgid: u32) -> bool {
    LIVE_GROUPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&pgid)
}

/// Best effort, kills every live tool process tree, once every spawn in progress has registered
/// its group.
///
/// Effect: signals each registered process group.
fn kill_live_groups() {
    for pgid in live_groups_once_spawned() {
        kill_tree(pgid);
    }
}

/// Best effort, kills the tool `pid` leads and its descendants. A tree already gone is not an
/// error.
///
/// On Unix `pid` is a process group id and this sends `SIGKILL` to the group through `/bin/kill`,
/// which reaches the group's members even after the leader exited.
/// On Windows it runs `taskkill.exe /T /F /PID`, which follows parent process ids at kill time.
/// So the tool must still be alive when this runs: once it has exited, its orphaned descendants
/// are unreachable and this kills nothing. A descendant whose parent already exited is missed
/// too, and a pid can be reused between a tool's exit and the kill, as a pgid can on Unix.
///
/// Effect: runs the kill program and signals the tree.
pub(crate) fn kill_tree(pid: u32) {
    #[cfg(unix)]
    let mut kill = {
        let mut kill = Command::new("/bin/kill");
        let _same = kill.args(["-KILL", "--", &format!("-{pid}")]);
        kill
    };
    #[cfg(windows)]
    let mut kill = {
        let root = SYSTEM_ROOT.get_or_init(|| system_root(None));
        let mut kill = Command::new(root.join("System32").join("taskkill.exe"));
        let _same = kill
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .env("SystemRoot", root);
        kill
    };
    let _tree = kill
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The Windows directory `taskkill.exe` lives under, set once at the process boundary by
/// [`record_system_root`].
#[cfg(windows)]
static SYSTEM_ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// `system_root` when it is absolute, else `C:\Windows`. Never a `PATH` lookup.
#[cfg(windows)]
fn system_root(system_root: Option<&OsStr>) -> std::path::PathBuf {
    system_root
        .map(Path::new)
        .filter(|root| root.is_absolute())
        .unwrap_or(Path::new("C:\\Windows"))
        .to_path_buf()
}

/// Records the Windows directory from the `SystemRoot` variable. Read here, at the process
/// boundary, and nowhere else.
///
/// Effect: sets [`SYSTEM_ROOT`] once.
#[cfg(windows)]
fn record_system_root() {
    let _first = SYSTEM_ROOT.set(system_root(std::env::var_os("SystemRoot").as_deref()));
}

/// Installs the handler for SIGINT, SIGTERM and SIGHUP: on one thread, kill every live tool group,
/// then end the process as the signal's default action would. If it can't be installed the run
/// goes on with the default signal behavior.
///
/// Effect: registers the signals and starts the signal-handling thread.
#[cfg(unix)]
fn handle_termination() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) else {
        return;
    };
    drop(std::thread::spawn(move || {
        // The first signal ends the process, so no later one is read.
        let Some(signal) = signals.forever().next() else {
            return;
        };
        end_by_termination(signal);
    }));
}

/// Installs the handler for SIGINT and SIGTERM: one thread polls the two flags the signals set,
/// then kills every live tool group and ends the process as the signal's default action would. If
/// either signal can't be registered the run goes on with the default signal behavior.
///
/// Effect: registers the signals and starts the signal-handling thread.
#[cfg(windows)]
fn handle_termination() {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let interrupt = Arc::new(AtomicBool::new(false));
    let terminate = Arc::new(AtomicBool::new(false));
    if signal_hook::flag::register(SIGINT, Arc::clone(&interrupt)).is_err()
        || signal_hook::flag::register(SIGTERM, Arc::clone(&terminate)).is_err()
    {
        return;
    }
    drop(std::thread::spawn(move || {
        let signal = loop {
            if interrupt.load(Ordering::SeqCst) {
                break SIGINT;
            }
            if terminate.load(Ordering::SeqCst) {
                break SIGTERM;
            }
            std::thread::sleep(TERMINATION_POLL);
        };
        end_by_termination(signal);
    }));
}

/// Records `signal`, kills every live tool group, then ends the process as the signal's default
/// action would.
///
/// Effect: signals the live tool groups and ends the process.
fn end_by_termination(signal: i32) {
    // Set first, so the main thread can't see a killed tool and end before the re-raise.
    TERMINATING.store(signal, Ordering::SeqCst);
    kill_live_groups();
    // Resets the signal to its default action and raises it, which ends the process. No
    // fallback is needed: for the signals registered here, signal-hook 0.4 raises the signal
    // and aborts if raising fails, so this call never returns.
    let _ended = signal_hook::low_level::emulate_default_handler(signal);
}

/// Whether `flag` records a termination signal, so the process must wait to be ended by it.
pub(crate) fn ends_by_signal(flag: &AtomicI32) -> bool {
    flag.load(Ordering::SeqCst) != 0
}

/// Blocks for good once `flag` records a termination signal, so the signal thread's re-raise ends
/// the process; returns at once otherwise.
///
/// Effect: sleeps the calling thread while a signal is pending.
pub(crate) fn await_termination(flag: &AtomicI32) {
    if ends_by_signal(flag) {
        wait_to_be_ended();
    }
}

/// Blocks for good: a termination signal has arrived, and its re-raise on the signal thread ends
/// the process.
///
/// Effect: sleeps the calling thread until the process ends.
pub(crate) fn wait_to_be_ended() -> ! {
    loop {
        std::thread::sleep(TERMINATION_POLL);
    }
}

/// Parses, runs and reports, and returns the process exit code.
///
/// After a termination signal it never returns: the signal ends the process.
pub(crate) fn exit_code() -> u8 {
    #[cfg(windows)]
    record_system_root();
    handle_termination();
    let code = parse_run_and_report();
    await_termination(&TERMINATING);
    code
}

/// Parses, runs and reports, and returns the exit code of the run.
///
/// Builds the one [`Output`] of the run, and every command prints through it.
fn parse_run_and_report() -> u8 {
    let var = |name: &str| std::env::var_os(name);
    let cli = match parse_from(std::env::args_os(), |name| {
        var(name).and_then(|value| value.into_string().ok())
    }) {
        Parsed::Run(cli) => *cli,
        Parsed::Print(printout) => return print(&printout),
    };
    let Cli { global, command } = cli;

    let (cwd, failure) = match current_dir() {
        Ok(cwd) => (cwd, None),
        Err(error) => match AbsolutePath::new(Path::new(MAIN_SEPARATOR_STR)) {
            Some(root) => (root, Some(error)),
            None => return ExitStatus::Io.code(),
        },
    };
    let mut output = Output::stdio(settings(&global, cwd.clone()), choice(global.color));
    let result = if let Some(error) = failure {
        Err(error)
    } else {
        let global_excludes = GlobalExcludes::find(&cwd);
        let context = Context {
            global,
            cwd,
            global_excludes,
            trust: crate::trust::TrustEnv::from_vars(var),
        };
        let tools = crate::code_blocks::tools::ToolEnv::from_vars(var);
        run(
            &command,
            &SkillDirs::from_vars(var),
            &tools,
            &context,
            &mut output,
        )
    };

    match result {
        Ok(outcome) => outcome.exit_code(),
        Err(error) => report(&mut output, &error),
    }
}

/// The current directory, read once for the whole run.
///
/// # Errors
///
/// Returns `cli-no-cwd` when the directory is unreadable or not absolute.
fn current_dir() -> Result<AbsolutePath, CliError> {
    std::env::current_dir()
        .ok()
        .and_then(|dir| AbsolutePath::new(&dir))
        .ok_or_else(|| {
            CliError::new(
                "cli-no-cwd",
                "could not read the current directory",
                "`cd` to a directory that exists and that you can read, then run supdoc again",
                ExitStatus::Io,
            )
        })
}

/// Writes help, a version or a usage error to its stream and returns its exit code.
///
/// A closed pipe does not change the exit code: the run's own code stands, so `--help` exits 0
/// and a usage error still exits 2.
fn print(printout: &Printout) -> u8 {
    // Held like every `Output` write: nothing prints once a termination signal has arrived.
    await_termination(&TERMINATING);
    let written = match printout.stream {
        Stream::Stdout => write_all(io::stdout(), &printout.text),
        Stream::Stderr => write_all(io::stderr(), &printout.text),
    };
    exit_after_print(&written, printout.exit_code)
}

/// The exit code of a run whose help or usage text was written with result `written`.
fn exit_after_print(written: &io::Result<()>, exit_code: u8) -> u8 {
    match written {
        Ok(()) => exit_code,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => exit_code,
        Err(_) => ExitStatus::Io.code(),
    }
}

/// Writes `text` to `sink` and flushes it.
///
/// # Errors
///
/// Returns the write or flush failure.
fn write_all(mut sink: impl Write, text: &str) -> io::Result<()> {
    sink.write_all(text.as_bytes())?;
    sink.flush()
}

/// Prints `error` as a fatal error in the requested format and returns its exit code.
///
/// A closed pipe does not change the exit code: the error's own code stands.
fn report(output: &mut Output, error: &CliError) -> u8 {
    let written = output.fatal(&fatal_finding(error), error.exit_code());
    if written.is_ok() {
        error.exit_code()
    } else {
        ExitStatus::Io.code()
    }
}

/// The finding a fatal error prints, pointing at its file and line when it has them.
fn fatal_finding(error: &CliError) -> Finding {
    let finding =
        Finding::new(error.code(), Severity::Error, error.message()).with_help(error.help());
    match error.site() {
        Some(site) => finding
            .with_location(site.location.clone())
            .with_snippet(site.snippet.clone()),
        None => finding,
    }
}

/// What `anstream` should do with ANSI codes, as `--color` says.
fn choice(color: ColorChoice) -> anstream::ColorChoice {
    match color {
        ColorChoice::Always => anstream::ColorChoice::Always,
        ColorChoice::Never => anstream::ColorChoice::Never,
        ColorChoice::Auto => anstream::ColorChoice::Auto,
    }
}

/// Whether text output carries color.
///
/// `--color always` and `never` decide alone. `auto` colors only when stdout, the stream the data
/// goes to, is a terminal, `NO_COLOR` is unset or empty, and `TERM` isn't `dumb`.
fn color_on(
    choice: ColorChoice,
    stdout_is_terminal: bool,
    no_color: Option<&OsStr>,
    term: Option<&str>,
) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            stdout_is_terminal && no_color.is_none_or(OsStr::is_empty) && term != Some("dumb")
        }
    }
}

/// What `Output` needs, decided once from the flags and the terminal.
fn settings(global: &GlobalArgs, cwd: AbsolutePath) -> OutputSettings {
    let term = std::env::var("TERM").ok();
    let color = color_on(
        global.color,
        io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
        term.as_deref(),
    );
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.parse().ok())
        .unwrap_or(DEFAULT_WIDTH);
    let github_workspace =
        std::env::var_os("GITHUB_WORKSPACE").and_then(|dir| AbsolutePath::new(Path::new(&dir)));
    OutputSettings {
        format: global.format(),
        verbosity: Verbosity::from_flags(global.quiet, global.verbose),
        color,
        unicode: supports_unicode(color, term.as_deref()),
        width,
        paths: PathStyle::new(cwd.clone(), cwd, github_workspace),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex};

    use crate::cli::ErrorSite;
    use crate::exit::ExitStatus;
    use crate::finding::{Location, Snippet};
    use crate::link::SourcePath;
    use crate::output::OutputFormat;

    use super::*;

    #[test]
    fn a_live_group_is_registered_until_it_is_dropped() {
        // Far above any `pid_max`, so no running tool's group shares these ids.
        let first = LiveGroup::register(4_000_000_001);
        let second = LiveGroup::register(4_000_000_002);

        assert!(is_live(4_000_000_001) && is_live(4_000_000_002));
        drop(first);
        assert!(!is_live(4_000_000_001), "a dropped group should be removed");
        assert!(
            is_live(4_000_000_002),
            "only the dropped group should be removed"
        );
        drop(second);
        assert!(!is_live(4_000_000_002));
    }

    #[test]
    fn the_signal_side_copies_the_live_groups_only_after_a_spawn_in_progress_registers() {
        // A fake spawn: no process, just a group id far above any `pid_max`.
        let spawning = Spawning::begin();
        let (sender, copies) = std::sync::mpsc::channel();
        let signal_side = std::thread::spawn(move || {
            let _sent = sender.send(live_groups_once_spawned());
        });

        assert!(
            copies.recv_timeout(Duration::from_millis(200)).is_err(),
            "the live groups should not be copied while a spawn is in progress"
        );
        let live = LiveGroup::register(4_000_000_003);
        drop(spawning);

        let copy = copies
            .recv_timeout(Duration::from_secs(10))
            .expect("the copy should run once the spawn ends");
        assert!(
            copy.contains(&4_000_000_003),
            "the copy should hold the group the spawn registered, got {copy:?}"
        );
        signal_side
            .join()
            .expect("the copying thread should not panic");
        drop(live);
    }

    #[test]
    fn a_pending_termination_signal_makes_supdoc_wait_to_be_ended_by_it() {
        let flag = AtomicI32::new(0);
        assert!(
            !ends_by_signal(&flag),
            "with no signal the run should end with its own code"
        );

        flag.store(signal_hook::consts::SIGTERM, Ordering::SeqCst);

        assert!(
            ends_by_signal(&flag),
            "after SIGTERM the run should wait for the signal to end it"
        );
    }

    #[test]
    fn closed_stdout_keeps_the_exit_code_of_help_and_of_a_usage_error() {
        let closed = Err(io::Error::from(io::ErrorKind::BrokenPipe));

        assert_eq!(
            (
                exit_after_print(&closed, ExitStatus::Clean.code()),
                exit_after_print(&closed, ExitStatus::Usage.code()),
            ),
            (0, 2),
            "a closed pipe keeps help at exit 0 and a usage error at exit 2"
        );
    }

    /// A writer whose bytes the test can read back after `Output` takes ownership of it.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            let bytes = self
                .0
                .lock()
                .expect("the capture lock should not be poisoned");
            String::from_utf8(bytes.clone()).expect("output should be UTF-8")
        }
    }

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("the capture lock should not be poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A TOML syntax error on line 2 of `supdoc.config.toml`, as the config loader reports it.
    fn syntax_error() -> CliError {
        let text = "include = [\"docs\"]\nexclude = oops\n";
        let start = text.find("oops").expect("the fixture should hold `oops`");
        let range = start..start + "oops".len();
        let file = SourcePath::from_relative(Path::new("supdoc.config.toml"))
            .expect("the fixture path should be a source path");
        let location =
            Location::in_text(file, text, range.clone()).expect("the range should be locatable");
        let snippet = Snippet::capture(text, range).expect("the range should be capturable");
        CliError::new(
            "config-parse",
            "supdoc.config.toml: invalid value",
            "Correct the config as the message says",
            ExitStatus::Config,
        )
        .at(ErrorSite { location, snippet })
    }

    /// Reports `error` in `format` and returns what reached stderr.
    fn stderr_of(format: OutputFormat, error: &CliError) -> String {
        let cwd = AbsolutePath::new(Path::new(MAIN_SEPARATOR_STR))
            .expect("the filesystem root should be absolute");
        let settings = OutputSettings {
            format,
            verbosity: Verbosity::Normal,
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd, None),
        };
        let stderr = Capture::default();
        let mut output = Output::new(
            settings,
            Box::new(Capture::default()),
            Box::new(stderr.clone()),
        );

        let _exit_code = report(&mut output, error);

        stderr.text()
    }

    #[test]
    fn a_fatal_error_with_a_site_shows_its_file_and_line_in_text() {
        let text = stderr_of(OutputFormat::Text, &syntax_error());

        assert!(
            text.contains("--> supdoc.config.toml:2:11"),
            "text should point at file:line:column: {text}"
        );
        assert!(
            text.contains(
                " --> supdoc.config.toml:2:11\n  |\n2 | exclude = oops\n  |           ^^^^\n  |\n  = help"
            ),
            "text should show the line once, with no stray ` |` line: {text}"
        );
    }

    #[test]
    fn a_fatal_error_with_a_site_carries_its_location_in_json() {
        let text = stderr_of(OutputFormat::Json, &syntax_error());

        let error: serde_json::Value =
            serde_json::from_str(&text).expect("stderr should hold one JSON document");
        assert_eq!(
            error.get("location"),
            Some(&serde_json::json!({"file": "supdoc.config.toml", "line": 2, "column": 11})),
            "the JSON error should carry its location: {text}"
        );
    }

    #[test]
    fn a_fatal_error_without_a_site_has_no_location() {
        let error = CliError::new(
            "cli-no-cwd",
            "no directory",
            "run it elsewhere",
            ExitStatus::Io,
        );

        let text = stderr_of(OutputFormat::Text, &error);
        let json = stderr_of(OutputFormat::Json, &error);

        assert!(
            !text.contains("-->"),
            "text should have no location: {text}"
        );
        assert!(
            !json.contains("location"),
            "JSON should have no location: {json}"
        );
    }

    #[test]
    fn auto_color_follows_the_stdout_terminal() {
        let auto = |terminal| color_on(ColorChoice::Auto, terminal, None, Some("xterm"));

        assert!(auto(true), "a terminal stdout should be colored");
        assert!(!auto(false), "a piped or redirected stdout should be plain");
    }

    #[test]
    fn no_color_and_dumb_term_turn_auto_color_off() {
        let no_color = OsString::from("1");
        let empty = OsString::new();

        assert!(!color_on(
            ColorChoice::Auto,
            true,
            Some(&no_color),
            Some("xterm")
        ));
        assert!(
            color_on(ColorChoice::Auto, true, Some(&empty), Some("xterm")),
            "an empty NO_COLOR should not turn color off"
        );
        assert!(!color_on(ColorChoice::Auto, true, None, Some("dumb")));
    }

    #[test]
    fn color_always_and_never_ignore_the_terminal_and_environment() {
        let no_color = OsString::from("1");

        assert!(color_on(
            ColorChoice::Always,
            false,
            Some(&no_color),
            Some("dumb")
        ));
        assert!(!color_on(ColorChoice::Never, true, None, Some("xterm")));
    }
}
