//! The process boundary: the code that reads the process environment and sets its exit code.
//!
//! `exit_code` reads the environment variables the commands need and hands them down. Clap reads
//! the `SUPDOC_*` flags from the same lookup.

use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::path::{MAIN_SEPARATOR_STR, Path};

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

/// Parses, runs and reports, and returns the process exit code.
///
/// Builds the one [`Output`] of the run, and every command prints through it.
pub(crate) fn exit_code() -> u8 {
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
