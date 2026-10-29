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
    /// The script text, written as is in text and GitHub output.
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
        .artifact(&report, &report.script, true)
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
    use std::io::{self, Write};

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

    /// Output in `format` rooted in `cwd`, writing to the given streams.
    fn output_in(
        format: OutputFormat,
        cwd: &AbsolutePath,
        stdout: Box<dyn Write>,
        stderr: Box<dyn Write>,
    ) -> Output {
        let settings = OutputSettings {
            format,
            verbosity: Verbosity::Normal,
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
        };
        Output::new(settings, stdout, stderr)
    }

    /// Text output rooted in `cwd`, writing to the given streams.
    fn output_to(cwd: &AbsolutePath, stdout: Box<dyn Write>, stderr: Box<dyn Write>) -> Output {
        output_in(OutputFormat::Text, cwd, stdout, stderr)
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

    /// What `completions zsh` writes to stdout under `format`, and its stderr.
    fn run_under(format: OutputFormat) -> (String, String) {
        let (args, context) = zsh_args();
        let (stdout, stderr) = (Capture::default(), Capture::default());
        let mut output = output_in(
            format,
            &context.cwd,
            Box::new(stdout.clone()),
            Box::new(stderr.clone()),
        );

        let outcome = run(&args, &context, &mut output).expect("completions should succeed");

        assert_eq!(outcome, Outcome::Clean, "{format:?} should exit 0");
        (stdout.text(), stderr.text())
    }

    #[test]
    fn text_and_github_print_the_identical_raw_script() {
        let (args, _context) = zsh_args();
        for format in [OutputFormat::Text, OutputFormat::Github] {
            let (stdout, stderr) = run_under(format);

            assert_eq!(
                stdout,
                script(args.shell),
                "{format:?} should print the raw script"
            );
            assert_eq!(stderr, "", "{format:?} says nothing on stderr");
        }
    }

    #[test]
    fn json_wraps_the_script_in_one_envelope() {
        let (args, _context) = zsh_args();
        let (stdout, _stderr) = run_under(OutputFormat::Json);

        let envelope: serde_json::Value =
            serde_json::from_str(&stdout).expect("--json should print one JSON object");

        assert_eq!(stdout.lines().count(), 1, "the envelope is one line");
        assert_eq!(
            envelope.pointer("/command"),
            Some(&serde_json::json!("completions"))
        );
        assert_eq!(
            envelope.pointer("/data/script"),
            Some(&serde_json::json!(script(args.shell))),
            "data.script should be the script"
        );
    }

    #[test]
    fn jsonl_summary_line_carries_the_script() {
        let (args, _context) = zsh_args();
        let (stdout, _stderr) = run_under(OutputFormat::Jsonl);

        let last = stdout
            .lines()
            .last()
            .expect("--jsonl should print at least a summary line");
        let summary: serde_json::Value =
            serde_json::from_str(last).expect("the summary line should be JSON");

        assert_eq!(
            summary.pointer("/type"),
            Some(&serde_json::json!("summary"))
        );
        assert_eq!(
            summary.pointer("/script"),
            Some(&serde_json::json!(script(args.shell))),
            "the summary line should contain script"
        );
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
