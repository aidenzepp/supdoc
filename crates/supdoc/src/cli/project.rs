//! The setup every config-loading command shares: load the config, root the output at it, narrow
//! the scope to `[PATHS]`, and read the lockfile.
//!
//! [`load_config`] sets the output root itself, so no command can load a config and then report
//! paths against the wrong directory.

use std::io::ErrorKind;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::check::plan::SettingsChange;
use crate::config::{Config, ConfigError, config_size_limit, read_config_text};
use crate::exit::ExitStatus;
use crate::finding::{Location, Snippet};
use crate::hash::HashSettings;
use crate::link::SourcePath;
use crate::lock::error::{LockError, SettingsMismatch, shell_quote};
use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
use crate::namespace::NamespaceName;
use crate::output::Output;
use crate::output::paths::relative_to;
use crate::path::AbsolutePath;
use crate::scan::model::Scope;

use super::{CliError, Context, ErrorSite, cause};

/// The config `--config` names, or the one found by searching upward from the current directory,
/// with the `[limits]` flags and variables put in effect over its own, so every reader the
/// command runs takes its ceiling from [`Config::limits_in_effect`].
///
/// On success, `output` reports paths against the config's root from then on. `command` names the
/// command in the fallback help, as in ``run `supdoc list` again``: it is the whole command to
/// run, arguments included, and the fallback adds `-c PATH` when the config came from there.
///
/// # Errors
///
/// Returns the config's own error, with its code, help and exit code.
pub(crate) fn load_config(context: &Context, output: &mut Output) -> Result<Config, CliError> {
    let given = &context.global.limits.0;
    let loaded = match &context.global.config {
        Some(file) => Config::load_file_under(&absolute(&context.cwd, file), given),
        None => Config::load_under(&context.cwd, given),
    };
    let config = loaded.map_err(|error| config_failure(&error, context, output))?;
    output.set_root(config.root().clone());
    Ok(config)
}

/// The shell command that runs `command` from the current directory: `supdoc command`, with
/// `-c PATH` in front when the run was pointed at a config, so it loads the same one.
pub(crate) fn invocation(context: &Context, command: &str) -> String {
    match context.global.config.as_deref() {
        Some(file) => format!(
            "supdoc -c {} {command}",
            shell_quote(&file.display().to_string())
        ),
        None => format!("supdoc {command}"),
    }
}

/// The shell command that runs `supdoc name OPERAND... FLAG...` from the current directory: every
/// operand is quoted for the shell, and `-c PATH` leads when the run was pointed at a config, so
/// the command acts on the same project and the same paths.
pub(crate) fn command_line<S: AsRef<str>>(
    context: &Context,
    name: &str,
    operands: &[S],
    flags: &[&str],
) -> String {
    let mut command = name.to_owned();
    for operand in operands {
        command.push(' ');
        command.push_str(&shell_quote(operand.as_ref()));
    }
    for flag in flags {
        command.push(' ');
        command.push_str(flag);
    }
    invocation(context, &command)
}

/// The fatal error for a config that did not load, pointing at the offending text when the config
/// error knows where it is.
///
/// The message and help name files relative to the current directory, as the location does: no
/// project root is known yet, and a command in the help then runs as written.
pub(crate) fn config_failure(
    error: &ConfigError,
    context: &Context,
    output: &mut Output,
) -> CliError {
    let failure = CliError::new(
        error.code(),
        error.message_from(&context.cwd),
        error.help_from(&context.cwd),
        error.status(),
    );
    match error_site(error, context, output) {
        Some(site) => failure.at(site),
        None => failure,
    }
}

/// The file and byte range a config error is about.
fn config_span(error: &ConfigError) -> Option<(&AbsolutePath, Range<usize>)> {
    match error {
        ConfigError::Parse { path, error } => error.span().map(|span| (path, span)),
        ConfigError::ExtendsMissing { file, span, .. }
        | ConfigError::ExtendsDirectory { file, span, .. }
        | ConfigError::ExtendsNotConfig { file, span, .. }
        | ConfigError::NamespaceOutsideRoot { file, span, .. } => Some((file, span.clone())),
        ConfigError::ExtendsLoop { chain, span } => chain
            .len()
            .checked_sub(2)
            .and_then(|index| chain.get(index))
            .map(|file| (file, span.clone())),
        ConfigError::NotFound { .. }
        | ConfigError::Read { .. }
        | ConfigError::TooLarge { .. }
        | ConfigError::SymlinkNotConfig { .. } => None,
    }
}

