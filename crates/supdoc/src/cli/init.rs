//! `supdoc init`: writes a starter `supdoc.config.toml` and nothing else (05 C10).
//!
//! The lockfile is created by the first `update --yes`. An existing config is never touched.

use std::fs::OpenOptions;
use std::io::{self, Write as _};

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::CONFIG_FILE_NAME;
use crate::output::{Output, OutputError, Report, TextContext};
use crate::path::{AbsolutePath, RelativePath};

use super::args::InitArgs;
use super::{CliError, Context, Outcome};

/// The file `supdoc init` writes. It loads as a valid config (see the tests), and its live keys
/// spell out the defaults so that editing them is the obvious next step.
const STARTER: &str = "\
# supdoc.config.toml. Every key is optional.

# Files to scan for links. Globs, relative to this file's directory.
# Each pattern also matches everything beneath it.
include = [\"**\"]

# Removed from include. Exclude always wins.
exclude = []

# Another config to inherit from, relative to this file.
# extends = \"../../supdoc.config.toml\"

# Short names for directories, relative to this file.
# [namespaces]
# auth = \"services/auth\"
";

/// The starter config as output data, shown by `--dry-run`.
#[derive(Debug, Serialize, JsonSchema)]
struct StarterReport {
    /// The file contents, written as is in text output.
    contents: &'static str,
}

impl Report for StarterReport {
    fn command(&self) -> &'static str {
        "init"
    }

    fn render_text(&self, _context: &TextContext, out: &mut dyn io::Write) -> io::Result<()> {
        out.write_all(self.contents.as_bytes())
    }
}

/// Runs `supdoc init` in the current directory.
///
/// With `--dry-run`, prints the starter file through `output` and writes nothing. Notes go
/// through `output` too, so `--quiet` and the data formats silence them.
///
/// # Errors
///
/// Returns `cli/init-exists` when `supdoc.config.toml` already exists, and `cli/init-write` when
/// the file cannot be created or the output cannot be written.
pub fn run(args: &InitArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let target = target(&context.cwd);

    if args.dry_run {
        refuse_if_present(&target)?;
        output
            .report(&StarterReport { contents: STARTER }, true)
            .map_err(|error| output_failed(&target, &error))?;
        output
            .note("Nothing written. Run `supdoc init` to write supdoc.config.toml.")
            .map_err(|error| output_failed(&target, &error))?;
    } else {
        write_new(&target)?;
        output
            .note(&format!("Wrote {CONFIG_FILE_NAME}"))
            .map_err(|error| output_failed(&target, &error))?;
    }
    Ok(Outcome::Clean)
}

/// Where the starter config goes: the current directory.
fn target(cwd: &AbsolutePath) -> AbsolutePath {
    let name = RelativePath::try_from(CONFIG_FILE_NAME.to_owned())
        .unwrap_or_else(|_| unreachable!("the config file name is a relative path"));
    cwd.join(&name)
}

/// Fails with `cli/init-exists` when something is already at `target`.
///
/// # Errors
///
/// Returns `cli/init-exists` when anything, including a dangling symlink, is at `target`.
fn refuse_if_present(target: &AbsolutePath) -> Result<(), CliError> {
    if target.as_path().symlink_metadata().is_ok() {
        Err(exists(target))
    } else {
        Ok(())
    }
}

/// Creates `target` with the starter contents, failing rather than replacing anything.
///
/// The file is created exclusively, so a config that appears between a check and the write is
/// still never overwritten.
///
/// # Errors
///
/// Returns `cli/init-exists` when `target` exists, and `cli/init-write` when it cannot be written.
fn write_new(target: &AbsolutePath) -> Result<(), CliError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target.as_path())
        .map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => exists(target),
            _ => write_failed(target, &error),
        })?;
    file.write_all(STARTER.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| write_failed(target, &error))
}

