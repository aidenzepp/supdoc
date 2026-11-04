//! `supdoc trust [PATH]` and `supdoc trust --revoke [PATH]`: lists or removes a checkout in the
//! trust store.
//!
//! `PATH` is an existing directory (any path, for `--revoke`), or the project root when omitted: the
//! directory of the config `check` would load. With no config there is nothing to trust. The trust
//! store reads only `XDG_STATE_HOME` and `HOME`; `SUPDOC_TRUST` is not consulted, since it grants
//! trust to a run and never records it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::discover_config;
use crate::exit::ExitStatus;
use crate::output::{Output, Report, TextContext};
use crate::path::AbsolutePath;
use crate::trust::TrustEnv;

use super::args::TrustArgs;
use super::project::{absolute, config_failure, load_config};
use super::{CliError, Context, Outcome, cause};

/// What `supdoc trust` did to the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum TrustAction {
    /// The root was added to the trust store.
    Trusted,
    /// The root was already listed.
    AlreadyTrusted,
    /// The root was removed from the trust store.
    Revoked,
    /// `--revoke` named a root that wasn't listed.
    NotTrusted,
}

/// `supdoc trust`: the root and what happened to it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct TrustReport {
    /// What happened.
    action: TrustAction,
    /// The root, by its real path.
    root: AbsolutePath,
}

impl Report for TrustReport {
    fn command(&self) -> &'static str {
        "trust"
    }

    /// One line: `trusted .`, `already trusted .`, `revoked .` or `was not trusted .`.
    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let said = match self.action {
            TrustAction::Trusted => "trusted",
            TrustAction::AlreadyTrusted => "already trusted",
            TrustAction::Revoked => "revoked",
            TrustAction::NotTrusted => "was not trusted",
        };
        writeln!(out, "{said} {}", context.paths.text_absolute(&self.root))
    }
}

/// The JSON Schema of `supdoc trust`'s `--json` output: the whole envelope, with `data` typed as
/// [`TrustReport`].
#[must_use]
pub(crate) fn schema() -> schemars::Schema {
    schemars::schema_for!(crate::output::json::Envelope<'static, TrustReport>)
}

/// Runs `supdoc trust`, listing the root in `trust`'s store, or with `--revoke` removing it.
/// `SUPDOC_TRUST` is never consulted.
///
/// With no `PATH` the root is the directory of the config `check` would load (`-c` is validated as
/// `check` validates it). `--revoke PATH` takes the path as typed, made absolute by text, so a
/// root that was deleted or became a symlink can still be revoked; the line names the entry removed.
///
/// # Errors
///
/// Returns `cli-trust-path` when a `PATH` to trust isn't an existing directory or its real path
/// holds a newline, `cli-trust-no-config` when no `PATH` is given and no config is found (78),
/// `cli-trust-no-state` when neither `XDG_STATE_HOME` nor `HOME` is an absolute path (78),
/// `cli-trust-store` when the store can't be read or written (74), the config error when `-c` or
/// the config search fails, and `cli-output-failed` when the report cannot be written.
pub(crate) fn run(
    args: &TrustArgs,
    trust: &TrustEnv,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let root = match &args.path {
        Some(path) => explicit_root(path, args.revoke, context)?,
        None => project_root(context, output)?,
    };
    let store = trust.store().ok_or_else(no_state)?;
    let (action, shown) = if args.revoke {
        match store
            .revoke(root.as_path())
            .map_err(|error| store_failed(&error))?
        {
            Some(removed) => (TrustAction::Revoked, removed),
            None => (TrustAction::NotTrusted, root.as_path().to_path_buf()),
        }
    } else {
        let added = store
            .add(root.as_path())
            .map_err(|error| store_failed(&error))?;
        let action = if added {
            TrustAction::Trusted
        } else {
            TrustAction::AlreadyTrusted
        };
        (action, real_or(&root))
    };
    let report = TrustReport {
        action,
        root: AbsolutePath::new(&shown).unwrap_or(root),
    };
    output
        .report(&report, true)
        .map_err(|error| CliError::output_failed("the trust report", &error))?;
    Ok(Outcome::Clean)
}

/// `path`'s real path, or `path` itself when it can't be resolved.
fn real_or(path: &AbsolutePath) -> PathBuf {
    path.as_path()
        .canonicalize()
        .unwrap_or_else(|_| path.as_path().to_path_buf())
}

/// The directory `PATH` names, made absolute by text. To trust it, it must exist; to revoke it,
/// it needn't.
///
/// # Errors
///
/// Returns `cli-trust-path` when `path` is to be trusted and isn't an existing directory.
fn explicit_root(path: &Path, revoke: bool, context: &Context) -> Result<AbsolutePath, CliError> {
    let full = absolute(&context.cwd, path);
    if revoke || full.as_path().is_dir() {
        Ok(full)
    } else {
        Err(CliError::new(
            "cli-trust-path",
            format!("`{}` is not an existing directory", path.display()),
            "Name the project's directory, or run `supdoc trust` inside it to trust its root",
            ExitStatus::Usage,
        ))
    }
}

/// The project root: the directory of the config `check` would load.
///
/// # Errors
///
/// Returns the config error when `-c` doesn't load or the search for the config fails, and
/// `cli-trust-no-config` when there is no config.
fn project_root(context: &Context, output: &mut Output) -> Result<AbsolutePath, CliError> {
    if context.global.config.is_some() {
        return Ok(load_config(context, output)?.root().clone());
    }
    discover_config(&context.cwd)
        .map_err(|error| config_failure(&error, context, output))?
        .and_then(|file| file.parent())
        .ok_or_else(no_config)
}

/// The error when there is no config to take the root from.
fn no_config() -> CliError {
    CliError::new(
        "cli-trust-no-config",
        "no supdoc.config.toml was found, so there is no project root to trust",
        "Run `supdoc trust` in a project with a supdoc.config.toml, or name the directory as PATH",
        ExitStatus::Config,
    )
}

/// The error when there is nowhere to keep the trust store.
fn no_state() -> CliError {
    CliError::new(
        "cli-trust-no-state",
        "there is no state directory to keep trusted roots in: neither XDG_STATE_HOME nor HOME is an absolute path",
        "Set HOME, for example `export HOME=/home/you`, or XDG_STATE_HOME to an absolute directory",
        ExitStatus::Config,
    )
}

/// The error when the trust store can't be read or written.
fn store_failed(error: &io::Error) -> CliError {
    if error.kind() == io::ErrorKind::InvalidInput {
        return CliError::new(
            "cli-trust-path",
            "the path's real location holds a newline, which the trust store can't list",
            "Rename the directory so its path has no newline, then run `supdoc trust` again",
            ExitStatus::Usage,
        );
    }
    CliError::new(
        "cli-trust-store",
        format!("cannot update the trust store: {}", cause(error)),
        "Check that you can create and write files in `$XDG_STATE_HOME/supdoc` (under `$HOME/.local/state` when it is unset)",
        ExitStatus::Io,
    )
}
