//! `supdoc trust [PATH]` and `supdoc trust --revoke [PATH]`: lists or removes a checkout in the
//! trust store.
//!
//! `PATH` is an existing directory (any path, for `--revoke`), or the project root when omitted: the
//! directory of the config `check` would load. With no config there is nothing to trust. The trust
//! store reads only `XDG_STATE_HOME` and `HOME` (on Windows, `LOCALAPPDATA`; `XDG_STATE_HOME` still wins when set); `SUPDOC_TRUST` is not consulted, since it grants
//! trust to a run and never records it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::discover_config;
use crate::exit::ExitStatus;
use crate::output::{Output, Report, TextContext};
use crate::path::AbsolutePath;
use crate::trust::{self, AddError};

use super::args::{OperatorPath, TrustArgs};
use super::project::{absolute, config_failure, load_config_held};
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

    /// One line: `trusted .`, `already trusted .`, `revoked .` or `was not trusted .`, with the
    /// root shown as `.` when it is the current directory and as its absolute path otherwise, so
    /// the operator sees exactly which directory the store names.
    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let said = match self.action {
            TrustAction::Trusted => "trusted",
            TrustAction::AlreadyTrusted => "already trusted",
            TrustAction::Revoked => "revoked",
            TrustAction::NotTrusted => "was not trusted",
        };
        let relative = context.paths.text_absolute(&self.root);
        if relative == "." {
            writeln!(out, "{said} .")
        } else {
            writeln!(out, "{said} {}", self.root.as_path().display())
        }
    }
}

