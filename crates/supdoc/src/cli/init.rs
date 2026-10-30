//! `supdoc init`: writes a starter config and nothing else (05 C10).
//!
//! The starter goes to `-c PATH` when given, otherwise to `supdoc.config.toml` in the current
//! directory. The lockfile is created by the first `update --yes`. An existing file is never
//! touched.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::CONFIG_FILE_NAME;
use crate::lock::error::shell_quote;
use crate::output::json::Envelope;
use crate::output::{Output, OutputError, Report, TextContext};
use crate::path::AbsolutePath;

use super::args::InitArgs;
use super::{CliError, Context, Outcome};

/// The file `supdoc init` writes. It loads as a valid config (see the tests). Every key is
/// commented out, so none of them is live: a live `include` or `exclude` would replace the
/// patterns of a config it later extends.
const STARTER: &str = "\
# supdoc.config.toml. Every key is optional.

# Files to scan for links. Globs, relative to this file's directory.
# Each pattern also matches everything beneath it. The default is everything.
# include = [\"**\"]

# Removed from include. Exclude always wins.
# exclude = []

# Another config to inherit from, relative to this file.
# extends = \"../../supdoc.config.toml\"

# Short names for directories, relative to this file.
# [namespaces]
# auth = \"services/auth\"

# How files are hashed. \"normalize\" drops the CR of each CRLF, so a checkout with other line
# endings still matches the lock. \"exact\" hashes every byte as is.
# [features.hash]
# line-endings = \"normalize\"
";

