//! The command-line interface: arguments, parsing and command dispatch.
//!
//! [`run`] holds the one dispatch `match`. Each command lives in `cli/<command>.rs` and exposes a
//! `run` function, so building a command means editing only its own file.

/// Command-line argument definitions.
pub(crate) mod args;

/// Example invocations shown in help.
pub(crate) mod examples;

/// The long help each command shows under `--help`.
pub(crate) mod help;

/// Parsing arguments, including JSON usage errors.
pub(crate) mod parse;

/// Setup shared by every command that loads the config.
pub(crate) mod project;

/// `supdoc init`.
pub(crate) mod init;

/// `supdoc completions`.
pub(crate) mod completions;

/// `supdoc resolve`.
pub(crate) mod resolve;

/// `supdoc config show` and `supdoc namespace list`.
pub(crate) mod config;

/// `supdoc check`.
pub(crate) mod check;

/// `supdoc update`.
pub(crate) mod update;

/// `supdoc list`.
pub(crate) mod list;

/// `supdoc fix`.
pub(crate) mod fix;

/// `supdoc namespace rename`.
pub(crate) mod rename;

/// `supdoc skill install`.
pub(crate) mod skill;

/// `supdoc schema`.
pub(crate) mod schema;

/// `supdoc trust`.
pub(crate) mod trust;
/// `supdoc upgrade`.
pub(crate) mod upgrade;

/// Test doubles the unit tests of this module share.
#[cfg(test)]
pub(crate) mod testing;

use std::fmt;

use crate::exit::ExitStatus;
use crate::finding::{Location, Snippet};
use crate::output::{Output, OutputError};
use crate::path::AbsolutePath;
use crate::read::error::CartridgeLoadError;
use crate::scan::assemble::ScanError;
use crate::scan::walk::GlobalExcludes;

use args::{Command, ConfigCommand, GlobalArgs, Inspect, NamespaceCommand, SkillCommand};

/// Everything a command receives besides its own arguments.
#[derive(Debug)]
pub(crate) struct Context {
    /// The flags every command accepts.
    pub(crate) global: GlobalArgs,
    /// The current directory, read once by `main`.
    pub(crate) cwd: AbsolutePath,
    /// The git global excludes file every walk honors, found once by `main`.
    pub(crate) global_excludes: GlobalExcludes,
    /// The variables trust is decided from, read once by `main`.
    pub(crate) trust: crate::trust::TrustEnv,
}

/// How a command that ran to completion ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing is wrong. Exit code 0.
    Clean,
    /// The command ran, and what it checks is failing or was refused. Exit code 1.
    Failing,
}

impl Outcome {
    /// The process exit code: 0 or 1.
    #[must_use]
    pub(crate) const fn exit_code(self) -> u8 {
        match self {
            Self::Clean => ExitStatus::Clean,
            Self::Failing => ExitStatus::Failed,
        }
        .code()
    }
}

/// The place in a file that a [`CliError`] is about, with its source line captured so rendering
/// never reads files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ErrorSite {
    /// The file, line and column.
    pub(crate) location: Location,
    /// The source lines to show under the message.
    pub(crate) snippet: Snippet,
}

/// A command that could not finish: a stable code, what happened, what to do next, and the exit
/// code to use.
///
/// Commands convert the errors of the modules they call with [`CliError::new`], using that
/// module's own `code()`, `help()` and `exit_code()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliError {
    /// Stable code, never reworded: `config-extends-missing`.
    code: &'static str,
    /// What happened, in one line.
    message: String,
    /// What to do next.
    help: String,
    /// The process exit status.
    status: ExitStatus,
    /// Where in a file it happened, when it is about one place.
    site: Option<Box<ErrorSite>>,
}

