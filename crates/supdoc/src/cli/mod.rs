//! The command-line interface: arguments, parsing and command dispatch (06).
//!
//! [`run`] holds the one dispatch `match`. Each command lives in `cli/<command>.rs` and exposes a
//! `run` function, so building a command means editing only its own file.

/// Command-line argument definitions.
pub mod args;

/// Example invocations shown in help.
pub mod examples;

/// Parsing arguments, including JSON usage errors.
pub mod parse;

/// Setup shared by every command that loads the config.
pub mod project;

/// `supdoc init`.
pub mod init;

/// `supdoc completions`.
pub mod completions;

/// `supdoc resolve`.
pub mod resolve;

/// `supdoc config show` and `supdoc namespace list`.
pub mod config;

/// `supdoc check`.
pub mod check;

/// `supdoc update`.
pub mod update;

/// `supdoc list`.
pub mod list;

/// `supdoc fix`.
pub mod fix;

/// `supdoc namespace rename`.
pub mod rename;

/// `supdoc skill install`.
pub mod skill;

/// `supdoc schema`.
pub mod schema;

use std::fmt;

use crate::finding::{Location, Snippet};
use crate::output::Output;
use crate::path::AbsolutePath;

use args::{Command, ConfigCommand, GlobalArgs, Inspect, NamespaceCommand, SkillCommand};

/// Everything a command receives besides its own arguments.
#[derive(Debug)]
pub struct Context {
    /// The flags every command accepts.
    pub global: GlobalArgs,
    /// The current directory, read once by `main` (01).
    pub cwd: AbsolutePath,
    /// The user's home directory (`HOME`), read once by `main`.
    pub home: Option<AbsolutePath>,
    /// Claude Code's config directory (`CLAUDE_CONFIG_DIR`), read once by `main`.
    pub claude_config_dir: Option<AbsolutePath>,
}

/// How a command that ran to completion ended (06 X7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing is wrong. Exit code 0.
    Clean,
    /// The command ran, and what it checks is failing or was refused. Exit code 1.
    Failing,
}

impl Outcome {
    /// The process exit code: 0 or 1.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Clean => 0,
            Self::Failing => 1,
        }
    }
}

/// The place in a file that a [`CliError`] is about, with its source line captured so rendering
/// never reads files (07 G11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorSite {
    /// The file, line and column.
    pub location: Location,
    /// The source lines to show under the message.
    pub snippet: Snippet,
}

/// A command that could not finish: a stable code, what happened, what to do next, and the exit
/// code to use (06 X7, 07 fatal error shape).
///
/// Commands convert the errors of the modules they call with [`CliError::new`], using that
/// module's own `code()`, `help()` and `exit_code()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// Stable code, never reworded: `config-extends-missing`.
    code: &'static str,
    /// What happened, in one line.
    message: String,
    /// What to do next.
    help: String,
    /// The process exit code.
    exit_code: u8,
    /// Where in a file it happened, when it is about one place.
    site: Option<Box<ErrorSite>>,
}

impl CliError {
    /// An error with every part its JSON form needs.
    #[must_use]
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        help: impl Into<String>,
        exit_code: u8,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            help: help.into(),
            exit_code,
            site: None,
        }
    }

    /// The same error, pointing at `site` in a file.
    #[must_use]
    pub fn at(self, site: ErrorSite) -> Self {
        Self {
            site: Some(Box::new(site)),
            ..self
        }
    }

    /// The exit code for a supdoc bug (06 X7).
    pub const BUG_EXIT_CODE: u8 = 70;

    /// The exit code for a usage error, such as a missing `[PATH]` (06 X7).
    pub const USAGE_EXIT_CODE: u8 = 2;

    /// The exit code for a refused change, such as an existing config (06 X7).
    pub const REFUSED_EXIT_CODE: u8 = 1;

    /// The exit code for an I/O failure (06 X7).
    pub const IO_EXIT_CODE: u8 = 74;

    /// The exit code for a config or lockfile problem (06 X7).
    pub const CONFIG_EXIT_CODE: u8 = 78;

    /// The stable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// What happened.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// What to do next.
    #[must_use]
    pub fn help(&self) -> &str {
        &self.help
    }

    /// The process exit code.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        self.exit_code
    }

    /// Where in a file it happened, if it is about one place.
    #[must_use]
    pub fn site(&self) -> Option<&ErrorSite> {
        self.site.as_deref()
    }
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
/// `output` closed returns its normal [`Outcome`]; `main` turns that into a clean exit (06 X13).
///
/// # Errors
///
/// Returns the command's own [`CliError`].
pub fn run(command: &Command, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    match command {
        Command::Check(args) => check::run(args, context, output),
        Command::Update(args) => update::run(args, context, output),
        Command::Fix(args) => fix::run(args, context, output),
        Command::List(args) => list::run(args, context, output),
        Command::Resolve(args) => resolve::run(args, context, output),
        Command::Init(args) => init::run(args, context, output),
        Command::Config(ConfigCommand::Show) => config::run(Inspect::ConfigShow, context, output),
        Command::Namespace(NamespaceCommand::List(args)) => config::run(
            Inspect::NamespaceList { limit: args.limit },
            context,
            output,
        ),
        Command::Namespace(NamespaceCommand::Rename(args)) => rename::run(args, context, output),
        Command::Schema(args) => schema::run(args, context, output),
        Command::Completions(args) => completions::run(args, context, output),
        Command::Skill(SkillCommand::Install(args)) => skill::run(args, context, output),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use crate::cli::args::Cli;

    /// One invocation per leaf subcommand, that the arguments must cover.
    const INVOCATIONS: [(&str, &[&str]); 12] = [
        ("check", &["supdoc", "check"]),
        ("update", &["supdoc", "update"]),
        ("fix", &["supdoc", "fix"]),
        ("list", &["supdoc", "list"]),
        ("resolve", &["supdoc", "resolve", "src/lib.rs"]),
        ("init", &["supdoc", "init"]),
        ("config show", &["supdoc", "config", "show"]),
        ("namespace list", &["supdoc", "namespace", "list"]),
        (
            "namespace rename",
            &["supdoc", "namespace", "rename", "auth", "identity"],
        ),
        ("schema", &["supdoc", "schema"]),
        ("completions", &["supdoc", "completions", "zsh"]),
        ("skill install", &["supdoc", "skill", "install"]),
    ];

    #[test]
    fn every_leaf_subcommand_has_an_invocation_above() {
        fn collect_leaves(command: &clap::Command, prefix: &str, found: &mut Vec<String>) {
            for subcommand in command.get_subcommands() {
                if subcommand.get_name() == "help" {
                    continue;
                }
                let path = format!("{prefix}{}", subcommand.get_name());
                if subcommand.has_subcommands() {
                    collect_leaves(subcommand, &format!("{path} "), found);
                } else {
                    found.push(path);
                }
            }
        }

        let mut leaves = Vec::new();
        collect_leaves(&Cli::command(), "", &mut leaves);
        leaves.sort();
        let mut invoked: Vec<&str> = INVOCATIONS.iter().map(|(name, _)| *name).collect();
        invoked.sort_unstable();

        assert_eq!(leaves, invoked, "leaf subcommands vs tested invocations");
    }
}
