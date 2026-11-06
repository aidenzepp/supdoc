//! `supdoc init`: writes a starter config and nothing else.
//!
//! The starter goes to `-c PATH` when given, otherwise to `supdoc.config.toml` in the current
//! directory. The lockfile is created by the first `update --yes`. An existing file is never
//! touched.

use std::io::{self, Write as _};

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::confine::{self, Admitted, Boundary, Escape};
use crate::config::{CONFIG_FILE_NAME, discover_config};
use crate::exit::ExitStatus;
use crate::lock::error::shell_quote;
use crate::output::json::Envelope;
use crate::output::paths::relative_to;
use crate::output::{Output, Report, TextContext};
use crate::path::{AbsolutePath, fixed};

use super::args::{InitArgs, OperatorPath};
use super::{CliError, Context, Outcome, cause};

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
/// Returns `cli-init-nested` when no `-c` is given and a config already governs the current
/// directory from above, `cli-init-exists` when the target file already exists, and
/// `cli-init-write` when the file cannot be created, and `cli-output-failed` when the
/// report cannot be written.
pub(crate) fn run(
    args: &InitArgs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let target = target(context);
    refuse_inside_project(&target, context)?;
    let label = relative_to(&context.cwd, &target);
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
            .map_err(|error| CliError::output_failed(&format!("the report on {label}"), &error))?;
        output
            .note(&format!(
                "Nothing written. Run `{}` to write {label}.",
                command(context)
            ))
            .map_err(|error| CliError::output_failed(&format!("the report on {label}"), &error))?;
    } else {
        write_new(&target, context)?;
        output
            .artifact(&report(true), "", true)
            .map_err(|error| CliError::output_failed(&format!("the report on {label}"), &error))?;
        output
            .note(&format!("Wrote {label}"))
            .map_err(|error| CliError::output_failed(&format!("the report on {label}"), &error))?;
    }
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc init`'s `--json` output: the whole envelope, with `data` typed as
/// the starter report.
#[must_use]
pub(crate) fn schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, StarterReport>)
}

/// Where the starter config goes: `-c PATH` resolved against the current directory, otherwise
/// the default config file name in the current directory.
fn target(context: &Context) -> AbsolutePath {
    match context.global.config.as_deref() {
        Some(requested) => context
            .cwd
            .resolve_operator(&OperatorPath::typed(requested)),
        None => context.cwd.beneath(&fixed::CONFIG_FILE),
    }
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

/// Fails with `cli-init-nested` when no `-c` was given and discovery finds a config above the
/// default target: a second config there would become the nearest one and split the project.
///
/// A config at the target itself is left to the `cli-init-exists` check, and a discovery error
/// is left to the commands that read the config.
///
/// # Errors
///
/// Returns `cli-init-nested` when a config other than the default target is found.
fn refuse_inside_project(target: &AbsolutePath, context: &Context) -> Result<(), CliError> {
    if context.global.config.is_some() {
        return Ok(());
    }
    match discover_config(&context.cwd) {
        Ok(Some(ancestor)) if ancestor != *target => Err(CliError::new(
            "cli-init-nested",
            format!(
                "{} already configures this directory",
                relative_to(&context.cwd, &ancestor)
            ),
            "A second config here would split the project: a new root and lockfile, and none of \
             the namespaces above. To start a separate project on purpose, run \
             `supdoc init -c ./supdoc.config.toml`.",
            ExitStatus::Usage,
        )),
        _ => Ok(()),
    }
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

/// The mode a new config is created with, before the umask: what an ordinary file create uses.
const NEW_FILE_MODE: u32 = 0o666;

/// Creates `target` with the starter contents, failing rather than replacing anything.
///
/// The starter is written to a temporary sibling and linked into place, which fails if `target`
/// exists, so a config that appears between a check and the write is never overwritten and a
/// failed write never leaves a partial config behind. Both paths are admitted against `target`'s
/// directory immediately before each is created, so a symlink there can't lead the write out.
///
/// # Errors
///
/// Returns `cli-init-exists` when `target` exists, and `cli-init-write` when it cannot be written.
fn write_new(target: &AbsolutePath, context: &Context) -> Result<(), CliError> {
    let boundary = Boundary::resolve(&target.parent().unwrap_or_else(|| target.clone()));
    write_beneath(&boundary, target, context, || ())
}

/// [`write_new`] against `boundary`, `target`'s directory as resolved before the write.
/// `created` runs once the temporary exists, before anything is written to it.
///
/// The temporary is admitted again just before it is removed. When that is refused, because the
/// directory was swapped for a symlink leading out after the create, the temporary is left where
/// it was created rather than removing whatever the swapped path now names.
///
/// # Errors
///
/// As [`write_new`]; `cli-init-exists` also when the temporary file's name is taken.
fn write_beneath(
    boundary: &Boundary,
    target: &AbsolutePath,
    context: &Context,
    created: impl FnOnce(),
) -> Result<(), CliError> {
    write_beneath_with(boundary, target, context, created, confine::hard_link_new)
}

/// [`write_beneath`] with the step that links the temporary into place supplied, so a filesystem
/// without hard links can be simulated.
///
/// # Errors
///
/// As [`write_beneath`].
fn write_beneath_with(
    boundary: &Boundary,
    target: &AbsolutePath,
    context: &Context,
    created: impl FnOnce(),
    link_step: impl FnOnce(&Admitted<'_>, &Admitted<'_>) -> io::Result<()>,
) -> Result<(), CliError> {
    let failed = |error: io::Error| match error.kind() {
        io::ErrorKind::AlreadyExists => exists(target, context),
        _ => write_failed(target, &error, context),
    };
    let temporary_path = temporary_sibling(target)
        .ok_or_else(|| failed(io::Error::other("it has no directory to write beside")))?;
    let temporary_failed = |error: io::Error| match error.kind() {
        io::ErrorKind::AlreadyExists => temporary_taken(&temporary_path, context),
        _ => write_failed(target, &error, context),
    };
    let temporary = boundary
        .admit(&temporary_path)
        .map_err(|escape| temporary_failed(refusal(&temporary_path, &escape)))?;
    let mut file = confine::create_new(&temporary, NEW_FILE_MODE).map_err(temporary_failed)?;
    created();
    let linked = file
        .write_all(STARTER.as_bytes())
        .and_then(|()| file.flush())
        .and_then(|()| link_into_place(boundary, &temporary, target, link_step));
    drop(file);
    // Best effort: this run created the temporary, and the outcome is already decided.
    if let Ok(again) = boundary.admit(&temporary_path) {
        drop(confine::remove_file(&again));
    }
    linked.map_err(failed)
}

/// Links the written `temporary` to `target`, once `target` is admitted against `boundary`, with
/// `link_step` making the link. When it fails because the filesystem
/// has no hard links, the starter is created at `target` directly instead.
///
/// # Errors
///
/// `AlreadyExists` when anything is at `target`; otherwise fails when `target` is refused or the
/// link can't be made. The fallback also fails when its write fails.
fn link_into_place(
    boundary: &Boundary,
    temporary: &Admitted<'_>,
    target: &AbsolutePath,
    link_step: impl FnOnce(&Admitted<'_>, &Admitted<'_>) -> io::Result<()>,
) -> io::Result<()> {
    let link = boundary
        .admit(target)
        .map_err(|escape| refusal(target, &escape))?;
    match link_step(temporary, &link) {
        Err(error) if links_unsupported(&error) => create_in_place(boundary, target, |file| {
            file.write_all(STARTER.as_bytes())
                .and_then(|()| file.flush())
        }),
        linked => linked,
    }
}

/// Whether `error` says the filesystem can't make hard links: `ErrorKind::Unsupported`, or the OS
/// codes that mean the same (`ENOTSUP`, `EOPNOTSUPP`, and on Linux the `EPERM` that vfat gives).
fn links_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    let Some(code) = error.raw_os_error() else {
        return false;
    };
    code == libc::ENOTSUP
        || code == libc::EOPNOTSUPP
        || (cfg!(target_os = "linux") && code == libc::EPERM)
}

/// Creates `target` itself, admitted against `boundary`, and fills it with `write`. Refuses an
/// existing path. When `write` fails, the file this call created is removed, and only that file.
///
/// # Errors
///
/// `AlreadyExists` when anything is at `target`; otherwise fails when `target` is refused, can't
/// be created, or `write` fails.
fn create_in_place(
    boundary: &Boundary,
    target: &AbsolutePath,
    write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let admitted = boundary
        .admit(target)
        .map_err(|escape| refusal(target, &escape))?;
    let mut file = confine::create_new(&admitted, NEW_FILE_MODE)?;
    let written = write(&mut file);
    drop(file);
    if written.is_err()
        && let Ok(again) = boundary.admit(target)
    {
        drop(confine::remove_file(&again));
    }
    written
}

/// The I/O error for a path [`Boundary::admit`] refused, so it reports like a failed create:
/// `AlreadyExists` when anything, such as a symlink leading out, is already at `path`.
fn refusal(path: &AbsolutePath, escape: &Escape) -> io::Error {
    if path.as_path().symlink_metadata().is_ok() {
        return io::Error::from(io::ErrorKind::AlreadyExists);
    }
    match escape {
        Escape::Lexical => io::Error::other("it isn't in the directory it names"),
        Escape::Real { real: Some(_) } => {
            io::Error::other("a symlink along it leads out of its directory")
        }
        Escape::Real { real: None } => io::Error::other("a symlink along it can't be followed"),
    }
}

/// A hidden file beside `target` that this run alone writes before linking it into place.
///
/// `None` only when the sibling isn't absolute, which an absolute `target` rules out.
fn temporary_sibling(target: &AbsolutePath) -> Option<AbsolutePath> {
    let name = target
        .as_path()
        .file_name()
        .map_or_else(|| CONFIG_FILE_NAME.into(), |name| name.to_string_lossy());
    let sibling = target
        .as_path()
        .with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    AbsolutePath::new(&sibling)
}

/// The refusal for an existing config, naming it as the notes do.
fn exists(target: &AbsolutePath, context: &Context) -> CliError {
    CliError::new(
        "cli-init-exists",
        format!("{} already exists", relative_to(&context.cwd, target)),
        format!(
            "The existing file wasn't changed. Edit it directly, or delete it and run `{}`.",
            command(context)
        ),
        ExitStatus::Failed,
    )
}

/// The refusal for a temporary file name already in use, naming it as the notes do.
fn temporary_taken(temporary: &AbsolutePath, context: &Context) -> CliError {
    CliError::new(
        "cli-init-exists",
        format!(
            "the temporary file {} already exists",
            relative_to(&context.cwd, temporary)
        ),
        format!(
            "supdoc writes the config there before linking it into place. Move or delete that \
             file, then run `{}` again.",
            command(context)
        ),
        ExitStatus::Failed,
    )
}

/// The error for a config that could not be written, naming it as the notes do.
fn write_failed(target: &AbsolutePath, error: &io::Error, context: &Context) -> CliError {
    let directory = target
        .as_path()
        .parent()
        .unwrap_or_else(|| target.as_path());
    let directory = shell_quote(&directory.to_string_lossy());
    let fix = match error.kind() {
        io::ErrorKind::NotFound => format!("create the directory with `mkdir -p {directory}`"),
        io::ErrorKind::NotADirectory => {
            format!("a file is in the way of the directory {directory}; move or delete that file")
        }
        io::ErrorKind::PermissionDenied => {
            format!("make the directory {directory} writable by you")
        }
        _ => format!("check that you can write to the directory {directory}"),
    };
    CliError::new(
        "cli-init-write",
        format!(
            "cannot write {}: {}",
            relative_to(&context.cwd, target),
            cause(error)
        ),
        format!("{fix}, then run `{}` again", command(context)),
        ExitStatus::Io,
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use clap::Parser as _;

    use crate::cli::args::{Cli, Command};
    use crate::cli::testing::{Capture, ClosedPipe};
    use crate::config::Config;
    use crate::output::{OutputFormat, OutputSettings, PathStyle, Verbosity};
    use crate::scan::walk::GlobalExcludes;

    use super::*;

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
                global_excludes: GlobalExcludes::none(),
                trust: crate::trust::TrustEnv::default(),
            },
        )
    }

    #[test]
    fn refuses_inside_a_configured_project_unless_config_is_given() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "# root\n")
            .expect("the root config should be written");
        std::fs::create_dir(dir.path().join("sub")).expect("the sub directory should be created");
        let sub = tempfile::TempDir::new_in(dir.path().join("sub"))
            .expect("a nested directory should be created");

        let (args, context) = context_in(&sub, &[]);
        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("init should refuse below a configured directory");
        assert_eq!(error.code(), "cli-init-nested");
        assert!(
            !sub.path().join(CONFIG_FILE_NAME).exists(),
            "nothing is written when init refuses"
        );

        let (args, context) = context_in(&sub, &["-c", "./supdoc.config.toml"]);
        let (result, _, _) = run_captured(&args, &context);
        let _outcome = result.expect("init -c should write the nested config on purpose");
        assert!(
            sub.path().join(CONFIG_FILE_NAME).exists(),
            "the config is written"
        );
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
    fn refusal_and_write_failure_name_the_path_from_the_current_directory() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::create_dir(dir.path().join("sub")).expect("the sub directory should be created");
        std::fs::write(dir.path().join("sub/x.toml"), "# mine\n")
            .expect("the existing file should be written");
        let (exists_args, exists_context) = context_in(&dir, &["-c", "sub/x.toml"]);
        let (missing_args, missing_context) = context_in(&dir, &["-c", "nope/x.toml"]);

        let (exists, _, _) = run_captured(&exists_args, &exists_context);
        let (missing, _, _) = run_captured(&missing_args, &missing_context);

        let exists = exists.expect_err("init should refuse to overwrite the -c path");
        let missing = missing.expect_err("init should fail without the -c path's directory");
        assert_eq!(
            [exists.message(), missing.message()],
            [
                "sub/x.toml already exists",
                "cannot write nope/x.toml: no such file or directory",
            ],
            "the messages should name the file as typed, not by its absolute path"
        );
    }

    #[test]
    fn refusal_and_write_failure_climb_out_of_a_subdirectory_as_typed() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::create_dir(dir.path().join("docs")).expect("the docs directory should be created");
        std::fs::write(config_path(&dir), "# mine\n")
            .expect("the existing config should be written");
        let docs = AbsolutePath::new(&dir.path().join("docs")).expect("a nested path is absolute");
        let in_docs = |flags: &[&str]| {
            let (args, mut context) = context_in(&dir, flags);
            context.cwd = docs.clone();
            run_captured(&args, &context).0
        };

        let exists = in_docs(&["-c", "../supdoc.config.toml"]);
        let missing = in_docs(&["-c", "../nope/x.toml"]);

        let exists = exists.expect_err("init should refuse to overwrite the -c path");
        let missing = missing.expect_err("init should fail without the -c path's directory");
        assert_eq!(
            [exists.message(), missing.message()],
            [
                "../supdoc.config.toml already exists",
                "cannot write ../nope/x.toml: no such file or directory",
            ],
            "from a subdirectory the messages should name the file as typed, not absolutely"
        );
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
        assert_eq!(
            json.pointer("/data/path"),
            Some(
                &dir.path()
                    .join("supdoc.config.toml")
                    .to_string_lossy()
                    .into()
            ),
            "path in {json}"
        );
    }

    #[test]
    fn write_failure_helps_by_kind_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::write(dir.path().join("file"), "").expect("the blocking file should be written");
        let (missing_args, missing_context) = context_in(&dir, &["-c", "nope/x.toml"]);
        let (blocked_args, blocked_context) = context_in(&dir, &["-c", "file/x.toml"]);

        let (missing, _, _) = run_captured(&missing_args, &missing_context);
        let (blocked, _, _) = run_captured(&blocked_args, &blocked_context);

        let missing = missing.expect_err("a missing directory cannot hold a config");
        let blocked = blocked.expect_err("a file cannot hold a config");
        let root = dir.path().to_string_lossy();
        assert!(
            missing.help().contains(&format!("mkdir -p {root}/nope")),
            "help was: {}",
            missing.help()
        );
        assert!(
            blocked.help().contains(&format!("{root}/file"))
                && blocked.help().contains("in the way"),
            "help was: {}",
            blocked.help()
        );
        let leftovers = std::fs::read_dir(dir.path())
            .expect("the directory should be readable")
            .count();
        assert_eq!(leftovers, 1, "only the blocking file remains");
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

    /// A temp dir holding an empty `project` the run starts in and an empty `outside` beside it.
    fn project_and_outside() -> (tempfile::TempDir, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        std::fs::create_dir(dir.path().join("project"))
            .expect("the project directory should be created");
        std::fs::create_dir(dir.path().join("outside"))
            .expect("the outside directory should be created");
        let project = tempfile::TempDir::new_in(dir.path().join("project"))
            .expect("the run directory should be created");
        (dir, project)
    }

    /// The names directly inside `dir`.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("the directory should be listable")
            .map(|entry| {
                entry
                    .expect("a directory entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_dangling_symlink_to_outside_at_the_target_is_refused_and_nothing_is_created_outside() {
        let (dir, project) = project_and_outside();
        let outside = dir.path().join("outside");
        std::os::unix::fs::symlink(outside.join("planted.toml"), config_path(&project))
            .expect("the dangling symlink should be created");
        let (args, context) = context_in(&project, &["-c", "./supdoc.config.toml"]);

        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("init should refuse a symlink leading out at its target");

        assert_eq!(error.code(), "cli-init-exists");
        assert!(
            names_in(&outside).is_empty(),
            "nothing should be created outside: {:?}",
            names_in(&outside)
        );
        assert_eq!(
            names_in(project.path()),
            [CONFIG_FILE_NAME],
            "only the planted symlink remains; no temporary file is left"
        );
    }

    #[test]
    fn the_temporary_sibling_is_admitted_under_the_targets_directory() {
        let (dir, project) = project_and_outside();
        let outside = dir.path().join("outside");
        let (args, context) = context_in(&project, &["-c", "./supdoc.config.toml"]);
        let temporary = temporary_sibling(&target(&context))
            .expect("the temporary sibling of an absolute target should be absolute");
        std::os::unix::fs::symlink(outside.join("planted.tmp"), temporary.as_path())
            .expect("the dangling symlink should be created");

        let (result, _, _) = run_captured(&args, &context);
        let _error = result.expect_err("init should refuse a symlink leading out at its temporary");

        assert!(
            names_in(&outside).is_empty(),
            "nothing should be created outside: {:?}",
            names_in(&outside)
        );
        assert!(
            !config_path(&project).exists(),
            "no config is linked into place when the temporary is refused"
        );
    }

    #[test]
    fn a_directory_swapped_for_a_symlink_to_outside_after_resolving_is_refused() {
        let (dir, project) = project_and_outside();
        let outside = dir.path().join("outside");
        let docs = project.path().join("docs");
        std::fs::create_dir(&docs).expect("the docs directory should be created");
        let (_, context) = context_in(&project, &["-c", "docs/supdoc.config.toml"]);
        let target = target(&context);
        let boundary = Boundary::resolve(
            &target
                .parent()
                .expect("the target should have a parent directory"),
        );
        std::fs::rename(&docs, project.path().join("moved"))
            .expect("the docs directory should be movable");
        std::os::unix::fs::symlink(&outside, &docs).expect("the swapped symlink should be created");
        // Read-only, so a create that wasn't refused first fails with a different error.
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500))
            .expect("the outside directory should be made read-only");

        let error = write_beneath(&boundary, &target, &context, || ())
            .expect_err("init should refuse a directory that now leads outside");

        assert_eq!(
            (error.code(), error.message()),
            (
                "cli-init-write",
                "cannot write docs/supdoc.config.toml: a symlink along it leads out of its directory"
            )
        );
        assert!(
            names_in(&outside).is_empty(),
            "nothing should be created outside: {:?}",
            names_in(&outside)
        );
    }

    #[test]
    fn a_directory_swapped_out_after_the_create_leaves_the_outside_file_of_the_temporarys_name() {
        let (dir, project) = project_and_outside();
        let outside = dir.path().join("outside");
        let docs = project.path().join("docs");
        std::fs::create_dir(&docs).expect("the docs directory should be created");
        let (_, context) = context_in(&project, &["-c", "docs/supdoc.config.toml"]);
        let target = target(&context);
        let boundary = Boundary::resolve(
            &target
                .parent()
                .expect("the target should have a parent directory"),
        );
        let temporary = temporary_sibling(&target)
            .expect("the temporary sibling of an absolute target should be absolute");
        let name = temporary
            .as_path()
            .file_name()
            .expect("the temporary should have a file name")
            .to_string_lossy()
            .into_owned();
        let swap = || {
            std::fs::write(outside.join(&name), "not ours")
                .expect("the outside file should be written");
            std::fs::rename(&docs, project.path().join("moved"))
                .expect("the docs directory should be movable");
            std::os::unix::fs::symlink(&outside, &docs)
                .expect("the swapped symlink should be created");
        };

        let error = write_beneath(&boundary, &target, &context, swap)
            .expect_err("the link should be refused once the directory leads outside");

        assert_eq!(error.code(), "cli-init-write");
        assert_eq!(
            std::fs::read_to_string(outside.join(&name))
                .expect("the outside file of the temporary's name should survive"),
            "not ours"
        );
        assert_eq!(
            names_in(&outside),
            std::slice::from_ref(&name),
            "no config should be linked in outside"
        );
        assert_eq!(
            names_in(&project.path().join("moved")),
            [name],
            "the run's own temporary is left in the moved directory"
        );
    }

    #[test]
    fn a_taken_temporary_name_is_named_in_the_refusal() {
        let dir = tempfile::tempdir().expect("a temp directory should be created");
        let (args, context) = context_in(&dir, &[]);
        let temporary = temporary_sibling(&target(&context))
            .expect("the temporary sibling of an absolute target should be absolute");
        std::fs::write(temporary.as_path(), "someone else's")
            .expect("the occupying file should be written");

        let (result, _, _) = run_captured(&args, &context);
        let error = result.expect_err("init should refuse a taken temporary name");

        let name = temporary
            .as_path()
            .file_name()
            .expect("the temporary should have a file name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            (error.code(), error.message()),
            (
                "cli-init-exists",
                format!("the temporary file {name} already exists").as_str()
            )
        );
        assert!(
            !config_path(&dir).exists(),
            "no config is written when the temporary name is taken"
        );
        assert_eq!(
            std::fs::read_to_string(temporary.as_path()).expect("the occupying file should remain"),
            "someone else's"
        );
    }

    #[test]
    fn the_target_is_admitted_again_before_the_link() {
        let (dir, project) = project_and_outside();
        let outside = dir.path().join("outside");
        let docs = project.path().join("docs");
        std::fs::create_dir(&docs).expect("the docs directory should be created");
        let (_, context) = context_in(&project, &["-c", "docs/supdoc.config.toml"]);
        let target = target(&context);
        let boundary = Boundary::resolve(
            &target
                .parent()
                .expect("the target should have a parent directory"),
        );
        let temporary = temporary_sibling(&target)
            .expect("the temporary sibling of an absolute target should be absolute");
        let temporary = boundary
            .admit(&temporary)
            .expect("the temporary should be admitted before the swap");
        drop(confine::create_new(&temporary, NEW_FILE_MODE).expect("the temporary is creatable"));
        let name = temporary
            .as_path()
            .file_name()
            .expect("the temporary should have a file name");
        std::fs::write(outside.join(name), "planted").expect("the planted file should be written");
        std::fs::rename(&docs, project.path().join("moved"))
            .expect("the docs directory should be movable");
        std::os::unix::fs::symlink(&outside, &docs).expect("the swapped symlink should be created");

        let error = link_into_place(&boundary, &temporary, &target, confine::hard_link_new)
            .expect_err("the link should be refused once the directory leads outside");

        assert_eq!(
            error.to_string(),
            "a symlink along it leads out of its directory"
        );
        assert_eq!(
            names_in(&outside),
            [name.to_string_lossy().into_owned()],
            "no config should be linked in outside"
        );
    }

    /// A hard-link step that fails with `error`.
    fn link_fails(
        error: impl Fn() -> io::Error,
    ) -> impl FnOnce(&Admitted<'_>, &Admitted<'_>) -> io::Result<()> {
        move |_, _| Err(error())
    }

    /// Runs `write_beneath_with` in a fresh project whose hard link fails with `error`.
    fn init_with_failing_link(
        error: impl Fn() -> io::Error,
        existing: Option<&str>,
    ) -> (tempfile::TempDir, tempfile::TempDir, Result<(), CliError>) {
        let (dir, project) = project_and_outside();
        let (_, context) = context_in(&project, &[]);
        let target = target(&context);
        if let Some(text) = existing {
            std::fs::write(target.as_path(), text).expect("the existing config should be written");
        }
        let boundary = Boundary::resolve(
            &target
                .parent()
                .expect("the target should have a parent directory"),
        );
        let result = write_beneath_with(&boundary, &target, &context, || (), link_fails(error));
        (dir, project, result)
    }

    #[test]
    fn without_hard_links_the_starter_is_written_in_place() {
        for error in [
            || io::Error::from(io::ErrorKind::Unsupported),
            || io::Error::from_raw_os_error(libc::ENOTSUP),
            || io::Error::from_raw_os_error(libc::EOPNOTSUPP),
        ] {
            let (_dir, project, result) = init_with_failing_link(error, None);

            result.expect("the fallback should write the config");
            assert_eq!(
                names_in(project.path()),
                [CONFIG_FILE_NAME],
                "only the config should remain"
            );
            assert_eq!(
                std::fs::read_to_string(project.path().join(CONFIG_FILE_NAME))
                    .expect("the config should be readable"),
                STARTER
            );
        }
    }

    #[test]
    fn the_fallback_never_replaces_an_existing_config() {
        let (_dir, project, result) =
            init_with_failing_link(|| io::Error::from(io::ErrorKind::Unsupported), Some("mine"));

        let error = result.expect_err("an existing config should be refused");

        assert_eq!(error.code(), "cli-init-exists");
        assert_eq!(
            std::fs::read_to_string(project.path().join(CONFIG_FILE_NAME))
                .expect("the config should be readable"),
            "mine"
        );
        assert_eq!(names_in(project.path()), [CONFIG_FILE_NAME]);
    }

    #[test]
    fn another_link_error_still_reports_a_write_failure() {
        let (_dir, project, result) =
            init_with_failing_link(|| io::Error::from(io::ErrorKind::PermissionDenied), None);

        let error = result.expect_err("a plain link failure should not fall back");

        assert_eq!(error.code(), "cli-init-write");
        assert!(
            names_in(project.path()).is_empty(),
            "nothing should be left behind"
        );
    }

    #[test]
    fn a_failed_fallback_write_removes_only_the_file_it_created() {
        let (_dir, project) = project_and_outside();
        let (_, context) = context_in(&project, &[]);
        let target = target(&context);
        let boundary = Boundary::resolve(
            &target
                .parent()
                .expect("the target should have a parent directory"),
        );

        let error = create_in_place(&boundary, &target, |_| Err(io::Error::other("disk full")))
            .expect_err("the write failure should be reported");

        assert_eq!(error.to_string(), "disk full");
        assert!(
            names_in(project.path()).is_empty(),
            "the half-made config should be removed"
        );
    }

    #[test]
    fn the_fallback_is_admitted_and_refuses_a_directory_swapped_out() {
        let (dir, project) = project_and_outside();
        let outside = dir.path().join("outside");
        let docs = project.path().join("docs");
        std::fs::create_dir(&docs).expect("the docs directory should be created");
        let (_, context) = context_in(&project, &["-c", "docs/supdoc.config.toml"]);
        let target = target(&context);
        let boundary = Boundary::resolve(
            &target
                .parent()
                .expect("the target should have a parent directory"),
        );
        std::fs::rename(&docs, project.path().join("moved"))
            .expect("the docs directory should be movable");
        std::os::unix::fs::symlink(&outside, &docs).expect("the swapped symlink should be created");

        let error = create_in_place(&boundary, &target, |_| Ok(()))
            .expect_err("a directory leading outside should be refused");

        assert_eq!(
            error.to_string(),
            "a symlink along it leads out of its directory"
        );
        assert!(
            names_in(&outside).is_empty(),
            "nothing should be created outside"
        );
    }

    #[test]
    fn a_failed_fallback_leaves_an_existing_config_alone() {
        let (_dir, project) = project_and_outside();
        let (_, context) = context_in(&project, &[]);
        let target = target(&context);
        std::fs::write(target.as_path(), "mine").expect("the existing config should be written");
        let boundary = Boundary::resolve(
            &target
                .parent()
                .expect("the target should have a parent directory"),
        );

        let error = create_in_place(&boundary, &target, |_| Err(io::Error::other("unreached")))
            .expect_err("an existing config should be refused");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(target.as_path()).expect("the config should be readable"),
            "mine"
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

        let (dry, _, dry_stderr) = run_captured(&dry_args, &dry_context);
        let (real, _, _) = run_captured(&real_args, &real_context);
        let dry = dry.expect_err("the preview should fail on a missing directory");
        let real = real.expect_err("the real run should fail on a missing directory");

        assert_eq!(
            (dry.code(), dry.exit_code(), dry.to_string()),
            (real.code(), real.exit_code(), real.to_string()),
            "the preview and the real run fail the same way"
        );
        assert_eq!(dry_stderr, "", "no note claims success");
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
        let schema = serde_json::to_value(schema()).expect("the init schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");

        let errors: Vec<String> = validator
            .iter_errors(&json)
            .map(|error| error.to_string())
            .collect();

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
    }
}
