//! The reader of pylint's output: its `--output-format=json2` report, and the comments in a block
//! that switch pylint off for the whole block.
//!
//! Probed against pylint 4.1.3 (`ci/requirements-pylint.txt`): `printf 'x = 1\nif x == None:\n
//! pass\n' | pylint --from-stdin /p/doc.md.1.py --output-format=json2` prints
//!
//! ```text
//! {"messages": [{"type": "convention", "symbol": "invalid-name",
//!   "message": "Module name \"1\" doesn't conform to snake_case naming style",
//!   "messageId": "C0103", "confidence": "HIGH", "module": "doc.md.1", "obj": "",
//!   "line": 1, "column": 0, "endLine": null, "endColumn": null, "path": "doc.md.1.py",
//!   "absolutePath": "/p/doc.md.1.py"},
//!  {"type": "convention", "symbol": "singleton-comparison",
//!   "message": "Comparison 'x == None' should be 'x is None'", "messageId": "C0121", ...,
//!   "line": 2, "column": 3, "endLine": 2, "endColumn": 12, ...}],
//!  "statistics": {"messageTypeCount": {"fatal": 0, "error": 0, ...}, "modulesLinted": 3,
//!   "score": 2.5}}
//! ```
//!
//! and exits 16. Its lines are 1-based and its columns 0-based. A syntax error is the message
//! `syntax-error` (`E0001`), of type `error`.

use super::format;
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

/// pylint's `--output-format=json2` report.
#[derive(serde::Deserialize)]
struct Report {
    /// Each message, in pylint's order.
    messages: Vec<Entry>,
    /// What the run covered.
    statistics: Statistics,
}

/// One message of pylint's report.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    /// `fatal`, `error`, `warning`, `refactor`, `convention` or `info`.
    #[serde(rename = "type")]
    kind: String,
    /// The rule's name, such as `syntax-error`.
    symbol: String,
    /// The rule's code, such as `E0001`.
    message_id: String,
    /// What pylint says.
    message: String,
    /// Its 1-based line.
    line: usize,
    /// Its 0-based column, counted in characters.
    column: usize,
    /// Its 1-based last line, when pylint says.
    end_line: Option<usize>,
}

/// What a run of pylint covered.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Statistics {
    /// How many modules it linted: none when the project's config left the block out.
    modules_linted: usize,
}

impl Entry {
    /// Whether it is about the module name pylint derives from the block's virtual path
    /// (`<doc>.<n>.py`, so a module named `1`), which supdoc chose and no author can change.
    fn names_the_virtual_module(&self) -> bool {
        self.symbol == "invalid-name" && self.message.starts_with("Module name \"")
    }

    /// It as a diagnostic: its column made 1-based, its end line its line when pylint gives
    /// none, and `syntax-error` the only syntax error.
    fn diagnostic(self) -> Diagnostic {
        Diagnostic {
            line: self.line,
            column: self.column + 1,
            end_line: self.end_line.unwrap_or(self.line),
            syntax: self.symbol == "syntax-error",
            rule: self.message_id,
            message: self.message,
        }
    }
}

/// The report in `stdout`. `None` when it isn't pylint's `json2` report.
fn report(stdout: &[u8]) -> Option<Report> {
    serde_json::from_slice(stdout).ok()
}

/// The diagnostics in pylint's `json2` report in `stdout`, without the message about the
/// virtual module's name. `None` when it isn't that report, or when the report holds a `fatal`
/// message: pylint itself failed.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let report = report(stdout)?;
    if report
        .messages
        .iter()
        .any(|message| message.kind == "fatal")
    {
        return None;
    }
    Some(
        report
            .messages
            .into_iter()
            .filter(|message| !message.names_the_virtual_module())
            .map(Entry::diagnostic)
            .collect(),
    )
}

/// Whether pylint's report in `stdout` says it linted no module, so the run says nothing about
/// the block.
#[must_use]
pub(crate) fn linted_nothing(stdout: &[u8]) -> bool {
    report(stdout).is_some_and(|report| report.statistics.modules_linted == 0)
}

