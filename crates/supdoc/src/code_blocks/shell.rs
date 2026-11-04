//! The readers of shell's tools' output: shfmt's syntax errors and shellcheck's json1 report.
//!
//! The lint step (see [`super::tools`]) runs `shellcheck -s <dialect> --norc --format=json1 --exclude=<relaxed> -` on
//! the block on stdin. `--norc` keeps any `.shellcheckrc` (a user's `~/.shellcheckrc`, or one in
//! a directory above where the tool runs) from changing results per machine or turning on
//! `external-sources`, so a sourced file is never read; `-x` and `--external-sources` are never
//! passed. The project's own `.shellcheckrc` is not honored either: the block arrives on stdin,
//! which has no filename for shellcheck's rc discovery to start from.

use super::format::{ParseError, first_line};
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

/// The columns a tab advances to the next multiple of in `shellcheck`'s json1 report.
const TAB_STOP: usize = 8;

/// `shellcheck --format=json1`'s report.
#[derive(serde::Deserialize)]
struct Json1 {
    /// Its diagnostics.
    comments: Vec<Comment>,
}

/// One diagnostic in a json1 report.
#[derive(serde::Deserialize)]
struct Comment {
    /// Its 1-based line.
    line: usize,
    /// Its 1-based column, with each tab advancing to the next multiple of [`TAB_STOP`].
    column: usize,
    /// The check's number, which shellcheck names `SC<code>`.
    code: u32,
    /// What shellcheck says.
    message: String,
}

/// The diagnostics in `shellcheck --format=json1`'s `stdout` for `input`, the checked text, each
/// column counted in characters. `None` when it isn't a json1 report.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8], input: &str) -> Option<Vec<Diagnostic>> {
    let report: Json1 = serde_json::from_slice(stdout).ok()?;
    Some(
        report
            .comments
            .into_iter()
            .map(|comment| Diagnostic {
                line: comment.line,
                column: input
                    .lines()
                    .nth(comment.line.saturating_sub(1))
                    .map_or(comment.column, |text| char_column(text, comment.column)),
                rule: format!("SC{}", comment.code),
                message: comment.message,
            })
            .collect(),
    )
}

/// The 1-based character column in `line` of `column`, a 1-based column with tabs expanded to
/// [`TAB_STOP`]. A column inside a tab's span lands on the tab; one past the line's end counts on
/// past it one character per column.
fn char_column(line: &str, column: usize) -> usize {
    let mut visual = 1;
    let mut chars = 1;
    for c in line.chars() {
        let next = if c == '\t' {
            (visual - 1) / TAB_STOP * TAB_STOP + TAB_STOP + 1
        } else {
            visual + 1
        };
        if next > column {
            return chars;
        }
        visual = next;
        chars += 1;
    }
    chars + column.saturating_sub(visual)
}

/// Judges a `shellcheck` run on `input` as [`lint::judge`] does, reading its stdout as a json1
/// report: a non-zero exit with at least one diagnostic gives those diagnostics.
#[must_use]
pub(crate) fn lint_verdict(input: &str, result: Result<Ran, Failure>) -> lint::Verdict {
    lint::judge(result, |stdout| diagnostics(stdout, input))
}

/// `shfmt`'s syntax-error report: a line `<path>:L:C: <message>`. `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let first = stderr.lines().next()?;
    let line = first_line(first)?;
    first
        .contains(": ")
        .then_some(ParseError { line: Some(line) })
}

#[cfg(test)]
mod tests {
    use super::{char_column, diagnostics, lint_verdict, parse_error};
    use crate::code_blocks::format::{self, ParseError};
    use crate::code_blocks::lint::{Diagnostic, Verdict};
    use crate::code_blocks::runner::Ran;

    #[test]
    fn a_path_line_column_report_is_a_parse_error_at_that_line() {
        assert_eq!(
            parse_error("/p/a.md.1.sh:3:5: `fi` must close an `if`\n"),
            Some(ParseError { line: Some(2) })
        );
    }

    #[test]
    fn stderr_without_a_position_is_not_a_parse_error() {
        assert_eq!(parse_error("shfmt: bad input\n"), None);
    }

    #[test]
    fn json1_gives_each_comments_sc_rule_line_and_column() {
        let stdout = br#"{"comments":[{"file":"-","line":2,"endLine":2,"column":6,"endColumn":8,"level":"info","code":2086,"message":"Double quote to prevent globbing and word splitting.","fix":null}]}"#;

        assert_eq!(
            diagnostics(stdout, "x=1\necho $x\n"),
            Some(vec![Diagnostic {
                line: 2,
                column: 6,
                rule: "SC2086".to_owned(),
                message: "Double quote to prevent globbing and word splitting.".to_owned(),
            }])
        );
    }

    #[test]
    fn a_column_after_a_tab_counts_the_tab_as_one_character() {
        // shellcheck puts `$x` in "\techo $x" at column 9 + 5 = 14: the tab spans columns 1-8.
        assert_eq!(char_column("\techo $x", 14), 7);
        assert_eq!(char_column("a\tb", 9), 3);
        assert_eq!(char_column("a\tb", 5), 2);
        assert_eq!(char_column("echo $x", 6), 6);
        assert_eq!(char_column("ab", 4), 4);
    }

    #[test]
    fn output_that_isnt_json1_has_no_diagnostics() {
        assert_eq!(diagnostics(b"shellcheck: boom\n", ""), None);
        assert_eq!(diagnostics(b"[]", ""), None);
        assert_eq!(diagnostics(br#"{"comments":[]}"#, ""), Some(Vec::new()));
    }

    #[test]
    fn a_non_zero_exit_with_comments_is_diagnostics_and_without_is_a_tool_failure() {
        let ran = |stdout: &[u8]| {
            Ok(Ran {
                success: false,
                stdout: stdout.to_vec(),
                stderr: b"bad".to_vec(),
            })
        };
        let comment = br#"{"comments":[{"line":1,"column":1,"code":2148,"message":"m"}]}"#;

        assert!(matches!(
            lint_verdict("echo\n", ran(comment)),
            Verdict::Diagnostics(found) if found.len() == 1
        ));
        assert!(matches!(
            lint_verdict("echo\n", ran(br#"{"comments":[]}"#)),
            Verdict::Failed(format::Verdict::ToolFailed(_))
        ));
        assert!(matches!(
            lint_verdict("echo\n", ran(b"")),
            Verdict::Failed(format::Verdict::ToolFailed(_))
        ));
    }
}
