//! The readers of Python's tools' output: ruff's JSON diagnostics, and ruff's and black's syntax
//! errors.

use super::format::{ParseError, first_line};
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

/// One entry of `ruff check --output-format=json`'s array.
#[derive(serde::Deserialize)]
struct RuffEntry {
    /// The rule's code; `null` for a syntax error.
    code: Option<String>,
    /// What ruff says.
    message: String,
    /// Where it starts.
    location: RuffLocation,
    /// Where it ends, when ruff says.
    end_location: Option<RuffLocation>,
}

/// A 1-based line and character column in ruff's JSON.
#[derive(serde::Deserialize)]
struct RuffLocation {
    /// The line.
    row: usize,
    /// The column.
    column: usize,
}

/// The diagnostics in `ruff check --output-format=json`'s `stdout`. `None` when it isn't that
/// JSON array. A syntax error, which ruff reports without a code, is named `syntax-error` and is
/// the only diagnostic read as one.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let entries: Vec<RuffEntry> = serde_json::from_slice(stdout).ok()?;
    Some(
        entries
            .into_iter()
            .map(|entry| Diagnostic {
                line: entry.location.row,
                column: entry.location.column,
                end_line: entry.end_location.map_or(entry.location.row, |end| end.row),
                syntax: entry.code.is_none(),
                rule: entry.code.unwrap_or_else(|| "syntax-error".to_owned()),
                message: entry.message,
            })
            .collect(),
    )
}

/// Judges a `ruff check` run as [`lint::judge`] does, reading its stdout as ruff's JSON array.
/// `input` isn't needed: ruff counts columns in characters, and reads a framed block alike.
#[must_use]
pub(crate) fn lint_verdict(
    _input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    lint::judge(result, diagnostics)
}

/// `ruff format`'s syntax-error report in `stderr`: "Failed to parse <path>:L:C: ...". `None`
/// for any other stderr.
#[must_use]
pub(crate) fn ruff_parse_error(stderr: &str) -> Option<ParseError> {
    parse_error_after("Failed to parse", stderr)
}

/// `black`'s syntax-error report in `stderr`: "Cannot parse: L:C: ...". `None` for any other
/// stderr.
#[must_use]
pub(crate) fn black_parse_error(stderr: &str) -> Option<ParseError> {
    parse_error_after("Cannot parse", stderr)
}

/// The syntax-error report in `stderr` that follows `marker`, at the first `L:C` on its line.
fn parse_error_after(marker: &str, stderr: &str) -> Option<ParseError> {
    let (_, rest) = stderr.split_once(marker)?;
    Some(ParseError {
        line: first_line(rest.lines().next().unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use super::{black_parse_error, diagnostics, ruff_parse_error};
    use crate::code_blocks::format::ParseError;
    use crate::code_blocks::lint::Diagnostic;

    #[test]
    fn ruffs_failed_to_parse_is_a_parse_error_at_its_line() {
        assert_eq!(
            ruff_parse_error("error: Failed to parse /p/a.md.1.py:4:7: Expected an expression\n"),
            Some(ParseError { line: Some(3) })
        );
    }

    #[test]
    fn blacks_cannot_parse_is_a_parse_error_at_its_line() {
        assert_eq!(
            black_parse_error("error: cannot format /p/a.md.1.py: Cannot parse: 2:4: x = (\n"),
            Some(ParseError { line: Some(1) })
        );
    }

    #[test]
    fn other_stderr_is_not_a_parse_error() {
        assert_eq!(black_parse_error("error: boom\n"), None);
    }

    #[test]
    fn ruffs_json_gives_each_diagnostics_rule_lines_and_column_and_a_codeless_one_is_syntax() {
        let stdout = br#"[{"code":"E711","message":"Comparison to `None`","location":{"row":2,"column":6},"end_location":{"row":3,"column":2},"filename":"/p/a.md.1.py","fix":null,"url":null,"noqa_row":2},
            {"code":null,"message":"Expected an expression","location":{"row":3,"column":1},"end_location":{"row":3,"column":2},"filename":"/p/a.md.1.py","fix":null,"url":null,"noqa_row":null}]"#;

        assert_eq!(
            diagnostics(stdout),
            Some(vec![
                Diagnostic {
                    line: 2,
                    column: 6,
                    end_line: 3,
                    syntax: false,
                    rule: "E711".to_owned(),
                    message: "Comparison to `None`".to_owned(),
                },
                Diagnostic {
                    line: 3,
                    column: 1,
                    end_line: 3,
                    syntax: true,
                    rule: "syntax-error".to_owned(),
                    message: "Expected an expression".to_owned(),
                },
            ])
        );
    }

    #[test]
    fn output_that_isnt_ruffs_json_array_has_no_diagnostics() {
        assert_eq!(diagnostics(b"error: boom\n"), None);
        assert_eq!(diagnostics(b"{}"), None);
        assert_eq!(diagnostics(b"[]"), Some(Vec::new()));
    }
}
