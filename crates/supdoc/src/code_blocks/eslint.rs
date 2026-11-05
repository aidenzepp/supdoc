//! The reader of eslint's output: its `--format json` report, and the comments in a block that
//! switch eslint off for all of it.
//!
//! Probed against eslint 10.14.0 (`ci/node-tools`), with an `eslint.config.js` setting `no-var`
//! and `eqeqeq`: `printf 'var a = 1;\nif (a == 2) {}\n' | eslint --stdin --stdin-filename
//! /p/doc.md.1.js --format json` exits 1 and prints
//!
//! ```text
//! [{"filePath":"/p/doc.md.1.js","messages":[
//!   {"ruleId":"no-var","severity":2,"message":"Unexpected var, use let or const instead.",
//!    "line":1,"column":1,"messageId":"unexpectedVar","endLine":1,"endColumn":11,...},
//!   {"ruleId":"eqeqeq","severity":1,"message":"Expected '===' and instead saw '=='.",
//!    "line":2,"column":7,...}],
//!  "suppressedMessages":[],"errorCount":1,"fatalErrorCount":0,"warningCount":1,...}]
//! ```
//!
//! Its lines and columns are 1-based. A block that doesn't parse gives one message with
//! `"ruleId":null,"fatal":true` (`Parsing error: Unexpected token =`), exit 1. A stdin path the
//! config ignores, or that no config object's `files` match, gives one message with
//! `"ruleId":null,"fatal":false,"severity":1` and no line, `File ignored because of a matching
//! ignore pattern. ...` or `File ignored because no matching configuration was supplied.`, exit
//! 0. A missing or broken config prints a banner (`Oops! Something went wrong! :(`, `ESLint:
//! 10.14.0`) and then its cause to stderr, nothing to stdout, and exits 2. A `console.log` in the
//! project's config or a plugin prints to stdout before the report, at any exit, so stdout that
//! isn't exactly one file's result is never read as one.
//!
//! eslint lints a file that config objects match but that no rule applies to, reporting nothing.
//! supdoc asks `eslint --print-config <virtual path>` first (see [`rules_apply`]), which prints
//! the resolved config as JSON, its `rules` normalized to arrays led by a severity (`{"no-var":
//! [2], "eqeqeq": [1, "always"]}`), or `undefined` for a path the config ignores. A plugin
//! processor that turns a block into no code at all still lints nothing and reports nothing: that
//! residual can't be seen from the config.

use std::path::Path;
use std::time::Duration;

use super::format;
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;

/// The start of the message eslint gives instead of linting a file its config leaves out.
const IGNORED: &str = "File ignored";

/// The rules eslint's findings drop, which a snippet breaks for lack of context: unused names,
/// in core eslint and in typescript-eslint. A `--rule` flag can't turn them off: naming the
/// plugin's rule where the project doesn't load the plugin makes eslint fail.
const RELAXED: [&str; 2] = ["no-unused-vars", "@typescript-eslint/no-unused-vars"];

/// The rule name supdoc gives a message eslint names no rule for, such as an unused
/// `eslint-disable` comment.
const UNNAMED: &str = "eslint";

/// The keyword that starts each comment switching eslint's rules off.
const DISABLE: &str = "eslint-disable";

/// The verdict on a run eslint failed in without reporting a message.
const FAILED_SILENTLY: &str = "eslint exited non-zero and reported no message";

/// The verdict on a run whose stdout isn't eslint's report on the one block.
const NOT_REPORT: &str = "its output wasn't its JSON report on the block, so supdoc can't read \
                          what it found; something else printed to stdout, such as a \
                          `console.log` in the project's eslint config or a plugin";

/// The verdict on a `--print-config` run whose stdout isn't the block's config.
const NOT_CONFIG: &str = "`eslint --print-config` printed something other than the block's \
                          config as JSON, so supdoc can't tell which rules apply to it; something \
                          else printed to stdout, such as a `console.log` in the project's eslint \
                          config or a plugin";

/// The lines eslint prints before the cause of a failure.
const BANNER: [&str; 2] = ["Oops! Something went wrong!", "ESLint: "];

