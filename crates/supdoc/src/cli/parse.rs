//! Turning the command line into a [`Cli`], or into the text to print and the exit code to use.
//!
//! Parsing never prints and never exits: [`parse_from`] returns what to show, so `main` is the only
//! code that touches the process and tests can inspect every outcome (06 `main`, X8).

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;

use clap::builder::{FalseyValueParser, TypedValueParser as _};
use clap::error::ErrorKind;
use clap::{CommandFactory as _, Parser, ValueEnum};

use super::CliError;
use super::args::{Cli, resolve_format};
use crate::output::OutputFormat;
use crate::output::json::FatalJson;

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
pub const USAGE_ERROR_CODE: &str = "cli-usage";

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
        Err(error) => Parsed::Print(printout(&error, &args, requested_format(&args, env))),
    }
}

/// Decides what to print for a clap error or an explicit `--help` / `--version`.
fn printout(error: &clap::Error, args: &[OsString], format: OutputFormat) -> Printout {
    let command = help_command(args);
    let text = redirect_flag_tip(&error.render().to_string(), &command);
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
            text: usage_error_json(&text, &command),
            exit_code: CliError::USAGE_EXIT_CODE,
        },
        _ => Printout {
            stream: Stream::Stderr,
            text: ensure_newline(text),
            exit_code: CliError::USAGE_EXIT_CODE,
        },
    }
}

/// The subcommand path the arguments name, such as `["namespace", "list"]`; empty when none parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HelpCommand(Vec<String>);

impl HelpCommand {
    /// Whether the command is `supdoc check`.
    fn is_check(&self) -> bool {
        self.0 == ["check"]
    }

    /// The command line that prints this command's help, such as `supdoc check --help`.
    fn help_invocation(&self) -> String {
        let mut line = String::from("supdoc");
        for word in &self.0 {
            line.push(' ');
            line.push_str(word);
        }
        line.push_str(" --help");
        line
    }
}

/// The deepest subcommand path the arguments name, read from the raw arguments.
///
/// Flags, flag values and unknown words are skipped; only words that are subcommands of the
/// command reached so far extend the path.
fn help_command(args: &[OsString]) -> HelpCommand {
    let mut command = Cli::command();
    let mut path = Vec::new();
    for word in args
        .iter()
        .skip(1)
        .filter_map(|arg| arg.to_str())
        .take_while(|arg| *arg != "--")
        .filter(|arg| !arg.starts_with('-'))
    {
        let Some(sub) = command.find_subcommand(word) else {
            continue;
        };
        path.push(sub.get_name().to_owned());
        command = sub.clone();
    }
    HelpCommand(path)
}

/// The leading whitespace of `line`.
fn indent_of(line: &str) -> &str {
    line.strip_suffix(line.trim_start()).unwrap_or_default()
}

