//! The command-line interface: arguments, parsing and command dispatch (06).
//!
//! [`run`] holds the one dispatch `match`. Each command lives in `cli/<command>.rs` and exposes a
//! `run` function, so building a command means editing only its own file. Until a command is
//! built, its `run` returns the `cli/not-built` error.

/// Command-line argument definitions.
pub mod args;

/// Example invocations shown in help.
pub mod examples;

/// Parsing arguments, including JSON usage errors.
pub mod parse;

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

use crate::path::AbsolutePath;

use args::{Cli, Command, ConfigCommand, GlobalArgs, Inspect, NamespaceCommand, SkillCommand};

/// Everything a command receives besides its own arguments.
#[derive(Debug)]
pub struct Context {
    /// The flags every command accepts.
    pub global: GlobalArgs,
    /// The current directory, read once by `main` (01).
    pub cwd: AbsolutePath,
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

/// A command that could not finish: a stable code, what happened, what to do next, and the exit
/// code to use (06 X7, 07 fatal error shape).
///
/// Commands convert the errors of the modules they call with [`CliError::new`], using that
/// module's own `code()`, `help()` and `exit_code()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// Stable code, never reworded: `config/extends-missing`, `cli/not-built`.
    code: &'static str,
    /// What happened, in one line.
    message: String,
    /// What to do next.
    help: String,
    /// The process exit code.
    exit_code: u8,
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
        }
    }

    /// The error a command returns until it is built.
    #[must_use]
    pub fn not_built(command: &str) -> Self {
        Self::new(
            Self::NOT_BUILT,
            format!("`supdoc {command}` is not built yet"),
            format!(
                "this build has the arguments of `supdoc {command}` but not its behavior; use a release that includes the command"
            ),
            Self::BUG_EXIT_CODE,
        )
    }

    /// The code of [`CliError::not_built`].
    pub const NOT_BUILT: &'static str = "cli/not-built";

    /// The exit code for a supdoc bug (06 X7).
    pub const BUG_EXIT_CODE: u8 = 70;

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
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Runs the parsed command line from `cwd`.
///
/// This `match` is complete: it never changes when a command is built.
///
/// # Errors
///
/// Returns the command's own [`CliError`], or `cli/not-built` for a command that is not built.
pub fn run(cli: Cli, cwd: AbsolutePath) -> Result<Outcome, CliError> {
    let context = Context {
        global: cli.global,
        cwd,
    };
    match &cli.command {
        Command::Check(args) => check::run(args, &context),
        Command::Update(args) => update::run(args, &context),
        Command::Fix(args) => fix::run(args, &context),
        Command::List(args) => list::run(args, &context),
        Command::Resolve(args) => resolve::run(args, &context),
        Command::Init(args) => init::run(args, &context),
        Command::Config(ConfigCommand::Show) => config::run(Inspect::ConfigShow, &context),
        Command::Namespace(NamespaceCommand::List) => config::run(Inspect::NamespaceList, &context),
        Command::Namespace(NamespaceCommand::Rename(args)) => rename::run(args, &context),
        Command::Schema(args) => schema::run(args, &context),
        Command::Completions(args) => completions::run(args, &context),
        Command::Skill(SkillCommand::Install(args)) => skill::run(args, &context),
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser as _};

    use super::*;

    /// One invocation per leaf subcommand, with the dispatch path it must take.
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

    fn cwd() -> AbsolutePath {
        AbsolutePath::new(&std::env::temp_dir()).expect("the temp directory should be absolute")
    }

    fn dispatch(argv: &[&str]) -> Result<Outcome, CliError> {
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|error| {
            panic!("`{}` should parse: {error}", argv.join(" "));
        });
        run(cli, cwd())
    }

    #[test]
    fn every_subcommand_dispatches_to_its_own_run() {
        for (name, argv) in INVOCATIONS {
            let result = dispatch(argv);

            // A command that is built has its own tests. One that is not built must say so by
            // name, which proves the match routed it to the right file.
            if let Err(error) = result.as_ref().map_err(CliError::code)
                && error == CliError::NOT_BUILT
            {
                let message = result.as_ref().err().map(CliError::message);
                assert_eq!(
                    message,
                    Some(format!("`supdoc {name}` is not built yet").as_str()),
                    "`supdoc {name}` was routed to another command",
                );
            }
        }
    }

    #[test]
    fn unbuilt_commands_return_the_not_built_error_with_exit_code_70() {
        for (name, argv) in INVOCATIONS {
            let Err(error) = dispatch(argv) else {
                continue;
            };
            if error.code() == CliError::NOT_BUILT {
                assert_eq!(error.exit_code(), 70, "exit code of unbuilt `{name}`");
                assert!(
                    error.help().contains(name),
                    "help of unbuilt `{name}` should name the command: {}",
                    error.help()
                );
            }
        }
    }

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