/// Whether `setting`, a rule's in a printed config, sets it to warn or error: a severity `1`,
/// `2`, `"warn"` or `"error"`, alone or leading an array.
fn enabled(setting: &serde_json::Value) -> bool {
    let severity = setting
        .as_array()
        .map_or(Some(setting), |array| array.first());
    match severity {
        Some(serde_json::Value::Number(level)) => level.as_u64().is_some_and(|level| level > 0),
        Some(serde_json::Value::String(level)) => {
            matches!(level.as_str(), "warn" | "error" | "1" | "2")
        }
        _ => false,
    }
}

/// Whether any rule of the project's eslint config applies to the block file at `path`, its
/// exact virtual path, asked of `program` with `--print-config` from `root` with `env` under
/// `timeout`. Asked afresh for each block: a config's `files` can name one doc and not its
/// sibling, so no block's answer stands for another's. A rule supdoc's reader drops (see
/// [`RELAXED`]) doesn't count. A path the config ignores answers `true`: the lint run reports it
/// ignored (see [`linted_nothing`]).
///
/// # Errors
///
/// The failure of a run that timed out, failed or printed anything but the config: a tool
/// failure in supdoc's words, as the lint step reports it.
///
/// Effect: runs the binary.
pub(crate) fn rules_apply(
    program: &Path,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<Duration>,
    path: &Path,
) -> Result<bool, format::Verdict> {
    let call = runner::Call {
        program,
        name: "eslint",
        args: &[std::ffi::OsStr::new("--print-config"), path.as_os_str()],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Root,
        cap: runner::CAP,
    };
    let ran = runner::run(&call, env, root).map_err(format::failed)?;
    let not_config = || format::Verdict::ToolFailed(NOT_CONFIG.to_owned());
    if !ran.success {
        return Err(cause(&ran.stderr).map_or_else(not_config, format::Verdict::ToolFailed));
    }
    applies(&ran.stdout).ok_or_else(not_config)
}

/// Whether the config `--print-config` printed in `stdout` sets a rule supdoc keeps (one not in
/// [`RELAXED`]) to warn or error; `true` for `undefined`, eslint's answer for an ignored path.
/// `None` when stdout is neither.
fn applies(stdout: &[u8]) -> Option<bool> {
    let printed = String::from_utf8_lossy(stdout);
    if printed.trim() == "undefined" {
        return Some(true);
    }
    // A JSON object, its `rules` mapping each rule to its setting, as eslint normalizes it.
    let printed: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&printed).ok()?;
    let rules = match printed.get("rules") {
        None => return Some(false),
        Some(rules) => rules.as_object()?,
    };
    Some(
        rules
            .iter()
            .any(|(rule, setting)| !RELAXED.contains(&rule.as_str()) && enabled(setting)),
    )
}

/// The cause eslint printed for a failure in `stderr`: its first lines after the banner (see
/// [`BANNER`]), without ANSI escapes. `None` when nothing follows the banner.
fn cause(stderr: &[u8]) -> Option<String> {
    let stderr = format::strip_ansi(&String::from_utf8_lossy(stderr));
    let rest: Vec<&str> = stderr
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty() && !BANNER.iter().any(|banner| line.starts_with(banner))
        })
        .collect();
    if rest.is_empty() {
        return None;
    }
    Some(format::exit_detail(rest.join("\n").as_bytes()))
}

/// What supdoc says about a run whose stdout isn't eslint's report: the cause eslint printed for
/// a failure (see [`cause`]), else that its output wasn't the report.
fn unread(ran: &Ran) -> String {
    (!ran.success)
        .then(|| cause(&ran.stderr))
        .flatten()
        .unwrap_or_else(|| NOT_REPORT.to_owned())
}

/// One file's result in eslint's `--format json` report.
#[derive(serde::Deserialize)]
struct FileResult {
    /// Each message, in eslint's order.
    messages: Vec<Message>,
}

/// One message of eslint's report.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    /// The rule's name, such as `no-var`. `None` for a parse error, an ignored file or an unused
    /// `eslint-disable` comment.
    rule_id: Option<String>,
    /// Whether the text didn't parse.
    #[serde(default)]
    fatal: bool,
    /// What eslint says.
    #[serde(rename = "message")]
    text: String,
    /// Its 1-based line. `None` for an ignored file.
    line: Option<usize>,
    /// Its 1-based column, counted in characters.
    column: Option<usize>,
    /// Its 1-based last line, when eslint says.
    end_line: Option<usize>,
}