/// The refusal for an existing config (06 error table).
fn exists(target: &AbsolutePath) -> CliError {
    CliError::new(
        "cli/init-exists",
        format!("{} already exists", target.display()),
        "The existing file wasn't changed. Edit it directly, or delete it and run `supdoc init`.",
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for a config that could not be written.
fn write_failed(target: &AbsolutePath, error: &io::Error) -> CliError {
    CliError::new(
        "cli/init-write",
        format!("cannot write {}: {error}", target.display()),
        "check that the directory exists and that you can write to it",
        CliError::IO_EXIT_CODE,
    )
}

/// The error for output that could not be written.
fn output_failed(target: &AbsolutePath, error: &OutputError) -> CliError {
    CliError::new(
        "cli/init-write",
        format!("cannot report on {}: {error}", target.display()),
        "check that standard output and standard error are open and writable",
        CliError::IO_EXIT_CODE,
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use clap::Parser as _;

    use crate::cli::args::{Cli, Command};
    use crate::config::Config;
    use crate::output::{OutputFormat, OutputSettings, PathStyle, Verbosity};

    use super::*;

    /// A writer that keeps what it is given, shared with the test that reads it back.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the capture lock should be free");
            String::from_utf8(bytes.clone()).expect("captured output should be UTF-8")
        }
    }

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("the capture lock should be free")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose reader has hung up.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Text output rooted in `cwd`, writing to the given streams.
    fn output_to(cwd: &AbsolutePath, stdout: Box<dyn Write>, stderr: Box<dyn Write>) -> Output {
        let settings = OutputSettings {
            format: OutputFormat::Text,
            verbosity: Verbosity::Normal,
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
        };
        Output::new(settings, stdout, stderr)
    }

    /// Runs init in `context`, returning its outcome and what it wrote to stdout and stderr.
    fn run_captured(
        args: &InitArgs,
        context: &Context,
    ) -> (Result<Outcome, CliError>, String, String) {
        let (stdout, stderr) = (Capture::default(), Capture::default());
        let mut output = output_to(
            &context.cwd,
            Box::new(stdout.clone()),
            Box::new(stderr.clone()),
        );
        let result = run(args, context, &mut output);
        (result, stdout.text(), stderr.text())
    }

    /// A context rooted in `dir`, built from a real command line so the flags are genuine.
    fn context_in(dir: &tempfile::TempDir, flags: &[&str]) -> (InitArgs, Context) {
        let mut words = vec!["supdoc", "init"];
        words.extend_from_slice(flags);
        let cli = Cli::try_parse_from(words).expect("the init command line should parse");
        let Command::Init(init_args) = cli.command else {
            panic!("`supdoc init` should parse as the init command");
        };
        let cwd = AbsolutePath::new(dir.path()).expect("a temp directory should be absolute");
        (
            init_args,
            Context {
                global: cli.global,
                cwd,
            },
        )
    }

    fn config_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join(CONFIG_FILE_NAME)
    }

    #[test]
    fn starter_config_loads_as_a_valid_config() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &[]);
        let (result, _, _) = run_captured(&args, &context);
        let outcome = result.expect("init should write into an empty directory");
        assert_eq!(outcome, Outcome::Clean);

        let file = AbsolutePath::new(&config_path(&dir)).expect("the config path is absolute");
        let config = Config::load_file(&file).expect("the starter config should load");

        assert_eq!(
            config.files(),
            [file],
            "the starter is the only file in its chain"
        );
        assert!(
            config.namespaces().is_empty(),
            "the starter defines no namespaces"
        );
    }

    #[test]
    fn init_writes_exactly_the_starter_and_nothing_else() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &[]);

        let (result, _, _) = run_captured(&args, &context);
        let outcome = result.expect("init should write into an empty directory");

        assert_eq!(outcome, Outcome::Clean);
        let written = std::fs::read_to_string(config_path(&dir)).expect("the config should exist");
        assert_eq!(written, STARTER, "contents of the written config");
        let entries = std::fs::read_dir(dir.path()).expect("the directory should be readable");
        assert_eq!(entries.count(), 1, "init writes one file and no lockfile");
    }

    #[test]
    fn init_refuses_to_overwrite() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::write(config_path(&dir), "include = [\"docs\"]\n")
            .expect("the existing config should be written");
        let (args, context) = context_in(&dir, &[]);

        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("init should refuse to overwrite");

        assert_eq!(error.code(), "cli/init-exists");
        assert_eq!(error.exit_code(), 1);
        let after = std::fs::read_to_string(config_path(&dir)).expect("the config should remain");
        assert_eq!(
            after, "include = [\"docs\"]\n",
            "the existing config is untouched"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &["--dry-run"]);

        let (result, _, _) = run_captured(&args, &context);
        let outcome = result.expect("a dry run should succeed");

        assert_eq!(outcome, Outcome::Clean);
        assert!(
            !config_path(&dir).exists(),
            "a dry run must not create the config"
        );
    }

    #[test]
    fn dry_run_still_refuses_when_the_config_exists() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::write(config_path(&dir), "").expect("the existing config should be written");
        let (args, context) = context_in(&dir, &["--dry-run"]);

        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("a dry run should report the refusal");

        assert_eq!(error.code(), "cli/init-exists");
    }

    #[test]
    fn missing_directory_is_an_io_error() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, mut context) = context_in(&dir, &[]);
        context.cwd =
            AbsolutePath::new(&dir.path().join("gone")).expect("a nested path should be absolute");

        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("a missing directory cannot hold a config");

        assert_eq!((error.code(), error.exit_code()), ("cli/init-write", 74));
    }

    #[test]
    fn dry_run_prints_the_starter_on_stdout_and_the_note_on_stderr() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &["--dry-run"]);

        let (_, stdout, stderr) = run_captured(&args, &context);

        assert_eq!(stdout, STARTER, "stdout carries the data");
        assert!(
            stderr.starts_with("Nothing written."),
            "stderr carries the note, got: {stderr}"
        );
    }

    #[test]
    fn writing_the_file_notes_it_on_stderr_and_prints_no_data() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &[]);

        let (_, stdout, stderr) = run_captured(&args, &context);

        assert_eq!(stdout, "", "a real run prints no data");
        assert_eq!(stderr, format!("Wrote {CONFIG_FILE_NAME}\n"), "the note");
    }

    #[test]
    fn a_closed_pipe_on_a_dry_run_is_a_clean_exit() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &["--dry-run"]);
        let mut output = output_to(
            &context.cwd,
            Box::new(ClosedPipe),
            Box::new(Capture::default()),
        );

        let outcome = run(&args, &context, &mut output).expect("a closed pipe is not an error");

        assert_eq!(outcome, Outcome::Clean);
    }
}
