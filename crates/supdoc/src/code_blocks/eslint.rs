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
//! 0. A missing or broken config prints a stack trace to stderr, nothing to stdout, and exits 2.

use super::format;
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

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

/// Every message in the report in `stdout`. `None` when it isn't eslint's `json` report.
fn messages(stdout: &[u8]) -> Option<Vec<Message>> {
    let results: Vec<FileResult> = serde_json::from_slice(stdout).ok()?;
    Some(
        results
            .into_iter()
            .flat_map(|result| result.messages)
            .collect(),
    )
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
/// message at all is a tool failure, and output that isn't the report is judged as
/// [`lint::judge`] judges it: a config error exits 2 with nothing on stdout. eslint checks no
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
        return lint::judge(Ok(ran), |_| None);
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
    use super::{FAILED_SILENTLY, lint_verdict, linted_nothing, switches_lint_off};
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
