//! `supdoc completions`: prints a shell completion script (06 X14).
//!
//! Completions never load the config, so they work in a broken project.

use std::io::{self, Write as _};

use clap::CommandFactory as _;
use clap_complete::Shell;

use super::args::{Cli, CompletionsArgs};
use super::{CliError, Context, Outcome};

/// The exit code for an I/O error (06 X7).
const IO_EXIT_CODE: u8 = 74;

/// Runs `supdoc completions`, writing the script to standard output.
///
/// A closed pipe is not a failure of supdoc, so it ends cleanly (06 X13).
///
/// # Errors
///
/// Returns `cli/completions-write` when standard output cannot be written for a reason other
/// than a closed pipe.
pub fn run(args: &CompletionsArgs, _context: &Context) -> Result<Outcome, CliError> {
    let script = script(args.shell);
    let mut stdout = io::stdout().lock();
    match stdout.write_all(&script).and_then(|()| stdout.flush()) {
        Ok(()) => Ok(Outcome::Clean),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(Outcome::Clean),
        Err(error) => Err(CliError::new(
            "cli/completions-write",
            format!(
                "could not write the {shell} completion script: {error}",
                shell = args.shell
            ),
            "check that standard output is open and writable, or redirect it to a file",
            IO_EXIT_CODE,
        )),
    }
}

/// The completion script for `shell`, complete before any of it is written so that a failed
/// write never leaves half a script behind a panic.
fn script(shell: Shell) -> Vec<u8> {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "supdoc", &mut script);
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_gets_a_script_naming_supdoc_and_its_subcommands() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let text = String::from_utf8(script(shell)).expect("a script should be UTF-8");

            for expected in ["supdoc", "check", "completions"] {
                assert!(
                    text.contains(expected),
                    "the {shell} script should mention `{expected}`:\n{text}"
                );
            }
        }
    }
}