/// Where `error` is, read back from the file so the message can show the line.
///
/// Source paths are relative to a root and cannot climb, so a file inside the current directory
/// is shown relative to it, and a file outside it becomes its own directory's only file: the
/// output root moves there, which keeps the text path correct relative to the current directory.
///
/// The file is read again through the same `max-config-size` cap as the load.
fn error_site(error: &ConfigError, context: &Context, output: &mut Output) -> Option<ErrorSite> {
    let cwd = &context.cwd;
    let (file, span) = config_span(error)?;
    let text = read_config_text(file, config_size_limit(&context.global.limits.0)).ok()?;
    let (root, relative) = match file.strip_prefix(cwd) {
        Some(inside) => (cwd.clone(), inside.to_path_buf()),
        None => (
            AbsolutePath::new(file.as_path().parent()?)?,
            PathBuf::from(file.as_path().file_name()?),
        ),
    };
    let location = Location::in_text(
        SourcePath::from_relative(&relative).ok()?,
        &text,
        span.clone(),
    )?;
    let snippet = Snippet::capture(&text, span)?;
    output.set_root(root);
    Some(ErrorSite { location, snippet })
}

/// `path` as typed, made absolute against the current directory.
pub(crate) fn absolute(cwd: &AbsolutePath, path: &Path) -> AbsolutePath {
    cwd.join_path(path)
}

/// The part of the project `[PATHS]` names: everything when none are given, or when one of them
/// is the project root itself.
///
/// Every path is checked before any of them can widen the scope to the whole project, so a bad
/// path is refused wherever it appears among the arguments.
///
/// # Errors
///
/// Returns `cli-path-missing` for a path that doesn't exist, `cli-path-outside-project` for one
/// that isn't inside the project root, and `cli-path-invalid` for one no source path can name
/// (all exit 2), and `cli-path-unreadable` (74) for one that can't be looked up for any other
/// reason, such as a parent directory it has no permission to search.
pub(crate) fn scope(
    paths: &[PathBuf],
    config: &Config,
    context: &Context,
) -> Result<Scope, CliError> {
    let (cwd, root) = (&context.cwd, config.root());
    let mut sources = Vec::with_capacity(paths.len());
    let mut names_root = false;
    for typed in paths {
        let path = absolute(cwd, typed);
        ensure_exists(&path, typed, cwd)?;
        let Some(inside) = path.strip_prefix(root) else {
            return Err(path_outside_config(typed, config));
        };
        if inside.as_os_str().is_empty() {
            names_root = true;
            continue;
        }
        let source =
            SourcePath::from_relative(inside).map_err(|error| path_invalid(typed, &error))?;
        sources.push(source);
    }
    Ok(if names_root || sources.is_empty() {
        Scope::All
    } else {
        Scope::Paths(sources)
    })
}

/// Where the project's `supdoc.lock` is, whether or not it exists yet.
pub(crate) fn lock_path(root: &AbsolutePath) -> AbsolutePath {
    root.join_path(Path::new(LOCK_FILE_NAME))
}

/// The lockfile in `config`'s root, or an empty one when there is none yet,
/// refusing one recorded under different hash settings than `config` resolves to.
///
/// Every command but `update` reads the lock through here, so a `passed` verdict always means the
/// target's bytes, read under the lock's recorded settings, hash to the recorded value. A help
/// names the lockfile relative to `cwd`, the directory supdoc runs in, so it runs as written.
///
/// # Errors
///
/// Returns the lockfile's own error (74 when it can't be read, 78 when its text is invalid), or
/// `lock-settings-mismatch` (78) when the recorded settings differ from the configured ones.
pub(crate) fn read_lock(config: &Config, cwd: &AbsolutePath) -> Result<Lockfile, CliError> {
    let read = read_lock_for_update(config, cwd)?;
    match read.change {
        None => Ok(read.lock),
        Some(change) => Err(settings_mismatch(config, change)),
    }
}