/// The JSON Schema of `supdoc trust`'s `--json` output: the whole envelope, with `data` typed as
/// [`TrustReport`].
#[must_use]
pub(crate) fn schema() -> schemars::Schema {
    schemars::schema_for!(crate::output::json::Envelope<'static, TrustReport>)
}

/// Runs `supdoc trust`, listing the root in the store of `context`'s trust variables, or with
/// `--revoke` removing it.
/// `SUPDOC_TRUST` is never consulted.
///
/// With no `PATH` the root is the directory of the config `check` would load (`-c` is validated as
/// `check` validates it). `--revoke PATH` takes the path as typed, made absolute by text, so a
/// root that was deleted or became a symlink can still be revoked; the line names the entry removed.
///
/// # Errors
///
/// Returns `cli-trust-path` when a `PATH` to trust isn't an existing directory or its real path
/// holds a newline, `cli-trust-broad-root` when the root to trust is `/`, `HOME` or a directory
/// above `HOME` (2), `cli-trust-no-config` when no `PATH` is given and no config is found (78),
/// `cli-trust-no-state` when neither `XDG_STATE_HOME` nor `HOME` (on Windows, `LOCALAPPDATA`) is an absolute path (78),
/// `cli-trust-store` when the store can't be read or written (74), the config error when `-c` or
/// the config search fails, and `cli-output-failed` when the report cannot be written.
pub(crate) fn run(
    args: &TrustArgs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let root = match &args.path {
        Some(path) => explicit_root(path, args.revoke, context)?,
        None => project_root(context, output)?,
    };
    let store = context.trust.store().ok_or_else(no_state)?;
    let (action, shown) = if args.revoke {
        match store
            .revoke(root.as_path())
            .map_err(|error| store_failed(&error))?
        {
            Some(removed) => (TrustAction::Revoked, removed),
            None => (TrustAction::NotTrusted, root.as_path().to_path_buf()),
        }
    } else {
        let added = store.add(root.as_path()).map_err(|error| match error {
            AddError::Broad(real) => broad_root(&real),
            AddError::Io(error) => store_failed(&error),
        })?;
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
    let full = absolute(&context.cwd, OperatorPath::typed(path));
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
        return Ok(load_config_held(context, output)?.root().clone());
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

/// The error when the root to trust, by its real path `real`, is too broad: `/`, `HOME` or a
/// directory above it, so its trust would cover every directory under it with no config.
fn broad_root(real: &Path) -> CliError {
    CliError::new(
        "cli-trust-broad-root",
        format!(
            "`{}` is too broad to trust: it is `/`, your home directory or a directory above it, \
             so trust would cover every directory under it that has no supdoc.config.toml",
            real.display()
        ),
        "Trust the project's own directory, or add a supdoc.config.toml there and run `supdoc trust` inside it",
        ExitStatus::Usage,
    )
}

/// The error when there is nowhere to keep the trust store.
fn no_state() -> CliError {
    CliError::new(
        "cli-trust-no-state",
        "there is no state directory to keep trusted roots in: neither XDG_STATE_HOME nor HOME (on Windows, LOCALAPPDATA) is an absolute path",
        "Set HOME, for example `export HOME=/home/you` (on Windows, LOCALAPPDATA), or XDG_STATE_HOME to an absolute directory",
        ExitStatus::Config,
    )
}

/// The error when the trust store can't be read or written. When it was refused for a reason
/// the operator can fix ([`trust::unread_of`]), its help is that reason's fix.
fn store_failed(error: &io::Error) -> CliError {
    if let Some(unread) = trust::unread_of(error) {
        return CliError::new(
            "cli-trust-store",
            format!("cannot update the trust store: {}", cause(error)),
            format!(
                "{}, then run `supdoc trust` again",
                sentence_case(&unread.fix())
            ),
            ExitStatus::Io,
        );
    }
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
        "Check that you can create and write files in `$XDG_STATE_HOME/supdoc` (under `$HOME/.local/state` when it is unset; on Windows, under `%LOCALAPPDATA%\\supdoc`)",
        ExitStatus::Io,
    )
}

/// `clause` with its first character uppercased, to open a help sentence.
fn sentence_case(clause: &str) -> String {
    let mut chars = clause.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use super::{AddError, store_failed};
    use crate::cli::testing::symlink;
    use crate::trust::TrustEnv;

    /// The help of `supdoc trust`'s refusal when adding `root` to the store under `state`, whose
    /// `supdoc` directory `arrange` sets up first.
    fn add_refusal_help(state: &Path, arrange: impl FnOnce(&Path)) -> String {
        let root = state.join("root");
        fs::create_dir_all(&root).expect("the root should be creatable");
        let dir = state.join("state").join("supdoc");
        fs::create_dir_all(&dir).expect("the store's directory should be creatable");
        arrange(&dir);
        let env = TrustEnv::from_vars(|name| {
            (name == "XDG_STATE_HOME").then(|| OsString::from(state.join("state")))
        });
        let store = env
            .store()
            .expect("an absolute XDG_STATE_HOME should locate the store");

        let Err(AddError::Io(error)) = store.add(&root) else {
            panic!("adding to a refused store should fail with an I/O refusal");
        };
        let refusal = store_failed(&error);
        assert_eq!(refusal.code(), "cli-trust-store");
        refusal.help().to_owned()
    }

    /// A fresh scratch directory, by its real path.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let real = dir
            .path()
            .canonicalize()
            .expect("the scratch directory should resolve");
        (dir, real)
    }

    /// The help for a store file that isn't a regular file in its own directory.
    const NOT_A_FILE_HELP: &str = "Make `supdoc/trusted-roots` under `XDG_STATE_HOME` a regular \
                                   file in that directory, not a symlink out of it, then run \
                                   `supdoc trust` again";

    #[test]
    fn a_store_refused_for_dot_dot_says_to_set_the_variable_without_it() {
        let (_dir, scratch) = scratch();
        let root = scratch.join("root");
        fs::create_dir_all(scratch.join("detour")).expect("the detour should be creatable");
        fs::create_dir(&root).expect("the root should be creatable");
        let climbing = scratch.join("detour").join("..").join("state");
        let env = TrustEnv::from_vars(|name| {
            (name == "XDG_STATE_HOME").then(|| OsString::from(climbing.clone()))
        });
        let store = env
            .store()
            .expect("an absolute XDG_STATE_HOME should locate the store");

        let Err(AddError::Io(error)) = store.add(&root) else {
            panic!("adding through `..` should fail with an I/O refusal");
        };
        let refusal = store_failed(&error);

        assert_eq!(refusal.code(), "cli-trust-store");
        assert_eq!(
            refusal.help(),
            "Set `XDG_STATE_HOME` to a path without `..`, then run `supdoc trust` again"
        );
    }

    #[test]
    fn a_store_file_symlinked_outside_says_to_make_it_a_regular_file() {
        let (_dir, scratch) = scratch();
        let outside = scratch.join("elsewhere");
        fs::write(&outside, "").expect("the outside file should be writable");

        let help = add_refusal_help(&scratch, |dir| {
            symlink(&outside, dir.join("trusted-roots")).expect("the link should be creatable");
        });

        assert_eq!(help, NOT_A_FILE_HELP);
    }

    #[test]
    fn a_store_file_that_is_a_directory_says_to_make_it_a_regular_file() {
        let (_dir, scratch) = scratch();

        let help = add_refusal_help(&scratch, |dir| {
            fs::create_dir(dir.join("trusted-roots")).expect("the directory should be creatable");
        });

        assert_eq!(help, NOT_A_FILE_HELP);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_store_file_says_to_make_it_readable() {
        let (_dir, scratch) = scratch();

        let help = add_refusal_help(&scratch, |dir| {
            let file = dir.join("trusted-roots");
            fs::write(&file, "").expect("the store should be writable");
            fs::set_permissions(&file, fs::Permissions::from_mode(0o000))
                .expect("the store's mode should be settable");
        });

        assert_eq!(
            help,
            "Make `supdoc/trusted-roots` under `XDG_STATE_HOME` readable by you, then run \
             `supdoc trust` again"
        );
    }
}