impl CliError {
    /// An error with every part its JSON form needs.
    #[must_use]
    pub(crate) fn new(
        code: &'static str,
        message: impl Into<String>,
        help: impl Into<String>,
        status: ExitStatus,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            help: help.into(),
            status,
            site: None,
        }
    }

    /// The same error, pointing at `site` in a file.
    #[must_use]
    pub(crate) fn at(self, site: ErrorSite) -> Self {
        Self {
            site: Some(Box::new(site)),
            ..self
        }
    }

    /// The error for a report that could not be written, for `subject` such as `the check report`.
    ///
    /// Every command's report failure shares this help and the I/O exit code.
    #[must_use]
    pub(crate) fn output_failed(subject: &str, error: &OutputError) -> Self {
        Self::new(
            "cli-output-failed",
            format!(
                "cannot write {subject}: {}",
                without_os_code(&error.to_string())
            ),
            "Check that standard output and standard error are open and writable, or redirect \
             both to a file with `> supdoc.out 2>&1`",
            ExitStatus::Io,
        )
    }

    /// The stable code.
    #[must_use]
    pub(crate) const fn code(&self) -> &'static str {
        self.code
    }

    /// What happened.
    #[must_use]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// What to do next.
    #[must_use]
    pub(crate) fn help(&self) -> &str {
        &self.help
    }

    /// The process exit code.
    #[must_use]
    pub(crate) const fn exit_code(&self) -> u8 {
        self.status.code()
    }

    /// Where in a file it happened, if it is about one place.
    #[must_use]
    pub(crate) fn site(&self) -> Option<&ErrorSite> {
        self.site.as_deref()
    }
}

impl From<ScanError> for CliError {
    fn from(error: ScanError) -> Self {
        Self::new(
            error.code(),
            error.to_string(),
            error.help(),
            error.status(),
        )
    }
}

impl From<&CartridgeLoadError> for CliError {
    fn from(error: &CartridgeLoadError) -> Self {
        Self::new(
            CartridgeLoadError::code(),
            error.to_string(),
            CartridgeLoadError::help(),
            ExitStatus::Bug,
        )
    }
}

/// `error`'s reason with its first character lowercased, so it reads on after a colon, without
/// the operating system's error-code suffix. The rest keeps its case, so a path in it stays
/// exact.
pub(crate) fn cause(error: &std::io::Error) -> String {
    let reason = without_os_code(&error.to_string()).to_owned();
    let mut chars = reason.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// `text` without a trailing ` (os ...)` code such as the one `io::Error` appends.
fn without_os_code(text: &str) -> &str {
    text.split(" (os ").next().unwrap_or(text)
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Runs `command`, printing everything through `output`.
///
/// This `match` is complete: it never changes when a command is built. A command that finds
/// `output` closed returns its normal [`Outcome`], so the run keeps its own exit code.
///
/// # Errors
///
/// Returns the command's own [`CliError`].
pub(crate) fn run(
    command: &Command,
    skill_dirs: &skill::SkillDirs,
    tools: &crate::code_blocks::tools::ToolEnv,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    match command {
        Command::Check(args) => check::run(args, tools, context, output),
        Command::Update(args) => update::run(args, context, output),
        Command::Fix(args) => fix::run(args, tools, context, output),
        Command::List(args) => list::run(args, context, output),
        Command::Resolve(args) => resolve::run(args, context, output),
        Command::Init(args) => init::run(args, context, output),
        Command::Config(ConfigCommand::Show) => config::run(Inspect::ConfigShow, context, output),
        Command::Namespace(NamespaceCommand::List(args)) => {
            config::run(Inspect::NamespaceList(args), context, output)
        }
        Command::Namespace(NamespaceCommand::Rename(args)) => rename::run(args, context, output),
        Command::Schema(args) => schema::run(args, context, output),
        Command::Trust(args) => trust::run(args, context, output),
        Command::Completions(args) => completions::run(args, context, output),
        Command::Skill(SkillCommand::Install(args)) => {
            skill::run(args, skill_dirs, context, output)
        }
        Command::Upgrade(args) => upgrade::run(args, context, output),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;

    use super::cause;

    #[test]
    fn cause_lowercases_only_the_first_character_so_a_path_keeps_its_case() {
        let refusal = io::Error::new(
            io::ErrorKind::PermissionDenied,
            "`/Scratch/MixedCase/trusted-roots` lies outside",
        );

        assert_eq!(
            (
                cause(&refusal),
                cause(&io::Error::from_raw_os_error(libc::ENOENT))
            ),
            (
                "`/Scratch/MixedCase/trusted-roots` lies outside".to_owned(),
                "no such file or directory".to_owned()
            )
        );
    }
}