/// A lockfile read for `update`, with the hash settings the run must hash under and how they
/// differ from the ones the lockfile records.
///
/// The settings travel with the lock so the scan and the plan cannot each pick their own.
#[derive(Debug)]
pub(crate) struct LockForUpdate {
    /// The lockfile as read, or an empty one under `settings` when there is none.
    pub(crate) lock: Lockfile,
    /// The settings every hash in this run uses: the ones `config` resolves to.
    pub(crate) settings: HashSettings,
    /// How the recorded settings differ from `settings`, if they do.
    pub(crate) change: Option<SettingsChange>,
}

/// The lockfile in `config`'s root with the settings to hash under, and how the ones it records
/// differ from them, if they do.
///
/// A missing lockfile is an empty one under `config`'s settings, so a fresh project never
/// differs. A help names the lockfile relative to `cwd`, the directory supdoc runs in.
///
/// # Errors
///
/// Returns the lockfile's own error: 74 when it can't be read, 78 when its text is invalid.
pub(crate) fn read_lock_for_update(
    config: &Config,
    cwd: &AbsolutePath,
) -> Result<LockForUpdate, CliError> {
    let configured = config.hash_settings();
    let limit = config.limits_in_effect().max_lock_size.value;
    let read = Lockfile::read(&lock_path(config.root()), limit)
        .map_err(|error| lock_failure(&error, config.root(), cwd))?;
    let Some(lock) = read else {
        let mut empty = Lockfile::default();
        empty.set_hash_settings(configured);
        return Ok(LockForUpdate {
            lock: empty,
            settings: configured,
            change: None,
        });
    };
    let change = SettingsChange::between(lock.hash_settings(), configured);
    Ok(LockForUpdate {
        lock,
        settings: configured,
        change,
    })
}

/// The `lock-settings-mismatch` error (78) for a command that can't re-record the lockfile under
/// `config`'s hash settings, as `change` says they differ from the recorded ones.
///
/// The message names the config file that set the value relative to the project root.
pub(crate) fn settings_mismatch(config: &Config, change: SettingsChange) -> CliError {
    let mismatch = SettingsMismatch {
        recorded: change.from.line_endings,
        configured: change.to.line_endings,
        configured_in: config.line_endings_defined_in().cloned(),
    };
    CliError::new(
        SettingsMismatch::code(),
        mismatch.message_from(config.root()),
        mismatch.help(),
        SettingsMismatch::status(),
    )
}

/// The command's error for a lockfile that could not be read or written, naming it in the
/// message relative to the project `root`, as the read messages name files, and in the help
/// relative to `cwd`, so a command there runs as written.
pub(crate) fn lock_failure(error: &LockError, root: &AbsolutePath, cwd: &AbsolutePath) -> CliError {
    CliError::new(
        error.code(),
        error.message_from(root),
        error.help_from(cwd),
        error.status(),
    )
}

/// Succeeds when `path`, which the user typed as `typed`, can be looked up.
///
/// # Errors
///
/// Returns `cli-path-missing` (2) when nothing is there or a parent isn't a directory, and
/// `cli-path-unreadable` (74) when the lookup failed for any other reason, so a path supdoc
/// couldn't check is never reported as absent.
pub(crate) fn ensure_exists(
    path: &AbsolutePath,
    typed: &Path,
    cwd: &AbsolutePath,
) -> Result<(), CliError> {
    match path.as_path().symlink_metadata() {
        Ok(_) => Ok(()),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Err(path_missing(typed, cwd))
        }
        Err(error) => Err(path_unreadable(typed, &error)),
    }
}

/// The error for a `[PATH]` that couldn't be looked up, with the system's reason.
///
/// The help names permissions, not `ls`, because listing the parent fails for the same reason.
fn path_unreadable(typed: &Path, error: &std::io::Error) -> CliError {
    CliError::new(
        "cli-path-unreadable",
        format!("can't read {}: {}", typed.display(), cause(error)),
        format!(
            "Check the permissions on `{}` and on each directory above it",
            typed.display()
        ),
        ExitStatus::Io,
    )
}

