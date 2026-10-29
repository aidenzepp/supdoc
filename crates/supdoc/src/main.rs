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
    let finding =
        Finding::new(error.code(), Severity::Error, error.message()).with_help(error.help());
    let written = output.fatal(&finding, error.exit_code());
    if output.is_closed() {
        0
    } else if written.is_ok() {
        error.exit_code()
    } else {
        IO_EXIT_CODE
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
