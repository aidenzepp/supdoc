//! Turning the command line into a [`Cli`], or into the text to print and the exit code to use.
//!
//! Parsing never prints and never exits: [`parse_from`] returns what to show, so `main` is the only
//! code that touches the process and tests can inspect every outcome (06 `main`, X8).

use std::ffi::{OsStr, OsString};

use clap::builder::{FalseyValueParser, TypedValueParser as _};
use clap::error::ErrorKind;
use clap::{Parser, ValueEnum};

use super::CliError;
use super::args::{Cli, resolve_format};
use crate::output::OutputFormat;

/// What parsing the command line produced.
#[derive(Debug)]
pub enum Parsed {
    /// A valid command line: run it.
    Run(Box<Cli>),
    /// Help, a version, or a usage error: print it and exit.
    Print(Printout),
}

/// Text for `main` to write, and the exit code to finish with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Printout {
    /// Where the text goes.
    pub stream: Stream,
    /// The text, ending in a newline.
    pub text: String,
    /// The process exit code: 0 for help and version, 2 for a usage error (06 X7).
    pub exit_code: u8,
}

/// The standard stream a [`Printout`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Data the user asked for: help and version.
    Stdout,
    /// Errors.
    Stderr,
}

/// The code of the error printed when the command line cannot be parsed.
pub const USAGE_ERROR_CODE: &str = "cli/usage";

/// Parses the process's own arguments and environment.
#[must_use]
pub fn parse_process() -> Parsed {
    parse_from(std::env::args_os(), |name| std::env::var(name).ok())
}

/// Parses `args` (the program name first).
///
/// `env` looks up an environment variable by name. It only decides whether a usage error is
/// printed as JSON, since a failed parse leaves no flags to read. clap itself reads the
/// environment to fill flags.
#[must_use]
pub fn parse_from<I, T>(args: I, env: impl Fn(&str) -> Option<String>) -> Parsed
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    match Cli::try_parse_from(&args) {
        Ok(cli) => Parsed::Run(Box::new(cli)),
        Err(error) => Parsed::Print(printout(&error, requested_format(&args, env))),
    }
}

/// Decides what to print for a clap error or an explicit `--help` / `--version`.
fn printout(error: &clap::Error, format: OutputFormat) -> Printout {
    let text = error.render().to_string();
    match error.kind() {
        // Bare `supdoc` shows help, as a successful request for help (06 X1).
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => Printout {
            stream: Stream::Stdout,
            text: ensure_newline(text),
            exit_code: 0,
        },
        _ if format.is_json() => Printout {
            stream: Stream::Stderr,
            text: usage_error_json(&text),
            exit_code: CliError::USAGE_EXIT_CODE,
        },
        _ => Printout {
            stream: Stream::Stderr,
            text: ensure_newline(text),
            exit_code: CliError::USAGE_EXIT_CODE,
        },
    }
}

/// Appends a newline unless the text already ends in one.
fn ensure_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The JSON error object for a usage error (06 X8, 07 fatal error shape), on one line.
fn usage_error_json(rendered: &str) -> String {
    #[derive(serde::Serialize)]
    struct UsageError<'a> {
        error: bool,
        kind: &'static str,
        message: &'a str,
        help: String,
        exit_code: u8,
    }

    let (message, help) = split_clap_message(rendered);
    let error = UsageError {
        error: true,
        kind: USAGE_ERROR_CODE,
        message,
        help,
        exit_code: CliError::USAGE_EXIT_CODE,
    };
    // Serializing plain strings and numbers cannot fail; the fallback keeps stderr valid JSON anyway.
    let mut line = serde_json::to_string(&error).unwrap_or_else(|_| {
        r#"{"error":true,"kind":"cli/usage","message":"invalid command line","help":"run `supdoc --help`","exit_code":2}"#
            .to_owned()
    });
    line.push('\n');
    line
}