impl Message {
    /// Whether it is eslint's word that it linted nothing: the config ignores the path, or no
    /// config object matches it.
    fn says_ignored(&self) -> bool {
        self.rule_id.is_none() && !self.fatal && self.text.starts_with(IGNORED)
    }

    /// Whether its rule is one supdoc relaxes for a snippet (see [`RELAXED`]).
    fn relaxed(&self) -> bool {
        self.rule_id
            .as_deref()
            .is_some_and(|rule| RELAXED.contains(&rule))
    }

    /// It as a diagnostic: line 1 and column 1 when eslint gives none, its end line its line when
    /// eslint gives none, and a fatal message the only syntax error.
    fn diagnostic(self) -> Diagnostic {
        let line = self.line.unwrap_or(1);
        Diagnostic {
            line,
            column: self.column.unwrap_or(1),
            end_line: self.end_line.unwrap_or(line),
            syntax: self.fatal,
            rule: self.rule_id.unwrap_or_else(|| UNNAMED.to_owned()),
            message: self.text,
        }
    }
}

/// The messages of the report in `stdout`. `None` unless stdout is exactly eslint's `json`
/// report on one file, as `--stdin` gives: anything printed besides it, no result or more than
/// one, a result without `messages`, or JSON nested past `serde_json`'s depth limit.
fn messages(stdout: &[u8]) -> Option<Vec<Message>> {
    let results: Vec<FileResult> = serde_json::from_slice(stdout).ok()?;
    let [result] = <[FileResult; 1]>::try_from(results).ok()?;
    Some(result.messages)
}

/// Whether eslint's report in `stdout` says it ignored the block, so the run says nothing about
/// it.
#[must_use]
pub(crate) fn linted_nothing(stdout: &[u8]) -> bool {
    messages(stdout).is_some_and(|messages| messages.iter().any(Message::says_ignored))
}

/// Judges an eslint run. A fatal message is a parse failure at its line, as a formatter's is;
/// every other message, warning or error, is a diagnostic, but for the relaxed rules (see
/// [`RELAXED`]); a report with none left is clean whatever the exit. A non-zero exit with no
/// message at all is a tool failure, as is stdout that isn't exactly the report (see
/// [`messages`]) whatever the exit, quoting eslint's cause for a failure (a config error exits 2
/// with nothing on stdout) and otherwise saying the output wasn't the report. eslint checks no
/// fragments, so no run is framed.
#[must_use]
pub(crate) fn lint_verdict(
    _input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    let ran = match result {
        Ok(ran) => ran,
        Err(failure) => return lint::judge(Err(failure), |_| None),
    };
    let Some(messages) = messages(&ran.stdout) else {
        return lint::Verdict::Failed(format::Verdict::ToolFailed(unread(&ran)));
    };
    if messages.is_empty() && !ran.success {
        return lint::Verdict::Failed(format::Verdict::ToolFailed(FAILED_SILENTLY.to_owned()));
    }
    let found: Vec<Diagnostic> = messages
        .into_iter()
        .filter(|message| !message.relaxed())
        .map(Message::diagnostic)
        .collect();
    if let Some(syntax) = found.iter().find(|found| found.syntax) {
        return lint::Verdict::Failed(format::Verdict::ParseFailed {
            detail: syntax.message.clone(),
            line: syntax.line.checked_sub(1),
        });
    }
    if found.is_empty() {
        lint::Verdict::Clean
    } else {
        lint::Verdict::Diagnostics(found)
    }
}

/// Whether `block` holds an `eslint-disable` comment that may switch off more than the rules it
/// names, so supdoc can't confirm eslint lints all of it: one naming no rule (`/* eslint-disable
/// */`, `// eslint-disable-next-line -- why`), or one whose list holds anything but rule names.
/// supdoc's own `lint(off: ...)` is the way to switch the step off.
///
/// The scan is conservative: it reads text, not code, in any case, so such a comment inside a
/// string counts, as does one scoped to a line and a near miss such as `eslint-disabled`. It reads
/// from the keyword up to the comment's end (`*/` or the line's end), dropping a `--`
/// description.
#[must_use]
pub(crate) fn switches_lint_off(block: &str) -> bool {
    let lower = block.to_ascii_lowercase();
    lower.match_indices(DISABLE).any(|(at, keyword)| {
        let rest = lower.get(at + keyword.len()..).unwrap_or_default();
        let rest = rest
            .strip_prefix("-next-line")
            .or_else(|| rest.strip_prefix("-line"))
            .unwrap_or(rest);
        let comment = rest.split(['\n', '\r']).next().unwrap_or_default();
        let comment = comment.split("*/").next().unwrap_or_default();
        let rules = comment.split("--").next().unwrap_or_default();
        !names_rules(rules)
    })
}