/// Replaces clap's `-- --flag` tip, which only leads to a second error, with a tip that names a real flag.
///
/// `--format` points at `--output` and `--json`; any other unknown flag points at `--help`.
fn redirect_flag_tip(rendered: &str, command: &HelpCommand) -> String {
    let mut out = String::with_capacity(rendered.len());
    for line in rendered.split_inclusive('\n') {
        let body = line.trim();
        let flag = body
            .strip_prefix("tip: to pass '")
            .and_then(|rest| rest.split_once("' as a value, use '-- "))
            .map(|(flag, _)| flag);
        match flag {
            Some("--format") => {
                out.push_str(indent_of(line));
                out.push_str(
                    "tip: use `--output json` (or `--json`) to choose the output format\n",
                );
            }
            Some("--yes") if command.is_check() => {
                out.push_str(indent_of(line));
                out.push_str(
                    "tip: `check` only reads, so it takes no `--yes`; run `supdoc update --yes` to accept changes into supdoc.lock\n",
                );
            }
            Some(flag) => {
                out.push_str(indent_of(line));
                // Writing into a `String` cannot fail.
                let _ = writeln!(
                    out,
                    "tip: `{flag}` is not a flag of this command; run `{}` to see its flags",
                    command.help_invocation()
                );
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Appends a newline unless the text already ends in one.
fn ensure_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The JSON error object for a usage error (06 X8, 07 fatal error shape), on one line.
fn usage_error_json(rendered: &str, command: &HelpCommand) -> String {
    let (message, help) = split_clap_message(rendered, command);
    let error = FatalJson {
        error: true,
        kind: USAGE_ERROR_CODE.to_owned(),
        message: message.to_owned(),
        help: Some(help),
        exit_code: CliError::USAGE_EXIT_CODE,
        location: None,
    };
    // Serializing plain strings and numbers cannot fail; the fallback keeps stderr valid JSON anyway.
    let mut line = serde_json::to_string(&error).unwrap_or_else(|_| {
        r#"{"error":true,"kind":"cli-usage","message":"invalid command line","help":"run `supdoc --help`","exit_code":2}"#
            .to_owned()
    });
    line.push('\n');
    line
}

/// Splits clap's rendered error into the message and a help line.
///
/// The message is clap's first paragraph without its `error: ` prefix, which already lists valid
/// values. The help repeats clap's `tip:` suggestions and points at the parsed command's `--help`.
fn split_clap_message<'a>(rendered: &'a str, command: &HelpCommand) -> (&'a str, String) {
    let first_paragraph = rendered.split("\n\n").next().unwrap_or(rendered).trim();
    let message = first_paragraph
        .strip_prefix("error: ")
        .unwrap_or(first_paragraph);
    let tips: Vec<&str> = rendered
        .lines()
        .filter_map(|line| line.trim().strip_prefix("tip: "))
        .collect();
    let pointer = if command.0.is_empty() {
        "run `supdoc --help` to see valid usage".to_owned()
    } else {
        format!("run `{}` to see valid usage", command.help_invocation())
    };
    let help = if tips.is_empty() {
        pointer
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
    use crate::cli::args::{Command, Limit, ListArgs, SkillCommand, TargetArg};
    use crate::state::LinkState;

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

        assert_eq!(
            (defaults.limit, defaults.target.is_none()),
            (Limit::at_most(100), true)
        );
        assert_eq!(given.limit, Limit::UNLIMITED);
        assert_eq!(
            given.target.as_ref().map(TargetArg::typed),
            Some("auth:src/session.rs#login")
        );
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

        assert!(!leaves.is_empty(), "no leaf subcommands found");
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

    /// Every `after_help` block of the command tree, the top level included.
    fn after_help_blocks(command: &clap::Command, name: &str, blocks: &mut Vec<(String, String)>) {
        if let Some(after_help) = command.get_after_help() {
            blocks.push((name.to_owned(), after_help.to_string()));
        }
        for subcommand in command.get_subcommands() {
            if subcommand.get_name() != "help" {
                let path = format!("{name} {}", subcommand.get_name());
                after_help_blocks(subcommand, &path, blocks);
            }
        }
    }

    #[test]
    fn every_example_parses() {
        let mut blocks = Vec::new();
        after_help_blocks(&Cli::command(), "supdoc", &mut blocks);

        for (name, block) in &blocks {
            let lines: Vec<&str> = example_lines(block).collect();
            assert!(!lines.is_empty(), "`{name}` has no example lines: {block}");
            for line in lines {
                let invocation = line.split(" > ").next().unwrap_or(line);
                let args: Vec<&str> = invocation.split_whitespace().collect();
                assert!(
                    matches!(parse_from(args.iter().copied(), |_| None), Parsed::Run(_)),
                    "example `{line}` in the `{name}` help should parse"
                );
            }
        }
    }

    #[test]
    fn every_leaf_command_says_whether_it_reads_or_writes() {
        fn check(command: &clap::Command, name: &str) {
            for subcommand in command.get_subcommands() {
                if subcommand.get_name() == "help" {
                    continue;
                }
                let path = format!("{name} {}", subcommand.get_name());
                if subcommand.has_subcommands() {
                    check(subcommand, &path);
                    continue;
                }
                let about = subcommand
                    .get_about()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                assert!(
                    about.contains("read-only") || about.contains("writes"),
                    "`{path}` about should say `read-only` or `writes`: {about}"
                );
            }
        }
        check(&Cli::command(), "supdoc");
    }

    #[test]
    fn unknown_format_flag_tip_names_the_real_flag_not_a_double_dash_escape() {
        let printout = parse_printout(&["check", "--format", "json", "--json"]);

        assert!(
            printout.text.contains("`--output json`"),
            "tip should name `--output json`: {}",
            printout.text
        );
        assert!(
            !printout.text.contains("-- --format"),
            "tip should not suggest `-- --format`: {}",
            printout.text
        );
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
    fn usage_error_json_validates_against_the_published_error_schema() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../../schema/error.json"))
                .expect("schema/error.json should be JSON");
        let validator = jsonschema::validator_for(&schema)
            .expect("schema/error.json should be a valid JSON Schema");

        for args in [&["chek", "--json"][..], &["check", "--bogus", "--json"][..]] {
            let printout = parse_printout(args);
            let value: serde_json::Value =
                serde_json::from_str(&printout.text).expect("the usage error should be JSON");
            let violations: Vec<String> = validator
                .iter_errors(&value)
                .map(|error| error.to_string())
                .collect();

            assert!(
                violations.is_empty(),
                "`supdoc {}` usage error breaks schema/error.json: {violations:?} in {value}",
                args.join(" ")
            );
            assert!(
                value.get("location").is_none(),
                "a usage error has no location: {value}"
            );
        }
    }

    #[test]
    fn flag_tips_name_the_parsed_subcommand() {
        for (args, command) in [
            (&["check", "--bogus"][..], "supdoc check --help"),
            (
                &["-c", "x.toml", "update", "--bogus"][..],
                "supdoc update --help",
            ),
        ] {
            let printout = parse_printout(args);

            assert!(
                printout.text.contains(&format!("`{command}`")),
                "tip of `supdoc {}` should name `{command}`: {}",
                args.join(" "),
                printout.text
            );
            assert!(
                !printout.text.contains("<command>"),
                "tip of `supdoc {}` should not use a placeholder: {}",
                args.join(" "),
                printout.text
            );
        }
    }

    #[test]
    fn json_usage_error_help_names_the_parsed_subcommand() {
        let printout = parse_printout(&["check", "--bogus", "--json"]);
        let value: serde_json::Value =
            serde_json::from_str(&printout.text).expect("the usage error should be JSON");
        let help = value
            .get("help")
            .and_then(serde_json::Value::as_str)
            .expect("the usage error should have a help string");

        assert!(
            help.ends_with("run `supdoc check --help` to see valid usage"),
            "{help}"
        );
    }

    #[test]
    fn check_yes_tip_points_at_update() {
        let printout = parse_printout(&["check", "--yes"]);

        assert!(
            printout.text.contains("`check` only reads")
                && printout.text.contains("supdoc update --yes"),
            "check --yes should say check only reads and point at update: {}",
            printout.text
        );
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

    /// The values `child_probe` read out of the parsed command line.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct Probe {
        format: String,
        output: String,
        color: String,
        quiet: bool,
        verbose: u8,
        config: Option<String>,
        deny_warnings: Option<bool>,
        yes: Option<bool>,
        dry_run: Option<bool>,
        global: Option<bool>,
        limit: Option<String>,
        states: Option<Vec<String>>,
    }

    impl Probe {
        fn of(cli: &Cli) -> Self {
            let mut probe = Self {
                format: format!("{:?}", cli.global.format()),
                output: format!("{:?}", cli.global.output),
                color: format!("{:?}", cli.global.color),
                quiet: cli.global.quiet,
                verbose: cli.global.verbose,
                config: cli
                    .global
                    .config
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                deny_warnings: None,
                yes: None,
                dry_run: None,
                global: None,
                limit: None,
                states: None,
            };
            let states = |states: &[LinkState]| states.iter().map(|s| format!("{s:?}")).collect();
            match &cli.command {
                Command::Check(args) => probe.deny_warnings = Some(args.deny_warnings),
                Command::Update(args) => {
                    probe.yes = Some(args.confirm.yes);
                    probe.dry_run = Some(args.confirm.dry_run);
                    probe.states = Some(states(&args.state));
                }
                Command::List(args) => {
                    probe.limit = Some(args.limit.to_string());
                    probe.states = Some(states(&args.state));
                }
                Command::Skill(SkillCommand::Install(args)) => probe.global = Some(args.global),
                _ => {}
            }
            probe
        }
    }

    /// Not a test on its own: when re-run by `parse_in_child`, parses the arguments it was given
    /// and prints a [`Probe`] as JSON. Otherwise it does nothing. `parse_in_child` fails if the
    /// child printed no probe line, so a child that never ran cannot pass a test.
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
            Parsed::Run(cli) => {
                serde_json::to_string(&Probe::of(&cli)).expect("the probe should serialize")
            }
            Parsed::Print(printout) => {
                panic!("the probe arguments should parse: {}", printout.text)
            }
        };
        writeln!(std::io::stdout(), "PROBE {report}").expect("the probe should write to stdout");
    }

    /// Parses `args` in a child process whose environment is only `env` plus the unrelated
    /// variables the test harness needs. Fails unless the child ran `child_probe` and printed what it parsed.
    fn parse_in_child(args: &[&str], env: &[(&str, &str)]) -> Probe {
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
        assert!(
            output.status.success(),
            "the probe child failed ({}):\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let line = stdout
            .lines()
            .find_map(|line| line.split_once("PROBE ").map(|(_, rest)| rest))
            .unwrap_or_else(|| panic!("the child printed no probe line:\n{stdout}"));
        serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("the probe line should be JSON ({error}): {line}"))
    }

    #[test]
    fn falsey_env_values_turn_flags_off() {
        for value in ["0", "false", "no", "off", "FALSE"] {
            let parsed = parse_in_child(
                &["supdoc", "check"],
                &[("SUPDOC_QUIET", value), ("SUPDOC_DENY_WARNINGS", value)],
            );

            assert!(
                !parsed.quiet && parsed.deny_warnings == Some(false),
                "SUPDOC_QUIET={value} should turn the flag off: {parsed:?}"
            );
        }
    }

    #[test]
    fn skill_install_takes_no_positional_and_accepts_global_flags() {
        let printout = parse_printout(&["skill", "install", "claude"]);
        assert_eq!(printout.exit_code, 2, "{}", printout.text);
        for flag in ["-g", "--global"] {
            let parsed = parse_in_child(&["supdoc", "skill", "install", flag], &[]);
            assert_eq!(parsed.global, Some(true), "{flag}: {parsed:?}");
        }
        let plain = parse_in_child(&["supdoc", "skill", "install"], &[]);
        assert_eq!(plain.global, Some(false), "{plain:?}");
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
            assert_eq!(
                parsed.global,
                Some(expected),
                "SUPDOC_GLOBAL={value:?} should give global: {expected}: {parsed:?}"
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
            parsed.quiet && parsed.yes == Some(true) && parsed.dry_run == Some(false),
            "{parsed:?}"
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

        assert_eq!(from_env.limit.as_deref(), Some("7"), "{from_env:?}");
        assert_eq!(
            from_env.states,
            Some(vec!["Failed".to_owned(), "New".to_owned()]),
            "{from_env:?}"
        );
        assert_eq!(from_env.output, "Github", "{from_env:?}");
        assert_eq!(from_env.color, "Never", "{from_env:?}");
        assert_eq!(
            from_env.config.as_deref(),
            Some("ci/supdoc.config.toml"),
            "{from_env:?}"
        );
        assert_eq!(from_env.verbose, 2, "{from_env:?}");
        assert_eq!(overridden.limit.as_deref(), Some("9"), "{overridden:?}");
        assert_eq!(overridden.output, "Jsonl", "{overridden:?}");
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

        assert_eq!(
            json.format, "Json",
            "SUPDOC_JSON beats SUPDOC_OUTPUT: {json:?}"
        );
        assert_eq!(
            jsonl.format, "Jsonl",
            "SUPDOC_JSONL beats SUPDOC_JSON: {jsonl:?}"
        );
        assert_eq!(off.format, "Text", "SUPDOC_JSON=0 is off: {off:?}");
    }

    #[test]
    fn global_defaults_are_text_auto_color_not_quiet() {
        let cli = parse_cli(&["list"]);

        assert_eq!(cli.global.format(), OutputFormat::Text);
        assert_eq!(cli.global.color, ColorChoice::Auto);
        assert!(!cli.global.quiet && cli.global.verbose == 0 && cli.global.config.is_none());
        assert!(matches!(
            cli.command,
            Command::List(ListArgs { limit, .. }) if limit == Limit::at_most(100)
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
