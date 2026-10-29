//! The setup every config-loading command shares: load the config, root the output at it, narrow
//! the scope to `[PATHS]`, and read the lockfile.
//!
//! [`load_config`] sets the output root itself, so no command can load a config and then report
//! paths against the wrong directory (X4).

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::config::{Config, ConfigError};
use crate::finding::{Location, Snippet};
use crate::link::SourcePath;
use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
use crate::output::Output;
use crate::path::AbsolutePath;
use crate::scan::model::Scope;

use super::{CliError, Context, ErrorSite};

/// The config `--config` names, or the one found by searching upward from the current directory.
///
/// On success, `output` reports paths against the config's root from then on. `command` names the
/// command in the fallback help, as in ``run `supdoc list` again``.
///
/// # Errors
///
/// Returns the config's own error, with its code, help and exit code.
pub(crate) fn load_config(
    context: &Context,
    output: &mut Output,
    command: &str,
) -> Result<Config, CliError> {
    let loaded = match &context.global.config {
        Some(file) => Config::load_file(&absolute(&context.cwd, file)),
        None => Config::load(&context.cwd),
    };
    let config = loaded.map_err(|error| config_failure(&error, context, output, command))?;
    output.set_root(config.root().clone());
    Ok(config)
}

/// The fatal error for a config that did not load, pointing at the offending text when the config
/// error knows where it is.
fn config_failure(
    error: &ConfigError,
    context: &Context,
    output: &mut Output,
    command: &str,
) -> CliError {
    let help = error.help().unwrap_or_else(|| {
        format!("Correct the config as the message says, then run `supdoc {command}` again")
    });
    let failure = CliError::new(error.code(), error.to_string(), help, error.exit_code());
    match error_site(error, &context.cwd, output) {
        Some(site) => failure.at(site),
        None => failure,
    }
}

/// The file and byte range a config error is about.
fn config_span(error: &ConfigError) -> Option<(&AbsolutePath, Range<usize>)> {
    match error {
        ConfigError::Parse { path, error } => error.span().map(|span| (path, span)),
        ConfigError::ExtendsMissing { file, span, .. }
        | ConfigError::ExtendsDirectory { file, span, .. } => Some((file, span.clone())),
        ConfigError::ExtendsLoop { chain, span } => chain
            .len()
            .checked_sub(2)
            .and_then(|index| chain.get(index))
            .map(|file| (file, span.clone())),
        ConfigError::NotFound { .. } | ConfigError::Read { .. } => None,
    }
}

/// Where `error` is, read back from the file so the message can show the line.
///
/// Source paths are relative to a root and cannot climb, so a file inside the current directory
/// is shown relative to it, and a file outside it becomes its own directory's only file: the
/// output root moves there, which keeps the text path correct relative to the current directory.
fn error_site(error: &ConfigError, cwd: &AbsolutePath, output: &mut Output) -> Option<ErrorSite> {
    let (file, span) = config_span(error)?;
    let text = fs::read_to_string(file.as_path()).ok()?;
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
    // Joining onto an absolute directory always gives an absolute path.
    AbsolutePath::new(&cwd.as_path().join(path)).unwrap_or_else(|| cwd.clone())
}

/// The part of the project `[PATHS]` names: everything when none are given, or when one of them
/// is the project root itself (04 S10).
///
/// # Errors
///
/// Returns `cli/path-missing` for a path that doesn't exist, `cli/path-outside-project` for one
/// that isn't inside the project root, and `cli/path-invalid` for one no source path can name.
/// All exit 2.
pub(crate) fn scope(
    paths: &[PathBuf],
    cwd: &AbsolutePath,
    root: &AbsolutePath,
) -> Result<Scope, CliError> {
    let mut sources = Vec::with_capacity(paths.len());
    for typed in paths {
        let path = absolute(cwd, typed);
        if path.as_path().symlink_metadata().is_err() {
            return Err(path_missing(typed));
        }
        let Some(inside) = path.strip_prefix(root) else {
            return Err(path_outside(typed, root));
        };
        if inside.as_os_str().is_empty() {
            return Ok(Scope::All);
        }
        let source =
            SourcePath::from_relative(inside).map_err(|error| path_invalid(typed, &error))?;
        sources.push(source);
    }
    Ok(if sources.is_empty() {
        Scope::All
    } else {
        Scope::Paths(sources)
    })
}

/// Where the project's `supdoc.lock` is, whether or not it exists yet.
pub(crate) fn lock_path(root: &AbsolutePath) -> AbsolutePath {
    AbsolutePath::new(&root.as_path().join(LOCK_FILE_NAME)).unwrap_or_else(|| {
        unreachable!("the lockfile name joined onto an absolute root is absolute")
    })
}

/// The lockfile in the project root, or an empty one when there is none yet (06 "What each
/// command touches").
///
/// # Errors
///
/// Returns the lockfile's own error: 74 when it can't be read, 78 when its text is invalid.
pub(crate) fn read_lock(root: &AbsolutePath) -> Result<Lockfile, CliError> {
    Lockfile::read(&lock_path(root))
        .map(Option::unwrap_or_default)
        .map_err(|error| {
            CliError::new(
                error.code(),
                error.to_string(),
                error.help(),
                error.exit_code(),
            )
        })
}

/// The error for a `[PATH]` that doesn't exist (06 error table).
pub(crate) fn path_missing(typed: &Path) -> CliError {
    CliError::new(
        "cli/path-missing",
        format!("{} does not exist", typed.display()),
        "Check the path. Paths are relative to the current directory",
        CliError::USAGE_EXIT_CODE,
    )
}

/// The error for a `[PATH]` outside the project, whose links no command can name.
pub(crate) fn path_outside(typed: &Path, root: &AbsolutePath) -> CliError {
    CliError::new(
        "cli/path-outside-project",
        format!(
            "{} is outside the project at {}",
            typed.display(),
            root.display()
        ),
        format!(
            "Pass a path inside {}, or run supdoc from the project that contains it",
            root.display()
        ),
        CliError::USAGE_EXIT_CODE,
    )
}

/// The error for a `[PATH]` inside the project that no link source could have.
pub(crate) fn path_invalid(typed: &Path, error: &impl std::fmt::Display) -> CliError {
    CliError::new(
        "cli/path-invalid",
        format!("{} can't be checked: {error}", typed.display()),
        "Pass a file or directory whose path supdoc.lock could record, or leave out [PATHS] to check the whole project",
        CliError::USAGE_EXIT_CODE,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::Cli;
    use crate::output::{OutputSettings, PathStyle, Verbosity};

    #[test]
    fn config_error_help_names_the_command_passed_in() {
        let dir = tempfile::tempdir().expect("a fresh temp directory should be created");
        // A wrong type has no specific hint, so the help falls back to rerunning the command.
        fs::write(dir.path().join("supdoc.config.toml"), "include = 3\n")
            .expect("the fixture config should be written");
        let cwd = AbsolutePath::new(dir.path()).expect("a temp directory should be absolute");
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
        };

        let error = load_config(&context, &mut output, "list")
            .expect_err("a config with a wrong type should not load");

        assert_eq!(
            (error.code(), error.exit_code()),
            ("config/parse", CliError::CONFIG_EXIT_CODE)
        );
        assert!(
            error.help().contains("run `supdoc list` again"),
            "the fallback help should name the command that was run: {}",
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
        let cwd = AbsolutePath::new(dir.path()).expect("a temp directory should be absolute");
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
        };

        let error = load_config(&context, &mut output, "list")
            .expect_err("a config with a syntax error should not load");

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
}
