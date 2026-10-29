//! The `supdoc` command: the only code that reads the process environment and sets its exit code.

#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]

use std::io::{self, IsTerminal, Write};
use std::path::{MAIN_SEPARATOR_STR, Path};
use std::process::ExitCode;

use clap::ColorChoice;
use supdoc::cli::args::Cli;
use supdoc::cli::args::GlobalArgs;
use supdoc::cli::parse::{Parsed, Printout, Stream, parse_process};
use supdoc::cli::{CliError, Context, run};
use supdoc::finding::{Finding, Severity};
use supdoc::output::format::supports_unicode;
use supdoc::output::{Output, OutputSettings, PathStyle, Verbosity};
use supdoc::path::AbsolutePath;

/// The exit code for an I/O error (06 X7).
const IO_EXIT_CODE: u8 = 74;

/// Terminal width when `COLUMNS` says nothing usable.
const DEFAULT_WIDTH: usize = 80;

fn main() -> ExitCode {
    ExitCode::from(exit_code())
}

/// Parses, runs and reports, and returns the process exit code.
///
/// Builds the one [`Output`] of the run, and every command prints through it.
fn exit_code() -> u8 {
    let cli = match parse_process() {
        Parsed::Run(cli) => *cli,
        Parsed::Print(printout) => return print(&printout),
    };
    let Cli { global, command } = cli;

    let (cwd, failure) = match current_dir() {
        Ok(cwd) => (cwd, None),
        Err(error) => match AbsolutePath::new(Path::new(MAIN_SEPARATOR_STR)) {
            Some(root) => (root, Some(error)),
            None => return IO_EXIT_CODE,
        },
    };
    let mut output = Output::stdio(settings(&global, cwd.clone()), choice(global.color));
    let result = match failure {
        Some(error) => Err(error),
        None => run(&command, &Context { global, cwd }, &mut output),
    };

    match result {
        Ok(_) if output.is_closed() => 0,
        Ok(outcome) => outcome.exit_code(),
        Err(error) => report(&mut output, &error),
    }
}

/// The current directory, read once for the whole run (01).
///
/// # Errors
///
/// Returns `cli/no-cwd` when the directory is unreadable or not absolute.
fn current_dir() -> Result<AbsolutePath, CliError> {
    std::env::current_dir()
        .ok()
        .and_then(|dir| AbsolutePath::new(&dir))
        .ok_or_else(|| {
            CliError::new(
                "cli/no-cwd",
                "could not read the current directory",
                "run supdoc from a directory that exists and that you can read",
                IO_EXIT_CODE,
            )
        })
}

/// Writes help, a version or a usage error to its stream and returns its exit code.
///
/// A closed pipe is not a failure of supdoc, so it exits 0 (06 X13).
fn print(printout: &Printout) -> u8 {
    let written = match printout.stream {
        Stream::Stdout => write_all(io::stdout(), &printout.text),
        Stream::Stderr => write_all(io::stderr(), &printout.text),
    };
    match written {
        Ok(()) => printout.exit_code,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(_) => IO_EXIT_CODE,
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
/// A closed pipe is not a failure of supdoc, so it exits 0 (06 X13).
fn report(output: &mut Output, error: &CliError) -> u8 {
    let written = output.fatal(&fatal_finding(error), error.exit_code());
    if output.is_closed() {
        0
    } else if written.is_ok() {
        error.exit_code()
    } else {
        IO_EXIT_CODE
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

/// What `Output` needs, decided once from the flags and the terminal.
fn settings(global: &GlobalArgs, cwd: AbsolutePath) -> OutputSettings {
    let color = match global.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            io::stderr().is_terminal()
                && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        }
    };
    let term = std::env::var("TERM").ok();
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
    use std::sync::{Arc, Mutex};

    use supdoc::cli::ErrorSite;
    use supdoc::finding::{Location, Snippet};
    use supdoc::link::SourcePath;
    use supdoc::output::OutputFormat;

    use super::*;

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
            "config/parse",
            "supdoc.config.toml: invalid value",
            "Correct the config as the message says",
            CliError::CONFIG_EXIT_CODE,
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
        let error = CliError::new("cli/no-cwd", "no directory", "run it elsewhere", 74);

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
}