/// The starter config as output data, reported on a dry run and on a real write.
#[derive(Debug, Serialize, JsonSchema)]
struct StarterReport {
    /// Whether the file was written. False on `--dry-run`.
    applied: bool,
    /// Where the starter is, or would be, written.
    path: AbsolutePath,
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

/// Runs `supdoc init`, writing to `-c PATH` or to `supdoc.config.toml` in the current directory.
///
/// The starter is reported through `output` on both paths: raw in text and GitHub output, inside
/// the envelope in the JSON formats. With `--dry-run` nothing is written. Notes go through
/// `output` too, so `--quiet` and the data formats silence them.
///
/// # Errors
///
/// Returns `cli-init-exists` when the target file already exists, and `cli-init-write` when the
/// file cannot be created or the output cannot be written.
pub fn run(args: &InitArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let target = target(context);
    let label = label(&target, &context.cwd);
    let report = |applied| StarterReport {
        applied,
        path: target.clone(),
        contents: STARTER,
    };

    if args.dry_run {
        require_parent_directory(&target, context)?;
        refuse_if_present(&target, context)?;
        output
            .artifact(&report(false), STARTER, true)
            .map_err(|error| output_failed(&target, &error))?;
        output
            .note(&format!(
                "Nothing written. Run `{}` to write {label}.",
                command(context)
            ))
            .map_err(|error| output_failed(&target, &error))?;
    } else {
        write_new(&target, context)?;
        output
            .artifact(&report(true), "", true)
            .map_err(|error| output_failed(&target, &error))?;
        output
            .note(&format!("Wrote {label}"))
            .map_err(|error| output_failed(&target, &error))?;
    }
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc init`'s `--json` output: the whole envelope, with `data` typed as
/// the starter report.
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, StarterReport>))
}

/// Where the starter config goes: `-c PATH` resolved against the current directory, otherwise
/// the default config file name in the current directory.
fn target(context: &Context) -> AbsolutePath {
    let requested = context
        .global
        .config
        .as_deref()
        .unwrap_or_else(|| Path::new(CONFIG_FILE_NAME));
    AbsolutePath::new(&context.cwd.as_path().join(requested))
        .unwrap_or_else(|| unreachable!("a path joined onto an absolute directory is absolute"))
}

/// The `supdoc init` command that writes what a dry run previewed: with `-c PATH` when the
/// preview was for that path, so running it writes the same file.
fn command(context: &Context) -> String {
    context.global.config.as_deref().map_or_else(
        || "supdoc init".to_owned(),
        |path| {
            format!(
                "supdoc init -c {}",
                shell_quote(&path.display().to_string())
            )
        },
    )
}

/// Fails with `cli-init-write` when `target`'s directory is missing or is not a directory, the
/// same error a real run reports when it cannot create the file there.
///
/// # Errors
///
/// Returns `cli-init-write` when the parent directory cannot be found or is not a directory.
fn require_parent_directory(target: &AbsolutePath, context: &Context) -> Result<(), CliError> {
    let Some(parent) = target.as_path().parent() else {
        return Ok(());
    };
    match std::fs::metadata(parent) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(write_failed(
            target,
            &io::Error::from(io::ErrorKind::NotADirectory),
            context,
        )),
        Err(error) => Err(write_failed(target, &error, context)),
    }
}

/// `target` as a person reads it in a note: relative to `cwd` when it is beneath it.
fn label(target: &AbsolutePath, cwd: &AbsolutePath) -> String {
    target.strip_prefix(cwd).map_or_else(
        || target.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

/// Fails with `cli-init-exists` when something is already at `target`.
///
/// # Errors
///
/// Returns `cli-init-exists` when anything, including a dangling symlink, is at `target`.
fn refuse_if_present(target: &AbsolutePath, context: &Context) -> Result<(), CliError> {
    if target.as_path().symlink_metadata().is_ok() {
        Err(exists(target, context))
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
/// Returns `cli-init-exists` when `target` exists, and `cli-init-write` when it cannot be written.
fn write_new(target: &AbsolutePath, context: &Context) -> Result<(), CliError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target.as_path())
        .map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => exists(target, context),
            _ => write_failed(target, &error, context),
        })?;
    file.write_all(STARTER.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| write_failed(target, &error, context))
}

/// The refusal for an existing config (06 error table).
fn exists(target: &AbsolutePath, context: &Context) -> CliError {
    CliError::new(
        "cli-init-exists",
        format!("{} already exists", target.display()),
        format!(
            "The existing file wasn't changed. Edit it directly, or delete it and run `{}`.",
            command(context)
        ),
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for a config that could not be written.
fn write_failed(target: &AbsolutePath, error: &io::Error, context: &Context) -> CliError {
    CliError::new(
        "cli-init-write",
        format!("cannot write {}: {error}", target.display()),
        format!(
            "check that the directory exists and that you can write to it, then run `{}` again",
            command(context)
        ),
        CliError::IO_EXIT_CODE,
    )
}

/// The error for output that could not be written.
fn output_failed(target: &AbsolutePath, error: &OutputError) -> CliError {
    CliError::new(
        "cli-init-write",
        format!("cannot report on {}: {error}", target.display()),
        "check that standard output and standard error are open and writable, or redirect both \
         to a file with `> supdoc.out 2>&1`",
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

    /// Plain text settings rooted in `cwd`.
    fn text_settings(cwd: &AbsolutePath) -> OutputSettings {
        OutputSettings {
            format: OutputFormat::Text,
            verbosity: Verbosity::Normal,
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
        }
    }

    /// Text output rooted in `cwd`, writing to the given streams.
    fn output_to(cwd: &AbsolutePath, stdout: Box<dyn Write>, stderr: Box<dyn Write>) -> Output {
        Output::new(text_settings(cwd), stdout, stderr)
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
                home: None,
                claude_config_dir: None,
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
    fn starter_keeps_no_key_live_so_extends_inherits_the_parent_patterns() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::write(config_path(&dir), "exclude = [\"vendor\"]\n")
            .expect("the parent config should be written");
        let child = dir.path().join("child/grandchild");
        std::fs::create_dir_all(&child).expect("the child directory should be created");
        let extending = STARTER.replace("# extends = ", "extends = ");
        assert_ne!(
            extending, STARTER,
            "the starter should have an extends line"
        );
        let child_config = child.join(CONFIG_FILE_NAME);
        std::fs::write(&child_config, extending).expect("the child config should be written");

        let file = AbsolutePath::new(&child_config).expect("the child path is absolute");
        let config = Config::load_file(&file).expect("the extending starter should load");

        let vendor = AbsolutePath::new(&dir.path().join("vendor/lib.md"))
            .expect("the vendor path is absolute");
        assert!(
            config.excluded_by(&vendor).is_some(),
            "the parent's exclude should apply to the child"
        );
    }

    #[test]
    fn starter_has_no_live_include_or_exclude() {
        let live: Vec<&str> = STARTER
            .lines()
            .filter(|line| line.starts_with("include =") || line.starts_with("exclude ="))
            .collect();
        assert!(live.is_empty(), "live keys in the starter: {live:?}");
    }

    #[test]
    fn config_flag_writes_the_starter_there() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::create_dir(dir.path().join("sub")).expect("the sub directory should be created");
        let (args, context) = context_in(&dir, &["-c", "sub/x.toml"]);

        let (result, _, stderr) = run_captured(&args, &context);
        let _outcome = result.expect("init should write to the requested path");

        let written = std::fs::read_to_string(dir.path().join("sub/x.toml"))
            .expect("the starter should be at the requested path");
        assert_eq!(written, STARTER, "contents at the requested path");
        assert!(
            !config_path(&dir).exists(),
            "the default file must not be written when -c names another"
        );
        assert_eq!(stderr, "Wrote sub/x.toml\n", "the note names the path");
    }

    #[test]
    fn config_flag_refuses_when_the_path_exists() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::create_dir(dir.path().join("sub")).expect("the sub directory should be created");
        std::fs::write(dir.path().join("sub/x.toml"), "# mine\n")
            .expect("the existing file should be written");
        let (args, context) = context_in(&dir, &["-c", "sub/x.toml"]);

        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("init should refuse to overwrite the -c path");

        assert_eq!(error.code(), "cli-init-exists");
        let after = std::fs::read_to_string(dir.path().join("sub/x.toml"))
            .expect("the existing file should remain");
        assert_eq!(after, "# mine\n", "the existing file is untouched");
    }

    #[test]
    fn real_write_json_reports_applied_path_and_contents() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &["--json"]);
        let stdout = Capture::default();
        let settings = OutputSettings {
            format: context.global.format(),
            ..text_settings(&context.cwd)
        };
        let mut output = Output::new(
            settings,
            Box::new(stdout.clone()),
            Box::new(Capture::default()),
        );

        let _outcome =
            run(&args, &context, &mut output).expect("init should write into an empty directory");

        let json: serde_json::Value = serde_json::from_str(&stdout.text())
            .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", stdout.text()));
        assert_eq!(
            json.pointer("/data/applied"),
            Some(&true.into()),
            "applied in {json}"
        );
        assert_eq!(
            json.pointer("/data/contents"),
            Some(&STARTER.into()),
            "contents in {json}"
        );
        assert!(
            json.pointer("/data/path")
                .is_some_and(serde_json::Value::is_string),
            "path in {json}"
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

        assert_eq!(error.code(), "cli-init-exists");
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

        assert_eq!(error.code(), "cli-init-exists");
    }

    #[test]
    fn missing_directory_is_an_io_error() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, mut context) = context_in(&dir, &[]);
        context.cwd =
            AbsolutePath::new(&dir.path().join("gone")).expect("a nested path should be absolute");

        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("a missing directory cannot hold a config");

        assert_eq!((error.code(), error.exit_code()), ("cli-init-write", 74));
    }

    #[test]
    fn dry_run_with_config_flag_names_the_same_command_in_its_note() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &["--dry-run", "-c", "sub/x.toml"]);
        std::fs::create_dir(dir.path().join("sub")).expect("the sub directory should be created");

        let (_, _, stderr) = run_captured(&args, &context);

        assert_eq!(
            stderr, "Nothing written. Run `supdoc init -c sub/x.toml` to write sub/x.toml.\n",
            "the note repeats the -c flag"
        );
    }

    #[test]
    fn dry_run_reports_a_missing_parent_directory_like_a_real_run() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (dry_args, dry_context) = context_in(&dir, &["--dry-run", "-c", "gone/x.toml"]);
        let (real_args, real_context) = context_in(&dir, &["-c", "gone/x.toml"]);

        let (dry, _, dry_stdout) = run_captured(&dry_args, &dry_context);
        let (real, _, _) = run_captured(&real_args, &real_context);
        let dry = dry.expect_err("the preview should fail on a missing directory");
        let real = real.expect_err("the real run should fail on a missing directory");

        assert_eq!(
            (dry.code(), dry.exit_code(), dry.to_string()),
            (real.code(), real.exit_code(), real.to_string()),
            "the preview and the real run fail the same way"
        );
        assert_eq!(dry_stdout, "", "no note claims success");
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

    #[test]
    fn init_dry_run_json_validates_against_its_schema() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &["--json", "--dry-run"]);
        let stdout = Capture::default();
        let settings = OutputSettings {
            format: context.global.format(),
            ..text_settings(&context.cwd)
        };
        let mut output = Output::new(
            settings,
            Box::new(stdout.clone()),
            Box::new(Capture::default()),
        );
        let _outcome = run(&args, &context, &mut output).expect("the dry run should succeed");
        let json: serde_json::Value = serde_json::from_str(&stdout.text())
            .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", stdout.text()));
        let schema = serde_json::to_value(schema().expect("init is built, so it has a schema"))
            .expect("the init schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");

        let errors: Vec<String> = validator
            .iter_errors(&json)
            .map(|error| error.to_string())
            .collect();

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
    }
}
