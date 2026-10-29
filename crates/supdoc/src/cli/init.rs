//! `supdoc init`: writes a starter `supdoc.config.toml` and nothing else (05 C10).
//!
//! The lockfile is created by the first `update --yes`. An existing config is never touched.

use std::fs::OpenOptions;
use std::io::{self, Write as _};

use crate::config::CONFIG_FILE_NAME;
use crate::path::{AbsolutePath, RelativePath};

use super::args::InitArgs;
use super::{CliError, Context, Outcome};

/// The exit code for an I/O error (06 X7).
const IO_EXIT_CODE: u8 = 74;

/// The exit code for a refused write (06 X7).
const REFUSED_EXIT_CODE: u8 = 1;

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

/// Runs `supdoc init` in the current directory.
///
/// With `--dry-run`, prints the starter file to standard output and writes nothing.
///
/// # Errors
///
/// Returns `cli/init-exists` when `supdoc.config.toml` already exists, and `cli/init-write` when
/// the file cannot be created.
pub fn run(args: &InitArgs, context: &Context) -> Result<Outcome, CliError> {
    let target = target(&context.cwd);
    let quiet = context.global.quiet;

    if args.dry_run {
        refuse_if_present(&target)?;
        emit(&mut io::stdout().lock(), STARTER);
        note(
            quiet,
            "Nothing written. Run `supdoc init` to write supdoc.config.toml.\n",
        );
    } else {
        write_new(&target)?;
        note(quiet, &format!("Wrote {CONFIG_FILE_NAME}\n"));
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
        REFUSED_EXIT_CODE,
    )
}

/// The error for a config that could not be written.
fn write_failed(target: &AbsolutePath, error: &io::Error) -> CliError {
    CliError::new(
        "cli/init-write",
        format!("cannot write {}: {error}", target.display()),
        "check that the directory exists and that you can write to it",
        IO_EXIT_CODE,
    )
}

/// Writes `text` to `sink`, ignoring failure: data that cannot be delivered (a closed pipe) is
/// not worth failing a command that otherwise succeeded.
fn emit(sink: &mut impl io::Write, text: &str) {
    drop(sink.write_all(text.as_bytes()).and_then(|()| sink.flush()));
}

/// Tells the user what happened on standard error, unless `--quiet` asks for data only.
fn note(quiet: bool, text: &str) {
    if !quiet {
        emit(&mut io::stderr().lock(), text);
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::args::{Cli, Command};
    use crate::config::Config;

    use super::*;

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
        let outcome = run(&args, &context).expect("init should write into an empty directory");
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

        let outcome = run(&args, &context).expect("init should write into an empty directory");

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

        let error = run(&args, &context).expect_err("init should refuse to overwrite");

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

        let outcome = run(&args, &context).expect("a dry run should succeed");

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

        let error = run(&args, &context).expect_err("a dry run should report the refusal");

        assert_eq!(error.code(), "cli/init-exists");
    }

    #[test]
    fn missing_directory_is_an_io_error() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, mut context) = context_in(&dir, &[]);
        context.cwd =
            AbsolutePath::new(&dir.path().join("gone")).expect("a nested path should be absolute");

        let error = run(&args, &context).expect_err("a missing directory cannot hold a config");

        assert_eq!((error.code(), error.exit_code()), ("cli/init-write", 74));
    }
}