/// Splits clap's rendered error into the message and a help line.
///
/// The message is clap's first paragraph without its `error: ` prefix, which already lists valid
/// values. The help repeats clap's `tip:` suggestions and points at `--help`.
fn split_clap_message(rendered: &str) -> (&str, String) {
    let first_paragraph = rendered.split("\n\n").next().unwrap_or(rendered).trim();
    let message = first_paragraph
        .strip_prefix("error: ")
        .unwrap_or(first_paragraph);
    let tips: Vec<&str> = rendered
        .lines()
        .filter_map(|line| line.trim().strip_prefix("tip: "))
        .collect();
    let pointer = "run `supdoc --help`, or `supdoc <command> --help`, to see valid usage";
    let help = if tips.is_empty() {
        pointer.to_owned()
    } else {
        format!("{}; {pointer}", tips.join("; "))
    };
    (message, help)
}

/// The format the command line asks for, read from the raw arguments and environment.
///
/// Mirrors [`resolve_format`]: `--jsonl` beats `--json`, which beats `--output`, and an
/// argument beats the environment variable for the same flag. Anything unreadable counts as text.
fn requested_format(args: &[OsString], env: impl Fn(&str) -> Option<String>) -> OutputFormat {
    let mut json = false;
    let mut jsonl = false;
    let mut output: Option<String> = None;

    let mut words = args
        .iter()
        .skip(1)
        .filter_map(|arg| arg.to_str())
        .take_while(|arg| *arg != "--");
    while let Some(word) = words.next() {
        match word {
            "--json" => json = true,
            "--jsonl" => jsonl = true,
            "--output" => output = words.next().map(str::to_owned),
            _ => {
                if let Some(value) = word.strip_prefix("--output=") {
                    output = Some(value.to_owned());
                }
            }
        }
    }

    // The same parser clap applies to these variables, so both agree on which values are truthy.
    let truthy = |name: &str| {
        env(name).is_some_and(|value| {
            FalseyValueParser::new()
                .parse_ref(&clap::Command::new("supdoc"), None, OsStr::new(&value))
                .unwrap_or(false)
        })
    };
    let output = output
        .or_else(|| env("SUPDOC_OUTPUT"))
        .and_then(|value| OutputFormat::from_str(&value, false).ok())
        .unwrap_or(OutputFormat::Text);

    resolve_format(
        output,
        json || truthy("SUPDOC_JSON"),
        jsonl || truthy("SUPDOC_JSONL"),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::process::Command as Process;

    use clap::{ColorChoice, CommandFactory};

    use super::*;
    use crate::cli::args::{Command, ListArgs};

    /// Parses `args` (without the program name) with an empty environment.
    fn parse(args: &[&str]) -> Parsed {
        parse_from(
            std::iter::once("supdoc").chain(args.iter().copied()),
            |_| None,
        )
    }

    fn parse_cli(args: &[&str]) -> Cli {
        match parse(args) {
            Parsed::Run(cli) => *cli,
            Parsed::Print(printout) => {
                panic!("`supdoc {}` should parse, got {printout:?}", args.join(" "))
            }
        }
    }

    fn parse_printout(args: &[&str]) -> Printout {
        match parse(args) {
            Parsed::Print(printout) => printout,
            Parsed::Run(cli) => panic!("`supdoc {}` should not parse: {cli:?}", args.join(" ")),
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_supdoc_prints_help_to_stdout_and_exits_0() {
        let printout = parse_printout(&[]);

        assert_eq!(printout.stream, Stream::Stdout);
        assert_eq!(printout.exit_code, 0);
        assert!(
            printout.text.contains("Usage: supdoc") && printout.text.contains("Commands:"),
            "bare supdoc should print the command list: {}",
            printout.text
        );
    }

    #[test]
    fn version_prints_to_stdout_and_exits_0() {
        let printout = parse_printout(&["--version"]);

        assert_eq!(printout.stream, Stream::Stdout);
        assert_eq!(printout.exit_code, 0);
        assert_eq!(
            printout.text,
            format!("supdoc {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn misspelled_subcommand_suggests_nearest() {
        let printout = parse_printout(&["chek"]);

        assert_eq!(printout.stream, Stream::Stderr);
        assert_eq!(printout.exit_code, 2);
        assert!(
            printout.text.contains("'check'"),
            "`supdoc chek` should suggest `check`: {}",
            printout.text
        );
    }

    #[test]
    fn json_and_jsonl_aliases_select_format() {
        assert_eq!(
            parse_cli(&["--json", "check"]).global.format(),
            OutputFormat::Json
        );
        assert_eq!(
            parse_cli(&["check", "--jsonl"]).global.format(),
            OutputFormat::Jsonl
        );
        assert_eq!(
            parse_cli(&["check", "--output", "github"]).global.format(),
            OutputFormat::Github
        );
        assert_eq!(parse_cli(&["check"]).global.format(), OutputFormat::Text);
    }

    #[test]
    fn jsonl_beats_json_beats_output() {
        for (args, expected) in [
            (
                &["check", "--json", "--output", "github"][..],
                OutputFormat::Json,
            ),
            (&["check", "--jsonl", "--json"][..], OutputFormat::Jsonl),
            (
                &["check", "--jsonl", "--output", "text"][..],
                OutputFormat::Jsonl,
            ),
            (
                &["check", "--output", "jsonl", "--json"][..],
                OutputFormat::Json,
            ),
        ] {
            assert_eq!(
                parse_cli(args).global.format(),
                expected,
                "format of `supdoc {}`",
                args.join(" ")
            );
        }
    }

    #[test]
    fn dry_run_beats_yes() {
        use crate::cli::args::WriteMode;

        let mode_of = |args: &[&str]| match parse_cli(args).command {
            Command::Update(update) => update.confirm.mode(),
            other => panic!("expected update, got {other:?}"),
        };

        assert_eq!(mode_of(&["update"]), WriteMode::Preview);
        assert_eq!(mode_of(&["update", "--yes"]), WriteMode::Apply);
        assert_eq!(mode_of(&["update", "--dry-run"]), WriteMode::Preview);
        assert_eq!(
            mode_of(&["update", "--yes", "--dry-run"]),
            WriteMode::Preview
        );
    }

    #[test]
    fn update_states_default_to_failed_new_obsolete() {
        use crate::state::LinkState;

        match parse_cli(&["update"]).command {
            Command::Update(update) => assert_eq!(
                update.state,
                [LinkState::Failed, LinkState::New, LinkState::Obsolete]
            ),
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[test]
    fn list_defaults_to_100_and_accepts_a_target_states_and_limit() {
        let Command::List(defaults) = parse_cli(&["list"]).command else {
            panic!("expected list");
        };
        let Command::List(given) = parse_cli(&[
            "list",
            "auth:src/session.rs#login",
            "--state",
            "failed,new",
            "--limit",
            "0",
        ])
        .command
        else {
            panic!("expected list");
        };

        assert_eq!((defaults.limit, defaults.target.is_none()), (100, true));
        assert_eq!(given.limit, 0);
        assert_eq!(given.target.as_deref(), Some("auth:src/session.rs#login"));
        assert_eq!(given.state.len(), 2);
    }

    #[test]
    fn invalid_namespace_is_a_usage_error_with_the_types_own_message() {
        let printout = parse_printout(&["namespace", "rename", "c", "identity"]);

        assert_eq!(printout.exit_code, 2);
        assert!(
            printout
                .text
                .contains("namespace names need at least 2 characters (got 'c')"),
            "usage error should carry the type's message: {}",
            printout.text
        );
    }

    #[test]
    fn invalid_enum_value_lists_the_valid_ones() {
        let printout = parse_printout(&["check", "--output", "xml"]);

        assert!(
            printout
                .text
                .contains("[possible values: text, json, jsonl, github]"),
            "usage error should list the valid formats: {}",
            printout.text
        );
    }

    #[test]
    fn every_flag_has_an_env_var() {
        let mut command = Cli::command();
        command.build();
        let mut missing = Vec::new();
        let mut visit = |path: String, command: &clap::Command| {
            for arg in command.get_arguments() {
                let id = arg.get_id().as_str();
                let is_flag = arg.get_long().is_some() || arg.get_short().is_some();
                if is_flag && !["help", "version"].contains(&id) && arg.get_env().is_none() {
                    missing.push(format!("{path} --{id}"));
                }
            }
        };
        visit("supdoc".to_owned(), &command);
        for subcommand in command.get_subcommands() {
            visit(format!("supdoc {}", subcommand.get_name()), subcommand);
            for leaf in subcommand.get_subcommands() {
                visit(
                    format!("supdoc {} {}", subcommand.get_name(), leaf.get_name()),
                    leaf,
                );
            }
        }

        assert!(
            missing.is_empty(),
            "flags without a SUPDOC_* variable: {missing:?}"
        );
    }

    #[test]
    fn every_env_var_is_prefixed_supdoc() {
        let mut command = Cli::command();
        command.build();
        let mut names = Vec::new();
        let mut collect = |command: &clap::Command| {
            names.extend(
                command
                    .get_arguments()
                    .filter_map(|arg| arg.get_env())
                    .map(|name| name.to_string_lossy().into_owned()),
            );
        };
        collect(&command);
        for subcommand in command.get_subcommands() {
            collect(subcommand);
            subcommand.get_subcommands().for_each(&mut collect);
        }

        let wrong: Vec<_> = names
            .iter()
            .filter(|name| !name.starts_with("SUPDOC_"))
            .collect();
        assert!(
            !names.is_empty() && wrong.is_empty(),
            "env names: {names:?}"
        );
    }

    #[test]
    fn every_subcommand_has_after_help_examples() {
        fn walk(path: &str, command: &clap::Command, leaves: &mut Vec<(String, String)>) {
            for subcommand in command.get_subcommands() {
                if subcommand.get_name() == "help" {
                    continue;
                }
                let name = format!("{path} {}", subcommand.get_name());
                if subcommand.has_subcommands() {
                    walk(&name, subcommand, leaves);
                } else {
                    let after = subcommand
                        .get_after_help()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    leaves.push((name, after));
                }
            }
        }
        let mut leaves = Vec::new();
        walk("supdoc", &Cli::command(), &mut leaves);

        assert_eq!(leaves.len(), 12, "leaf subcommands: {leaves:?}");
        for (name, after_help) in leaves {
            assert!(
                after_help.starts_with("Examples:") && after_help.contains(&format!("\n  {name}")),
                "`{name}` needs after_help examples that start with `{name}`: {after_help:?}"
            );
        }
    }

    #[test]
    fn top_level_help_has_three_examples() {
        let after_help = Cli::command()
            .get_after_help()
            .map(ToString::to_string)
            .unwrap_or_default();
        let examples = example_lines(&after_help)
            .filter(|line| !line.contains("--json"))
            .count();

        assert_eq!(examples, 3, "top-level examples: {after_help}");
    }

    /// The runnable lines of an `Examples:` block: indented, not comments.
    fn example_lines(text: &str) -> impl Iterator<Item = &str> {
        text.lines()
            .map(str::trim)
            .filter(|line| line.starts_with("supdoc "))
    }

    #[test]
    fn every_example_parses() {
        use crate::cli::examples::{
            CHECK_EXAMPLES, COMPLETIONS_EXAMPLES, CONFIG_SHOW_EXAMPLES, FIX_EXAMPLES,
            INIT_EXAMPLES, LIST_EXAMPLES, NAMESPACE_LIST_EXAMPLES, NAMESPACE_RENAME_EXAMPLES,
            RESOLVE_EXAMPLES, SCHEMA_EXAMPLES, SKILL_INSTALL_EXAMPLES, TOP_EXAMPLES,
            UPDATE_EXAMPLES,
        };

        for block in [
            TOP_EXAMPLES,
            CHECK_EXAMPLES,
            UPDATE_EXAMPLES,
            FIX_EXAMPLES,
            LIST_EXAMPLES,
            RESOLVE_EXAMPLES,
            CONFIG_SHOW_EXAMPLES,
            NAMESPACE_LIST_EXAMPLES,
            NAMESPACE_RENAME_EXAMPLES,
            INIT_EXAMPLES,
            SCHEMA_EXAMPLES,
            COMPLETIONS_EXAMPLES,
            SKILL_INSTALL_EXAMPLES,
        ] {
            for line in example_lines(block) {
                let invocation = line.split(" > ").next().unwrap_or(line);
                let args: Vec<&str> = invocation.split_whitespace().collect();
                assert!(
                    matches!(parse_from(args.iter().copied(), |_| None), Parsed::Run(_)),
                    "example `{line}` should parse"
                );
            }
        }
    }

    #[test]
    fn read_and_write_commands_say_so_in_their_help() {
        let mut command = Cli::command();
        command.build();
        for (name, expected) in [
            ("check", "read-only"),
            ("list", "read-only"),
            ("resolve", "read-only"),
            ("schema", "read-only"),
            ("completions", "read-only"),
            ("update", "writes"),
            ("fix", "writes"),
            ("init", "writes"),
        ] {
            let about = command
                .find_subcommand(name)
                .and_then(clap::Command::get_about)
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(
                about.contains(expected),
                "`{name}` about should say `{expected}`: {about}"
            );
        }
    }

    #[test]
    fn usage_error_is_json_on_stderr_when_json_requested() {
        for args in [
            &["chek", "--json"][..],
            &["--jsonl", "check", "--bogus"][..],
            &["--output", "json", "update", "--state", "nope"][..],
            &["check", "--output=json", "--bogus"][..],
        ] {
            let printout = parse_printout(args);
            let value: serde_json::Value =
                serde_json::from_str(&printout.text).unwrap_or_else(|error| {
                    panic!(
                        "`supdoc {}` should print JSON ({error}): {}",
                        args.join(" "),
                        printout.text
                    )
                });

            assert_eq!(printout.stream, Stream::Stderr);
            assert_eq!(printout.exit_code, 2);
            assert_eq!(value.get("error"), Some(&serde_json::json!(true)));
            assert_eq!(
                value.get("kind"),
                Some(&serde_json::json!(USAGE_ERROR_CODE))
            );
            assert_eq!(value.get("exit_code"), Some(&serde_json::json!(2)));
            assert!(
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|m| !m.is_empty()),
                "message of `supdoc {}`: {value}",
                args.join(" ")
            );
            assert!(
                value
                    .get("help")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|h| h.contains("--help")),
                "help of `supdoc {}`: {value}",
                args.join(" ")
            );
            assert_eq!(
                printout.text.lines().count(),
                1,
                "the error should be one JSON line: {}",
                printout.text
            );
        }
    }

    #[test]
    fn usage_error_json_keys_come_in_the_documented_order() {
        let printout = parse_printout(&["chek", "--json"]);
        let keys: Vec<usize> = [
            "\"error\"",
            "\"kind\"",
            "\"message\"",
            "\"help\"",
            "\"exit_code\"",
        ]
        .iter()
        .map(|key| printout.text.find(key).unwrap_or(usize::MAX))
        .collect();

        assert!(keys.is_sorted(), "key order in {}", printout.text);
    }

    #[test]
    fn usage_error_is_json_when_the_environment_asks_for_it() {
        let is_json = |env: &[(&str, &str)], args: &[&str]| {
            let lookup = |asked: &str| {
                env.iter()
                    .find(|(name, _)| *name == asked)
                    .map(|(_, value)| (*value).to_owned())
            };
            match parse_from(
                std::iter::once("supdoc").chain(args.iter().copied()),
                lookup,
            ) {
                Parsed::Print(printout) => {
                    serde_json::from_str::<serde_json::Value>(&printout.text).is_ok()
                }
                Parsed::Run(_) => panic!("`supdoc {}` should not parse", args.join(" ")),
            }
        };

        assert!(is_json(&[("SUPDOC_OUTPUT", "json")], &["chek"]));
        assert!(is_json(&[("SUPDOC_JSONL", "1")], &["chek"]));
        assert!(is_json(&[("SUPDOC_JSON", "true")], &["chek"]));
        for truthy in ["2", "yes", "on", "anything", "TRUE"] {
            assert!(
                is_json(&[("SUPDOC_JSON", truthy)], &["chek"]),
                "SUPDOC_JSON={truthy} should request JSON"
            );
            assert!(
                is_json(&[("SUPDOC_JSONL", truthy)], &["chek"]),
                "SUPDOC_JSONL={truthy} should request JSON"
            );
        }
        for falsey in ["0", "false", "no", "off", ""] {
            assert!(
                !is_json(&[("SUPDOC_JSON", falsey)], &["chek"]),
                "SUPDOC_JSON={falsey:?} should not request JSON"
            );
        }
        assert!(
            !is_json(&[("SUPDOC_OUTPUT", "json")], &["chek", "--output", "text"]),
            "an --output argument should beat SUPDOC_OUTPUT"
        );
    }

    #[test]
    fn text_usage_error_is_clap_text_on_stderr() {
        let printout = parse_printout(&["check", "--bogus"]);

        assert_eq!(printout.stream, Stream::Stderr);
        assert!(
            printout
                .text
                .starts_with("error: unexpected argument '--bogus'"),
            "text usage error: {}",
            printout.text
        );
    }

    #[test]
    fn arguments_after_double_dash_do_not_request_json() {
        let args: Vec<OsString> = ["supdoc", "check", "--bogus", "--", "--json"]
            .iter()
            .map(OsString::from)
            .collect();

        assert_eq!(requested_format(&args, |_| None), OutputFormat::Text);
    }

    // The tests below read real environment variables. Setting variables in this process is
    // unsafe, so each one re-runs this test binary with the variables set and reads what the
    // child parsed.

    /// Environment variable that tells `child_probe` which arguments to parse.
    const PROBE_ARGS: &str = "SUPDOC_TEST_PROBE_ARGS";

    /// Not a test on its own: when re-run by `parse_in_child`, parses the arguments it was given
    /// and prints what clap produced. Otherwise it does nothing.
    #[test]
    fn child_probe() {
        let Some(joined) = std::env::var_os(PROBE_ARGS) else {
            return;
        };
        let args = joined
            .to_string_lossy()
            .split('\u{1f}')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let report = match parse_from(args, |name| std::env::var(name).ok()) {
            Parsed::Run(cli) => format!("PROBE {:?} {:?}", cli.global.format(), cli),
            Parsed::Print(printout) => format!("PROBE Print {}", printout.text.replace('\n', " ")),
        };
        writeln!(std::io::stdout(), "{report}").expect("the probe should write to stdout");
    }

    /// Parses `args` in a child process whose environment is only `env` plus the unrelated
    /// variables the test harness needs. Returns the `Cli` as a debug string, or the printed text.
    fn parse_in_child(args: &[&str], env: &[(&str, &str)]) -> String {
        let mut child = Process::new(
            std::env::current_exe().expect("the test binary should know its own path"),
        );
        for (name, _) in std::env::vars_os().filter_map(|(name, value)| {
            name.to_str()
                .filter(|n| n.starts_with("SUPDOC_"))
                .map(|n| (n.to_owned(), value))
        }) {
            let _ = child.env_remove(name);
        }
        let output = child
            .args([
                "--exact",
                "cli::parse::tests::child_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE_ARGS, args.join("\u{1f}"))
            .envs(env.iter().copied())
            .output()
            .expect("the test binary should re-run itself");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find_map(|line| line.split_once("PROBE ").map(|(_, rest)| rest.to_owned()))
            .unwrap_or_else(|| panic!("the child printed no probe line:\n{stdout}"))
    }

    #[test]
    fn falsey_env_values_turn_flags_off() {
        for value in ["0", "false", "no", "off", "FALSE"] {
            let parsed = parse_in_child(
                &["supdoc", "check"],
                &[("SUPDOC_QUIET", value), ("SUPDOC_DENY_WARNINGS", value)],
            );

            assert!(
                parsed.contains("quiet: false") && parsed.contains("deny_warnings: false"),
                "SUPDOC_QUIET={value} should turn the flag off: {parsed}"
            );
        }
    }

    #[test]
    fn skill_install_takes_no_positional_and_accepts_global_flags() {
        let printout = parse_printout(&["skill", "install", "claude"]);
        assert_eq!(printout.exit_code, 2, "{}", printout.text);
        for flag in ["-g", "--global"] {
            let parsed = parse_in_child(&["supdoc", "skill", "install", flag], &[]);
            assert!(parsed.contains("global: true"), "{flag}: {parsed}");
        }
        let plain = parse_in_child(&["supdoc", "skill", "install"], &[]);
        assert!(plain.contains("global: false"), "{plain}");
    }

    #[test]
    fn supdoc_global_env_follows_the_falsey_parser() {
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("0", false),
            ("false", false),
            ("", false),
        ] {
            let parsed =
                parse_in_child(&["supdoc", "skill", "install"], &[("SUPDOC_GLOBAL", value)]);
            assert!(
                parsed.contains(&format!("global: {expected}")),
                "SUPDOC_GLOBAL={value:?} should give global: {expected}: {parsed}"
            );
        }
    }

    #[test]
    fn truthy_env_values_turn_flags_on() {
        let parsed = parse_in_child(
            &["supdoc", "update"],
            &[
                ("SUPDOC_QUIET", "1"),
                ("SUPDOC_YES", "true"),
                ("SUPDOC_DRY_RUN", "no"),
            ],
        );

        assert!(
            parsed.contains("quiet: true")
                && parsed.contains("yes: true")
                && parsed.contains("dry_run: false"),
            "{parsed}"
        );
    }

    #[test]
    fn env_vars_fill_values_and_arguments_beat_them() {
        let from_env = parse_in_child(
            &["supdoc", "list"],
            &[
                ("SUPDOC_LIMIT", "7"),
                ("SUPDOC_STATE", "failed,new"),
                ("SUPDOC_OUTPUT", "github"),
                ("SUPDOC_COLOR", "never"),
                ("SUPDOC_CONFIG", "ci/supdoc.config.toml"),
                ("SUPDOC_VERBOSE", "2"),
            ],
        );
        let overridden = parse_in_child(
            &["supdoc", "list", "--limit", "9", "--output", "jsonl"],
            &[("SUPDOC_LIMIT", "7"), ("SUPDOC_OUTPUT", "github")],
        );

        assert!(
            from_env.contains("limit: 7")
                && from_env.contains("Failed, New")
                && from_env.contains("output: Github")
                && from_env.contains("color: Never")
                && from_env.contains("ci/supdoc.config.toml")
                && from_env.contains("verbose: 2"),
            "{from_env}"
        );
        assert!(
            overridden.contains("limit: 9") && overridden.contains("output: Jsonl"),
            "{overridden}"
        );
    }

    #[test]
    fn json_env_vars_select_format_by_precedence() {
        let json = parse_in_child(
            &["supdoc", "check"],
            &[("SUPDOC_JSON", "1"), ("SUPDOC_OUTPUT", "github")],
        );
        let jsonl = parse_in_child(
            &["supdoc", "check"],
            &[("SUPDOC_JSON", "1"), ("SUPDOC_JSONL", "1")],
        );
        let off = parse_in_child(&["supdoc", "check"], &[("SUPDOC_JSON", "0")]);

        assert!(
            json.starts_with("Json "),
            "SUPDOC_JSON beats SUPDOC_OUTPUT: {json}"
        );
        assert!(
            jsonl.starts_with("Jsonl "),
            "SUPDOC_JSONL beats SUPDOC_JSON: {jsonl}"
        );
        assert!(off.starts_with("Text "), "SUPDOC_JSON=0 is off: {off}");
    }

    #[test]
    fn global_defaults_are_text_auto_color_not_quiet() {
        let cli = parse_cli(&["list"]);

        assert_eq!(cli.global.format(), OutputFormat::Text);
        assert_eq!(cli.global.color, ColorChoice::Auto);
        assert!(!cli.global.quiet && cli.global.verbose == 0 && cli.global.config.is_none());
        assert!(matches!(
            cli.command,
            Command::List(ListArgs { limit: 100, .. })
        ));
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = parse_cli(&[
            "namespace",
            "list",
            "-vv",
            "-q",
            "-c",
            "x.toml",
            "--color",
            "always",
        ]);

        assert_eq!(cli.global.verbose, 2);
        assert!(cli.global.quiet);
        assert_eq!(
            cli.global.config.as_deref(),
            Some(std::path::Path::new("x.toml"))
        );
        assert_eq!(cli.global.color, ColorChoice::Always);
    }
}