/// The error for a `[PATH]` that doesn't exist.
///
/// The help lists the nearest directory of `typed` that exists under `cwd`, so the command it
/// names runs where this one did.
pub(crate) fn path_missing(typed: &Path, cwd: &AbsolutePath) -> CliError {
    CliError::new(
        "cli-path-missing",
        format!("{} does not exist", typed.display()),
        format!(
            "Check the path, which is relative to the current directory. `ls {}` lists what is \
             there",
            shell_quote(&listing_dir(typed, cwd).display().to_string())
        ),
        ExitStatus::Usage,
    )
}

/// The error for a namespaced `TARGET` whose file doesn't exist. Its path is relative to the
/// namespace's `directory`, not the current directory, so the help lists that directory as seen
/// from `cwd`.
pub(crate) fn namespaced_missing(
    typed: &Path,
    name: &NamespaceName,
    directory: &AbsolutePath,
    cwd: &AbsolutePath,
) -> CliError {
    let shown = match relative_to(cwd, directory) {
        empty if empty.is_empty() => ".".to_owned(),
        shown => shown,
    };
    CliError::new(
        "cli-path-missing",
        format!("{} does not exist", typed.display()),
        format!(
            "The path after `{name}:` is relative to the `{name}` namespace's directory. \
             `ls {}` lists what is there",
            shell_quote(&shown)
        ),
        ExitStatus::Usage,
    )
}

/// The nearest ancestor of a missing `typed` path that exists under `cwd`, as typed: `.` when
/// none of them does.
fn listing_dir<'a>(typed: &'a Path, cwd: &AbsolutePath) -> &'a Path {
    typed
        .ancestors()
        .skip(1)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .find(|ancestor| absolute(cwd, ancestor).as_path().is_dir())
        .unwrap_or_else(|| Path::new("."))
}

/// The error for a `[PATH]` outside the project, whose links no command can name.
pub(crate) fn path_outside(typed: &Path, root: &AbsolutePath) -> CliError {
    CliError::new(
        "cli-path-outside-project",
        format!(
            "{} is outside the project at {}",
            typed.display(),
            root.display()
        ),
        format!(
            "Pass a path inside `{}`, or run supdoc from the project that contains it",
            root.display()
        ),
        ExitStatus::Usage,
    )
}

/// The error for a `[PATH]` outside the project `config` names. When no config was found, the
/// "project" is only the current directory, so the error says that and offers `-c`.
pub(crate) fn path_outside_config(typed: &Path, config: &Config) -> CliError {
    if !config.files().is_empty() {
        return path_outside(typed, config.root());
    }
    CliError::new(
        "cli-path-outside-project",
        format!(
            "{} is outside the current directory, and no {} was found above it",
            typed.display(),
            crate::config::CONFIG_FILE_NAME
        ),
        format!(
            "Run supdoc from the project that contains it, or name its config with `supdoc -c \
             <dir>/{} …`",
            crate::config::CONFIG_FILE_NAME
        ),
        ExitStatus::Usage,
    )
}