/// Whether `list`, what follows an `eslint-disable` keyword, starts with whitespace and is a
/// comma-separated list of rule names (`no-var`, `@typescript-eslint/no-explicit-any`).
fn names_rules(list: &str) -> bool {
    list.starts_with(char::is_whitespace)
        && list.split(',').all(|rule| {
            let rule = rule.trim();
            !rule.is_empty()
                && rule
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_/@.".contains(c))
        })
}

#[cfg(test)]
mod tests {
    use super::{
        FAILED_SILENTLY, NOT_REPORT, applies, cause, enabled, lint_verdict, linted_nothing,
        switches_lint_off,
    };
    use crate::code_blocks::format;
    use crate::code_blocks::lint::{Diagnostic, Verdict};
    use crate::code_blocks::runner::Ran;

    /// eslint's `json` report on one file holding `messages`.
    fn report(messages: &str) -> Vec<u8> {
        format!(
            r#"[{{"filePath":"/p/doc.md.1.js","messages":[{messages}],"suppressedMessages":[],"errorCount":0,"fatalErrorCount":0,"warningCount":0,"fixableErrorCount":0,"fixableWarningCount":0,"usedDeprecatedRules":[]}}]"#
        )
        .into_bytes()
    }

    /// A message breaking `rule` at `at`, as eslint 10.14.0 prints one, of `severity` 1 or 2.
    fn message(rule: &str, severity: u8, text: &str, at: (usize, usize)) -> String {
        format!(
            r#"{{"ruleId":"{rule}","severity":{severity},"message":"{text}","line":{},"column":{},"messageId":"x","endLine":{},"endColumn":11}}"#,
            at.0, at.1, at.0
        )
    }

    /// A finished run that exited `code`, printed `stdout` and nothing to stderr.
    fn ran_quietly(code: i32, stdout: Vec<u8>) -> Ran {
        Ran {
            success: code == 0,
            stdout,
            stderr: Vec::new(),
        }
    }

    /// A finished run that exited `code` and printed `stdout`.
    fn ran(code: i32, stdout: Vec<u8>) -> Ran {
        Ran {
            success: code == 0,
            stdout,
            stderr: b"eslint's stderr".to_vec(),
        }
    }

    #[test]
    fn every_message_warning_or_error_is_a_diagnostic_at_its_one_based_place() {
        let stdout = report(&format!(
            "{},{}",
            message("no-var", 2, "Unexpected var.", (1, 1)),
            message("eqeqeq", 1, "Expected '==='.", (2, 7))
        ));

        assert_eq!(
            lint_verdict("", false, Ok(ran(1, stdout))),
            Verdict::Diagnostics(vec![
                Diagnostic {
                    line: 1,
                    column: 1,
                    end_line: 1,
                    syntax: false,
                    rule: "no-var".to_owned(),
                    message: "Unexpected var.".to_owned(),
                },
                Diagnostic {
                    line: 2,
                    column: 7,
                    end_line: 2,
                    syntax: false,
                    rule: "eqeqeq".to_owned(),
                    message: "Expected '==='.".to_owned(),
                },
            ])
        );
    }

    #[test]
    fn a_warning_alone_fails_the_block_though_eslint_exits_zero() {
        let stdout = report(&message("eqeqeq", 1, "Expected '==='.", (2, 7)));

        assert!(
            matches!(lint_verdict("", false, Ok(ran(0, stdout))), Verdict::Diagnostics(found) if found.len() == 1)
        );
    }

    #[test]
    fn a_fatal_message_is_a_parse_failure_at_its_body_line() {
        let stdout = report(
            r#"{"ruleId":null,"fatal":true,"severity":2,"message":"Parsing error: Unexpected token =","line":3,"column":7}"#,
        );

        assert_eq!(
            lint_verdict("", false, Ok(ran(1, stdout))),
            Verdict::Failed(format::Verdict::ParseFailed {
                detail: "Parsing error: Unexpected token =".to_owned(),
                line: Some(2),
            })
        );
    }