/// Judges a pylint run: a `syntax-error` is a parse failure at its line, as a formatter's is; a
/// report with no diagnostics left is clean whatever the exit, since pylint exits non-zero for
/// the virtual module's name alone; a report with diagnostics gives them; anything else is
/// judged as [`lint::judge`] judges output it can't read. pylint checks no fragments, so no run
/// is framed.
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
    let Some(found) = diagnostics(&ran.stdout) else {
        return lint::judge(Ok(ran), |_| None);
    };
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

/// Whether `block` holds a pylint comment that switches every message off for the whole block:
/// `skip-file`, `disable-all` or `all` among a `pylint:` comment's words. supdoc's own
/// `lint(off: ...)` is the way to switch the step off.
///
/// The scan is conservative, so a near miss counts as switching the step off, never as a pass:
/// it reads text, not Python, so such a comment inside a string counts, as does `enable=all` and
/// one scoped to a line or a block, and it finds a comment wherever a `#` starts it, as pylint
/// finds `pylint:` anywhere after one.
#[must_use]
pub(crate) fn switches_lint_off(block: &str) -> bool {
    block.split('\n').any(|line| {
        line.match_indices('#').any(|(at, _)| {
            let comment = line.get(at + 1..).unwrap_or_default();
            pragmas(comment).any(|words| {
                words
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                    .any(|word| {
                        ["all", "skip-file", "disable-all"]
                            .iter()
                            .any(|off| word.eq_ignore_ascii_case(off))
                    })
            })
        })
    })
}

/// The text after each `pylint:` in `comment`, with any space before the colon.
fn pragmas(comment: &str) -> impl Iterator<Item = &str> {
    comment
        .match_indices("pylint")
        .filter_map(move |(at, name)| {
            comment
                .get(at + name.len()..)?
                .trim_start()
                .strip_prefix(':')
        })
}

#[cfg(test)]
mod tests {
    use super::{diagnostics, lint_verdict, linted_nothing, switches_lint_off};
    use crate::code_blocks::format;
    use crate::code_blocks::lint::{Diagnostic, Verdict};
    use crate::code_blocks::runner::Ran;

    /// A pylint `json2` report holding `messages` that linted `modules` modules.
    fn report(messages: &str, modules: usize) -> Vec<u8> {
        format!(
            r#"{{"messages": [{messages}], "statistics": {{"messageTypeCount": {{"fatal": 0}}, "modulesLinted": {modules}, "score": 0}}}}"#
        )
        .into_bytes()
    }

    /// A message as pylint 4.1.3 prints it.
    fn message(kind: &str, symbol: &str, id: &str, text: &str, at: (usize, usize)) -> String {
        format!(
            r#"{{"type": "{kind}", "symbol": "{symbol}", "message": "{text}", "messageId": "{id}", "confidence": "HIGH", "module": "doc.md.1", "obj": "", "line": {}, "column": {}, "endLine": null, "endColumn": null, "path": "doc.md.1.py", "absolutePath": "/p/doc.md.1.py"}}"#,
            at.0, at.1
        )
    }

    /// The message pylint gives every block about its virtual module's name.
    fn module_name() -> String {
        message(
            "convention",
            "invalid-name",
            "C0103",
            r#"Module name \"1\" doesn't conform to snake_case naming style"#,
            (1, 0),
        )
    }

    /// A finished run that exited `code` and printed `stdout`.
    fn ran(code: i32, stdout: Vec<u8>) -> Ran {
        Ran {
            success: code == 0,
            stdout,
            stderr: Vec::new(),
        }
    }

    #[test]
    fn each_message_is_a_diagnostic_with_its_column_made_one_based() {
        let stdout = report(
            &format!(
                r#"{{"type": "convention", "symbol": "singleton-comparison", "message": "Comparison 'x == None' should be 'x is None'", "messageId": "C0121", "confidence": "UNDEFINED", "module": "doc.md.1", "obj": "", "line": 3, "column": 3, "endLine": 4, "endColumn": 12, "path": "doc.md.1.py", "absolutePath": "/p/doc.md.1.py"}}, {}"#,
                message(
                    "convention",
                    "invalid-name",
                    "C0103",
                    r#"Constant name \"x\" doesn't conform to UPPER_CASE naming style"#,
                    (2, 0)
                )
            ),
            3,
        );

        assert_eq!(
            diagnostics(&stdout),
            Some(vec![
                Diagnostic {
                    line: 3,
                    column: 4,
                    end_line: 4,
                    syntax: false,
                    rule: "C0121".to_owned(),
                    message: "Comparison 'x == None' should be 'x is None'".to_owned(),
                },
                Diagnostic {
                    line: 2,
                    column: 1,
                    end_line: 2,
                    syntax: false,
                    rule: "C0103".to_owned(),
                    message: "Constant name \"x\" doesn't conform to UPPER_CASE naming style"
                        .to_owned(),
                },
            ]),
            "a constant's invalid name is kept; only the module's is dropped"
        );
    }