/// The error for a `[PATH]` inside the project that no link source could have.
pub(crate) fn path_invalid(typed: &Path, error: &impl std::fmt::Display) -> CliError {
    CliError::new(
        "cli-path-invalid",
        format!("{} can't be checked: {error}", typed.display()),
        "Pass a file or directory whose path `supdoc.lock` could record, or leave out `[PATHS]` to cover the whole project",
        ExitStatus::Usage,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::Cli;
    use crate::output::{OutputSettings, PathStyle, Verbosity};
    use crate::scan::walk::GlobalExcludes;

    #[test]
    fn read_lock_refuses_mismatched_settings_with_78() {
        let dir = tempfile::tempdir().expect("a fresh temp directory should be created");
        fs::write(
            dir.path().join("supdoc.config.toml"),
            "[features.hash]\nline-endings = \"exact\"\n",
        )
        .expect("the fixture config should be written");
        fs::write(
            dir.path().join("supdoc.lock"),
            "version = 1\n\n[features.hash]\nline-endings = \"normalize\"\n",
        )
        .expect("the fixture lock should be written");
        let root = AbsolutePath::new(dir.path()).expect("a temp directory should be absolute");
        let config = Config::load(&root).expect("the fixture config should load");

        let error =
            read_lock(&config, &root).expect_err("a lock under other settings should be refused");

        assert_eq!(
            (error.code(), error.exit_code()),
            ("lock-settings-mismatch", 78)
        );
    }

    /// `load_config` for `supdoc list` run in `cwd`, with plain text output.
    fn load_in(cwd: &Path) -> Result<Config, CliError> {
        let cwd = AbsolutePath::new(cwd).expect("a temp directory should be absolute");
        let cli = Cli::try_parse_from(["supdoc", "list"]).expect("`supdoc list` should parse");
        let settings = OutputSettings {
            format: cli.global.format(),
            verbosity: Verbosity::from_flags(cli.global.quiet, cli.global.verbose),
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
        };
        let mut output = Output::stdio(settings, anstream::ColorChoice::Never);
        let context = Context {
            global: cli.global,
            cwd,
            global_excludes: GlobalExcludes::none(),
        };
        load_config(&context, &mut output)
    }

    /// A project with an empty config and a `docs` directory to run from.
    fn project_with_docs() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a fresh temp directory should be created");
        fs::create_dir(dir.path().join("docs")).expect("the docs directory should be created");
        fs::write(dir.path().join("supdoc.config.toml"), "")
            .expect("the fixture config should be written");
        dir
    }

    /// The `docs` directory of a project from [`project_with_docs`].
    fn docs_of(dir: &tempfile::TempDir) -> AbsolutePath {
        AbsolutePath::new(&dir.path().join("docs")).expect("a temp directory should be absolute")
    }

    #[test]
    fn config_errors_name_the_config_from_the_current_directory() {
        let dir = project_with_docs();
        fs::write(dir.path().join("supdoc.config.toml"), "include = oops\n")
            .expect("the fixture config should be written");

        let error = load_in(&dir.path().join("docs")).expect_err("a broken config should not load");

        assert_eq!(
            error.message(),
            "../supdoc.config.toml: string values must be quoted, expected literal string",
            "the message should name the config as the location does, not absolutely"
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_symlink_help_runs_from_the_current_directory() {
        let dir = project_with_docs();
        fs::remove_file(dir.path().join("supdoc.config.toml"))
            .expect("the fixture config should be replaced");
        fs::write(dir.path().join("real.toml"), "").expect("the symlink target should be written");
        std::os::unix::fs::symlink("real.toml", dir.path().join("supdoc.config.toml"))
            .expect("the config symlink should be created");

        let error = load_in(&dir.path().join("docs"))
            .expect_err("a config symlinked to another name should be refused");

        assert_eq!(
            (error.code(), error.message()),
            (
                "config-symlink-not-config",
                "../supdoc.config.toml is a symlink that doesn't lead to a supdoc.config.toml file"
            )
        );
        assert!(
            error
                .help()
                .contains("`rm ../supdoc.config.toml && supdoc -c ../supdoc.config.toml init`"),
            "the help's command should run from the current directory: {}",
            error.help()
        );
    }

    #[test]
    fn lock_errors_name_the_lockfile_from_the_root() {
        let dir = project_with_docs();
        fs::write(dir.path().join("supdoc.lock"), "version = x\n")
            .expect("the fixture lock should be written");
        let docs = docs_of(&dir);
        let config = load_in(docs.as_path()).expect("the fixture config should load");

        let error = read_lock(&config, &docs).expect_err("a broken lock should be refused");

        assert_eq!(
            (error.code(), error.message()),
            (
                "lock-parse",
                "supdoc.lock: string values must be quoted, expected literal string (line 1)"
            ),
            "the message should name the lockfile relative to the root, not absolutely"
        );
    }

    #[test]
    fn settings_mismatch_names_the_config_from_the_root() {
        let dir = project_with_docs();
        fs::write(
            dir.path().join("supdoc.config.toml"),
            "[features.hash]\nline-endings = \"exact\"\n",
        )
        .expect("the fixture config should be written");
        fs::write(
            dir.path().join("supdoc.lock"),
            "version = 1\n\n[features.hash]\nline-endings = \"normalize\"\n",
        )
        .expect("the fixture lock should be written");
        let docs = docs_of(&dir);
        let config = load_in(docs.as_path()).expect("the fixture config should load");

        let error =
            read_lock(&config, &docs).expect_err("a lock under other settings should be refused");

        assert_eq!(
            error.message(),
            "supdoc.lock was written with line-endings = \"normalize\", but supdoc.config.toml \
             sets \"exact\""
        );
    }

    #[test]
    fn a_wrong_type_help_names_the_key_and_its_form() {
        let dir = tempfile::tempdir().expect("a fresh temp directory should be created");
        fs::write(dir.path().join("supdoc.config.toml"), "include = 3\n")
            .expect("the fixture config should be written");

        let error = load_in(dir.path()).expect_err("a config with a wrong type should not load");

        assert_eq!(
            (error.code(), error.exit_code()),
            ("config-parse", ExitStatus::Config.code())
        );
        assert!(
            error.help().contains("`include = [\"docs\"]`"),
            "a wrong type should get a help that names the key and its form: {}",
            error.help()
        );
    }

    #[test]
    fn a_toml_syntax_error_carries_its_file_and_line() {
        let dir = tempfile::tempdir().expect("a fresh temp directory should be created");
        fs::write(
            dir.path().join("supdoc.config.toml"),
            "include = [\"docs\"]\nexclude = oops\n",
        )
        .expect("the fixture config should be written");

        let error = load_in(dir.path()).expect_err("a config with a syntax error should not load");

        let site = error.site().expect("a parse error should carry its site");
        assert_eq!(
            (
                site.location.file.as_str(),
                site.location.line,
                site.location.column
            ),
            ("supdoc.config.toml", 2, 11),
            "the site should point at the bad value"
        );
        assert!(
            site.snippet.text.contains("exclude = oops"),
            "the snippet should hold the offending line: {}",
            site.snippet.text
        );
    }

    /// `scope` for `paths` typed in the root of the project from [`project_with_docs`].
    fn scope_in(dir: &tempfile::TempDir, paths: &[&Path]) -> Result<Scope, CliError> {
        let cwd = AbsolutePath::new(dir.path()).expect("a temp directory should be absolute");
        let config = Config::load(&cwd).expect("the fixture config should load");
        let cli = Cli::try_parse_from(["supdoc", "check"]).expect("`supdoc check` should parse");
        let context = Context {
            global: cli.global,
            cwd,
            global_excludes: GlobalExcludes::none(),
        };
        let paths: Vec<PathBuf> = paths.iter().map(|path| path.to_path_buf()).collect();
        scope(&paths, &config, &context)
    }

    #[test]
    fn a_missing_path_is_refused_on_either_side_of_the_root() {
        let dir = project_with_docs();
        let (root, missing) = (Path::new("."), Path::new("nope.md"));

        for paths in [[root, missing], [missing, root]] {
            let error = scope_in(&dir, &paths)
                .expect_err("a missing path should be refused even beside the root");

            assert_eq!(
                (error.code(), error.exit_code()),
                ("cli-path-missing", 2),
                "paths {paths:?} should be refused for the missing one"
            );
        }
    }

    #[test]
    fn an_outside_path_is_refused_on_either_side_of_the_root() {
        let dir = project_with_docs();
        let elsewhere = tempfile::tempdir().expect("a fresh temp directory should be created");
        let outside = elsewhere.path().join("passwd");
        fs::write(&outside, "").expect("the outside file should be written");
        let root = Path::new(".");

        for paths in [[root, outside.as_path()], [outside.as_path(), root]] {
            let error = scope_in(&dir, &paths)
                .expect_err("a path outside the project should be refused even beside the root");

            assert_eq!(
                (error.code(), error.exit_code()),
                ("cli-path-outside-project", 2),
                "paths {paths:?} should be refused for the outside one"
            );
        }
    }

    #[test]
    fn the_root_beside_valid_paths_scopes_the_whole_project() {
        let dir = project_with_docs();
        let (root, docs) = (Path::new("."), Path::new("docs"));

        for paths in [[root, docs], [docs, root]] {
            let scope = scope_in(&dir, &paths).expect("valid paths should be accepted");

            assert_eq!(
                scope,
                Scope::All,
                "paths {paths:?} name the root, so all of it"
            );
        }
    }
}