    #[test]
    fn the_unused_names_rules_are_dropped_so_alone_they_leave_the_block_clean() {
        let stdout = report(&format!(
            "{},{}",
            message(
                "no-unused-vars",
                2,
                "'b' is defined but never used.",
                (1, 5)
            ),
            message(
                "@typescript-eslint/no-unused-vars",
                2,
                "'c' is defined but never used.",
                (2, 5)
            )
        ));

        assert_eq!(lint_verdict("", false, Ok(ran(1, stdout))), Verdict::Clean);
    }

    #[test]
    fn a_message_naming_no_rule_is_a_diagnostic_named_for_eslint() {
        let stdout = report(
            r#"{"ruleId":null,"message":"Unused eslint-disable directive (no problems were reported from 'no-var').","line":1,"column":12,"severity":2}"#,
        );

        let verdict = lint_verdict("", false, Ok(ran(1, stdout)));

        assert!(
            matches!(&verdict, Verdict::Diagnostics(found) if found.first().is_some_and(|found| found.rule == "eslint" && !found.syntax)),
            "{verdict:?}"
        );
    }

    #[test]
    fn output_that_isnt_the_report_or_a_silent_non_zero_exit_is_a_tool_failure() {
        assert_eq!(
            lint_verdict(
                "",
                false,
                Ok(ran(2, b"Oops! Something went wrong!".to_vec()))
            ),
            Verdict::Failed(format::Verdict::ToolFailed("eslint's stderr".to_owned()))
        );
        assert_eq!(
            lint_verdict("", false, Ok(ran(1, report("")))),
            Verdict::Failed(format::Verdict::ToolFailed(FAILED_SILENTLY.to_owned()))
        );
        assert_eq!(
            lint_verdict("", false, Ok(ran(0, report("")))),
            Verdict::Clean
        );
    }

    #[test]
    fn stdout_that_isnt_exactly_one_files_report_fails_whatever_the_exit() {
        let warning = String::from_utf8(report(&message("eqeqeq", 1, "Expected '==='.", (2, 7))))
            .expect("the report is UTF-8");
        let ignored = String::from_utf8(report(
            r#"{"ruleId":null,"fatal":false,"severity":1,"message":"File ignored because of a matching ignore pattern."}"#,
        ))
        .expect("the report is UTF-8");
        let one = warning
            .strip_prefix('[')
            .and_then(|it| it.strip_suffix(']'))
            .expect("the report is an array");
        for stdout in [
            format!("config loaded\n{warning}"),
            format!("config loaded\n{ignored}"),
            format!("{warning}\nrule ran"),
            "not json".to_owned(),
            "[]".to_owned(),
            r#"[{"filePath":"x"}]"#.to_owned(),
            format!("[{one},{one}]"),
            format!("{}{}", "[".repeat(100_000), "]".repeat(100_000)),
        ] {
            for code in [0, 1] {
                assert_eq!(
                    lint_verdict(
                        "",
                        false,
                        Ok(ran_quietly(code, stdout.clone().into_bytes()))
                    ),
                    Verdict::Failed(format::Verdict::ToolFailed(NOT_REPORT.to_owned())),
                    "exit {code}: {}",
                    stdout.get(..60).unwrap_or(&stdout)
                );
            }
            assert!(
                !linted_nothing(stdout.as_bytes()),
                "{}",
                stdout.get(..60).unwrap_or(&stdout)
            );
        }
    }

    #[test]
    fn a_failures_cause_is_quoted_without_eslints_banner() {
        let stderr = b"\nOops! Something went wrong! :(\n\nESLint: 10.14.0\n\n\x1b[31mTypeError: Key \"rules\": Key \"nope\": Could not find \"nope\" in plugin \"@\".\x1b[0m\n    at throwRuleNotFoundError (config.js:119:16)\n";

        assert_eq!(
            cause(stderr).as_deref(),
            Some(
                "TypeError: Key \"rules\": Key \"nope\": Could not find \"nope\" in plugin \"@\".\n    at throwRuleNotFoundError (config.js:119:16)"
            )
        );
        assert_eq!(
            cause(b"\nOops! Something went wrong! :(\n\nESLint: 10.14.0\n"),
            None
        );
        assert_eq!(
            lint_verdict(
                "",
                false,
                Ok(Ran {
                    success: false,
                    stdout: Vec::new(),
                    stderr: stderr.to_vec(),
                })
            ),
            Verdict::Failed(format::Verdict::ToolFailed(
                cause(stderr).expect("the stderr names a cause")
            ))
        );
    }

