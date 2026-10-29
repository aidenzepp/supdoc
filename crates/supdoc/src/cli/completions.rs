//! `supdoc completions`: prints a shell completion script (06 X14).
//!
//! Completions never load the config, so they work in a broken project.

use std::io::{self, Write};

use clap::CommandFactory as _;
use clap_complete::Shell;
use schemars::JsonSchema;
use serde::Serialize;

use crate::output::{Output, OutputError, Report, TextContext};

use super::args::{Cli, CompletionsArgs};
use super::{CliError, Context, Outcome};

/// The completion script as output data.
#[derive(Debug, Serialize, JsonSchema)]
struct ScriptReport {
    /// The script text, written as is in text output.
    script: String,
}

impl Report for ScriptReport {
    fn command(&self) -> &'static str {
        "completions"
    }

    fn render_text(&self, _context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(self.script.as_bytes())
    }
}

/// Runs `supdoc completions`, printing the script through `output`.
///
/// A closed pipe is not a failure of supdoc: `output` drops what it cannot deliver and the
/// command ends cleanly (06 X13).
///
/// # Errors
///
/// Returns `cli/completions-write` when standard output cannot be written for a reason other
/// than a closed pipe.
pub fn run(
    args: &CompletionsArgs,
    _context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let report = ScriptReport {
        script: script(args.shell),
    };
    output
        .report(&report, true)
        .map_err(|error| write_failed(args.shell, &error))?;
    Ok(Outcome::Clean)
}

/// The error for a script that could not be written.
fn write_failed(shell: Shell, error: &OutputError) -> CliError {
    CliError::new(
        "cli/completions-write",
        format!("could not write the {shell} completion script: {error}"),
        "check that standard output is open and writable, or redirect it to a file",
        CliError::IO_EXIT_CODE,
    )
}

/// The completion script for `shell`, complete before any of it is written so that a failed
/// write never leaves half a script behind.
fn script(shell: Shell) -> String {
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "supdoc", &mut script);
    String::from_utf8_lossy(&script).into_owned()
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::args::Command;
    use crate::output::{OutputFormat, OutputSettings, PathStyle, Verbosity};
    use crate::path::AbsolutePath;

    use super::*;

    /// A writer that keeps what it is given, shared with the test that reads it back.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the capture lock should be free");
            String::from_utf8(bytes.clone()).expect("captured output should be UTF-8")
        }
    }

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("the capture lock should be free")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose reader has hung up.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Text output rooted in `cwd`, writing to the given streams.
    fn output_to(cwd: &AbsolutePath, stdout: Box<dyn Write>, stderr: Box<dyn Write>) -> Output {
        let settings = OutputSettings {
            format: OutputFormat::Text,
            verbosity: Verbosity::Normal,
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
        };
        Output::new(settings, stdout, stderr)
    }

    fn zsh_args() -> (CompletionsArgs, Context) {
        let cli = Cli::try_parse_from(["supdoc", "completions", "zsh"])
            .expect("the completions command line should parse");
        let Command::Completions(args) = cli.command else {
            panic!("`supdoc completions zsh` should parse as the completions command");
        };
        let cwd = AbsolutePath::new(&std::env::temp_dir()).expect("a temp path is absolute");
        (
            args,
            Context {
                global: cli.global,
                cwd,
            },
        )
    }

    #[test]
    fn the_script_goes_to_stdout_through_output() {
        let (args, context) = zsh_args();
        let (stdout, stderr) = (Capture::default(), Capture::default());
        let mut output = output_to(
            &context.cwd,
            Box::new(stdout.clone()),
            Box::new(stderr.clone()),
        );

        let outcome = run(&args, &context, &mut output).expect("completions should succeed");

        assert_eq!(outcome, Outcome::Clean);
        assert_eq!(stdout.text(), script(args.shell), "stdout is the script");
        assert_eq!(stderr.text(), "", "completions say nothing on stderr");
    }

    #[test]
    fn a_closed_pipe_is_a_clean_exit() {
        let (args, context) = zsh_args();
        let mut output = output_to(
            &context.cwd,
            Box::new(ClosedPipe),
            Box::new(Capture::default()),
        );

        let outcome = run(&args, &context, &mut output).expect("a closed pipe is not an error");

        assert_eq!(outcome, Outcome::Clean);
        assert!(output.is_closed(), "the output should record the hang-up");
    }

    #[test]
    fn every_shell_gets_a_script_naming_supdoc_and_its_subcommands() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let text = script(shell);

            for expected in ["supdoc", "check", "completions"] {
                assert!(
                    text.contains(expected),
                    "the {shell} script should mention `{expected}`:\n{text}"
                );
            }
        }
    }
}
