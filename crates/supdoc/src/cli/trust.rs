//! `supdoc trust [PATH]` and `supdoc trust --revoke [PATH]`: lists or removes a checkout in the
//! trust store.
//!
//! `PATH` is an existing directory, or the project root when omitted: the directory of the config
//! found searching upward from the current directory (or of `--config`), else the top of the
//! current directory's work tree, else the current directory. Only `XDG_STATE_HOME` and `HOME`
//! are read; `SUPDOC_TRUST` is not consulted, since it grants trust to a run and never records it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::discover_config;
use crate::exit::ExitStatus;
use crate::output::{Output, Report, TextContext};
use crate::path::{AbsolutePath, holds_git};
use crate::trust::{Store, TrustEnv};

use super::args::TrustArgs;
use super::project::{absolute, config_failure};
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

/// Runs `supdoc trust`, listing the root, or with `--revoke` removing it.
///
/// # Errors
///
/// Returns `cli-trust-path` when `PATH` isn't an existing directory, `cli-trust-no-state` when
/// neither `XDG_STATE_HOME` nor `HOME` is an absolute path (78), `cli-trust-store` when the store
/// can't be read or written (74), the config error when the project root search fails, and
/// `cli-output-failed` when the report cannot be written.
pub(crate) fn run(
    args: &TrustArgs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let root = match &args.path {
        Some(path) => explicit_root(path, context)?,
        None => project_root(context, output)?,
    };
    let store = TrustEnv::from_vars(|name| std::env::var_os(name))
        .store()
        .ok_or_else(no_state)?;
    let action = apply(&store, &root, args.revoke).map_err(|error| store_failed(&error))?;
    let shown = AbsolutePath::new(&real_or(&root)).unwrap_or(root);
    let report = TrustReport {
        action,
        root: shown,
    };
    output
        .report(&report, true)
        .map_err(|error| CliError::output_failed("the trust report", &error))?;
    Ok(Outcome::Clean)
}

/// Lists or removes `root`, and says what changed.
///
/// Effect: reads and rewrites the trust store.
///
/// # Errors
///
/// Fails when the store can't be read, written, or `root` can't be listed.
fn apply(store: &Store, root: &AbsolutePath, revoke: bool) -> io::Result<TrustAction> {
    let path = root.as_path();
    Ok(if revoke {
        if store.revoke(path)? {
            TrustAction::Revoked
        } else {
            TrustAction::NotTrusted
        }
    } else if store.add(path)? {
        TrustAction::Trusted
    } else {
        TrustAction::AlreadyTrusted
    })
}

/// `path`'s real path, or `path` itself when it can't be resolved.
fn real_or(path: &AbsolutePath) -> PathBuf {
    path.as_path()
        .canonicalize()
        .unwrap_or_else(|_| path.as_path().to_path_buf())
}

/// The directory `PATH` names, which must exist. The store lists its real path.
///
/// # Errors
///
/// Returns `cli-trust-path` when `path` isn't an existing directory.
fn explicit_root(path: &Path, context: &Context) -> Result<AbsolutePath, CliError> {
    let full = absolute(&context.cwd, path);
    if full.as_path().is_dir() {
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

/// The project root: the directory of the config `check` would load, else the top of the current
/// directory's work tree, else the current directory.
///
/// # Errors
///
/// Returns the config error when the search for the config fails.
fn project_root(context: &Context, output: &mut Output) -> Result<AbsolutePath, CliError> {
    let found = match &context.global.config {
        Some(file) => Some(absolute(&context.cwd, file)),
        None => discover_config(&context.cwd)
            .map_err(|error| config_failure(&error, context, output))?,
    };
    if let Some(parent) = found.and_then(|file| file.parent()) {
        return Ok(parent);
    }
    let cwd = real_or(&context.cwd);
    let top = cwd.ancestors().find(|dir| holds_git(dir)).unwrap_or(&cwd);
    Ok(AbsolutePath::new(top).unwrap_or_else(|| context.cwd.clone()))
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
    CliError::new(
        "cli-trust-store",
        format!("cannot update the trust store: {}", cause(error)),
        "Check that you can create and write files in `$XDG_STATE_HOME/supdoc` (under `$HOME/.local/state` when it is unset)",
        ExitStatus::Io,
    )
}