    #[test]
    fn a_rule_set_to_warn_or_error_is_enabled_and_off_is_not() {
        for (setting, on) in [
            ("[2]", true),
            ("[1, \"always\"]", true),
            ("\"error\"", true),
            ("[\"warn\"]", true),
            ("2", true),
            ("[0, {\"typeof\": false}]", false),
            ("\"off\"", false),
            ("0", false),
            ("[]", false),
            ("{}", false),
        ] {
            let value: serde_json::Value =
                serde_json::from_str(setting).expect("the fixture setting is JSON");
            assert_eq!(enabled(&value), on, "{setting}");
        }
    }

    #[test]
    fn a_printed_config_applies_only_with_a_kept_rule_set_to_warn_or_error() {
        for (printed, expected) in [
            (r#"{"rules":{"no-var":[2]}}"#, Some(true)),
            (
                r#"{"rules":{"eqeqeq":[1,"always"],"semi":[0]}}"#,
                Some(true),
            ),
            (r#"{"rules":{}}"#, Some(false)),
            (r#"{"language":"@/js"}"#, Some(false)),
            (r#"{"rules":{"semi":[0],"no-undef":"off"}}"#, Some(false)),
            (r#"{"rules":{"no-unused-vars":[2]}}"#, Some(false)),
            (
                r#"{"rules":{"no-unused-vars":[2],"@typescript-eslint/no-unused-vars":["warn"]}}"#,
                Some(false),
            ),
            ("undefined\n", Some(true)),
            ("config loaded\n{\"rules\":{\"no-var\":[2]}}", None),
            ("[]", None),
        ] {
            assert_eq!(applies(printed.as_bytes()), expected, "{printed}");
        }
    }

    #[test]
    fn a_file_ignored_message_linted_nothing() {
        for said in [
            r#"File ignored because of a matching ignore pattern. Use \"--no-ignore\" to disable file ignore settings or use \"--no-warn-ignored\" to suppress this warning."#,
            "File ignored because no matching configuration was supplied.",
        ] {
            let stdout = report(&format!(
                r#"{{"ruleId":null,"fatal":false,"severity":1,"message":"{said}"}}"#
            ));
            assert!(linted_nothing(&stdout), "{said}");
        }
        assert!(!linted_nothing(&report(&message("no-var", 2, "x", (1, 1)))));
        assert!(!linted_nothing(&report("")));
        assert!(
            !linted_nothing(b"not json"),
            "unreadable output is judged, not skipped"
        );
    }

    #[test]
    fn a_disable_naming_no_rule_or_more_than_rule_names_switches_lint_off() {
        for block in [
            "/* eslint-disable */\nvar a = 1;",
            "/*eslint-disable*/",
            "var a = 1; // eslint-disable-line",
            "// eslint-disable-next-line -- the reason\nvar a = 1;",
            "/* eslint-disable-line */ var a = 1;",
            "/* ESLINT-DISABLE */",
            "/* eslint-disable no-var, */",
            "/* eslint-disable no-var eqeqeq */",
            "/* eslint-disabled */",
            "const s = '/* eslint-disable */';",
            "/* eslint-disable no-var */ /* eslint-disable */",
            "/* eslint-disable\n */",
        ] {
            assert!(switches_lint_off(block), "{block:?} should switch lint off");
        }
    }

    #[test]
    fn a_disable_naming_rules_or_a_mention_of_eslint_leaves_lint_on() {
        for block in [
            "/* eslint-disable no-var */\nvar a = 1;",
            "var a = 1; // eslint-disable-line no-var -- why",
            "// eslint-disable-next-line no-var, @typescript-eslint/no-explicit-any\nvar a = 1;",
            "/* eslint no-var: off */",
            "// run eslint on this file\nconst a = 1;",
            "const a = 1;",
        ] {
            assert!(!switches_lint_off(block), "{block:?} should leave lint on");
        }
    }
}