    #[test]
    fn a_syntax_error_is_the_only_diagnostic_read_as_one() {
        let stdout = report(
            &message(
                "error",
                "syntax-error",
                "E0001",
                "Parsing failed: 'invalid syntax (doc.md.1, line 1)'",
                (1, 7),
            ),
            3,
        );

        let found = diagnostics(&stdout).expect("the report should be read");

        let [syntax] = found.as_slice() else {
            panic!("one diagnostic should be read: {found:?}");
        };
        assert!(syntax.syntax, "{found:?}");
        assert_eq!((syntax.line, syntax.column), (1, 8), "{found:?}");
        assert_eq!(
            lint_verdict("", false, Ok(ran(2, stdout))),
            Verdict::Failed(format::Verdict::ParseFailed {
                detail: "Parsing failed: 'invalid syntax (doc.md.1, line 1)'".to_owned(),
                line: Some(0),
            }),
            "a syntax error is a parse failure at its 0-based body line"
        );
    }

    #[test]
    fn the_virtual_module_name_alone_is_clean_though_pylint_exits_non_zero() {
        let stdout = report(&module_name(), 3);

        assert_eq!(diagnostics(&stdout), Some(Vec::new()));
        assert_eq!(lint_verdict("", false, Ok(ran(16, stdout))), Verdict::Clean);
    }

    #[test]
    fn a_diagnostic_after_a_non_zero_exit_is_reported() {
        let stdout = report(
            &message(
                "warning",
                "unused-argument",
                "W0613",
                "Unused argument 'a'",
                (1, 6),
            ),
            3,
        );

        let verdict = lint_verdict("", false, Ok(ran(4, stdout)));

        assert!(
            matches!(&verdict, Verdict::Diagnostics(found) if found.first().is_some_and(|found| found.rule == "W0613")),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_fatal_message_or_output_that_isnt_the_report_is_a_tool_failure() {
        let fatal = report(
            &message("fatal", "astroid-error", "F0002", "astroid crashed", (1, 0)),
            3,
        );

        assert_eq!(diagnostics(&fatal), None);
        assert_eq!(diagnostics(b"usage: pylint"), None);
        assert!(
            matches!(
                lint_verdict("", false, Ok(ran(1, fatal))),
                Verdict::Failed(_)
            ),
            "a fatal message means pylint failed"
        );
    }

    #[test]
    fn a_report_that_linted_no_module_linted_nothing() {
        assert!(linted_nothing(&report("", 0)));
        assert!(!linted_nothing(&report(&module_name(), 3)));
        assert!(
            !linted_nothing(b"not json"),
            "unreadable output is judged, not skipped"
        );
    }

    #[test]
    fn a_whole_block_disable_switches_lint_off() {
        for block in [
            "# pylint: skip-file\nx = 1\n",
            "x = 1  # pylint: disable=all\n",
            "#pylint:disable=all",
            "# pylint : disable = unused-import, ALL\n",
            "# pylint: disable-all\n",
            "# noqa # pylint: skip-file\n",
            "s = '# pylint: disable=all'\n",
        ] {
            assert!(switches_lint_off(block), "{block:?} should switch lint off");
        }
    }

    #[test]
    fn a_rule_disable_or_a_mention_of_pylint_leaves_lint_on() {
        for block in [
            "x = 1  # pylint: disable=invalid-name\n",
            "# run pylint on all files\n",
            "# pylint disable=all\n",
            "all_files = 1  # pylint: disable=unused-variable\n",
            "x = 1\n",
        ] {
            assert!(!switches_lint_off(block), "{block:?} should leave lint on");
        }
    }
}
