//! Python's format and lint steps.

use std::ffi::OsString;
use std::path::Path;

use super::format::{ParseError, first_line};
use super::lint::Diagnostic;
use super::tools::Tool;

/// The rules `ruff check` leaves off for a block, which a snippet breaks for lack of context:
/// undefined names, unused imports and unused variables.
const RELAXED: &str = "F821,F401,F841";

/// The `index`th block (1-based) of `doc`, the doc's absolute path, as a Python tool sees it:
/// `<doc dir>/<doc file name>.<index>.py`, so the tool finds its config walking up from the doc's
/// real directory.
fn virtual_path(doc: &Path, index: usize) -> OsString {
    let mut name = doc.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}.py"));
    doc.with_file_name(name).into_os_string()
}

/// `ruff check`'s arguments for the `index`th block (1-based) of `doc`, the doc's absolute path:
/// the block on stdin under its virtual path (see [`virtual_path`]), JSON diagnostics, no cache,
/// the snippet relaxations ignored, and never `--fix`. `None` for any other tool.
#[must_use]
pub(crate) fn lint_args(tool: &Tool, doc: &Path, index: usize) -> Option<Vec<OsString>> {
    (tool.name == "ruff").then(|| {
        vec![
            OsString::from("check"),
            OsString::from("--no-cache"),
            OsString::from("--output-format=json"),
            OsString::from("--stdin-filename"),
            virtual_path(doc, index),
            OsString::from("--ignore"),
            OsString::from(RELAXED),
            OsString::from("-"),
        ]
    })
}

/// One entry of `ruff check --output-format=json`'s array.
#[derive(serde::Deserialize)]
struct RuffEntry {
    /// The rule's code; `null` for a syntax error.
    code: Option<String>,
    /// What ruff says.
    message: String,
    /// Where it starts.
    location: RuffLocation,
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
/// JSON array. A syntax error, which ruff reports without a code, is named `syntax-error`.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let entries: Vec<RuffEntry> = serde_json::from_slice(stdout).ok()?;
    Some(
        entries
            .into_iter()
            .map(|entry| Diagnostic {
                line: entry.location.row,
                column: entry.location.column,
                rule: entry.code.unwrap_or_else(|| "syntax-error".to_owned()),
                message: entry.message,
            })
            .collect(),
    )
}

/// `ruff format` or `black`'s arguments for the `index`th block (1-based) of `doc`, the doc's
/// absolute path, as chosen by `tool`: the block arrives on stdin under its virtual path (see
/// [`virtual_path`]). Neither writes a file or, for ruff, a cache. `None` for any other tool.
#[must_use]
pub(crate) fn format_args(tool: &Tool, doc: &Path, index: usize) -> Option<Vec<OsString>> {
    let virtual_path = virtual_path(doc, index);
    let mut args: Vec<OsString> = match tool.name {
        "ruff-format" => vec!["format".into(), "--no-cache".into()],
        "black" => vec!["--quiet".into()],
        _ => return None,
    };
    args.extend([
        OsString::from("--stdin-filename"),
        virtual_path,
        OsString::from("-"),
    ]);
    Some(args)
}

/// `tool`'s syntax-error report in `stderr`: ruff's "Failed to parse <path>:L:C: ..." or black's
/// "Cannot parse: L:C: ...". `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(tool: &Tool, stderr: &str) -> Option<ParseError> {
    let marker = match tool.name {
        "ruff-format" => "Failed to parse",
        "black" => "Cannot parse",
        _ => return None,
    };
    let (_, rest) = stderr.split_once(marker)?;
    Some(ParseError {
        line: first_line(rest.lines().next().unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{diagnostics, format_args, lint_args, parse_error};
    use crate::code_blocks::format::ParseError;
    use crate::code_blocks::lint::Diagnostic;
    use crate::code_blocks::tools::TOOLS;

    fn args_of(name: &str, index: usize) -> Option<Vec<std::ffi::OsString>> {
        let tool = TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .expect("the tool table should list the formatter");
        format_args(tool, Path::new("/p/docs/a.md"), index)
    }

    #[test]
    fn ruff_formats_stdin_under_the_blocks_virtual_path_without_a_cache() {
        assert_eq!(
            args_of("ruff-format", 2).expect("ruff-format has a format step"),
            [
                "format",
                "--no-cache",
                "--stdin-filename",
                "/p/docs/a.md.2.py",
                "-"
            ]
        );
    }

    #[test]
    fn black_formats_stdin_quietly_under_the_blocks_virtual_path() {
        assert_eq!(
            args_of("black", 1).expect("black has a format step"),
            ["--quiet", "--stdin-filename", "/p/docs/a.md.1.py", "-"]
        );
    }

    #[test]
    fn no_other_tool_gets_python_arguments() {
        assert_eq!(args_of("shfmt", 1), None);
    }

    fn tool(name: &str) -> &'static crate::code_blocks::tools::Tool {
        TOOLS
            .iter()
            .find(|tool| tool.name == name)
            .expect("the tool table should list the formatter")
    }

    #[test]
    fn ruffs_failed_to_parse_is_a_parse_error_at_its_line() {
        assert_eq!(
            parse_error(
                tool("ruff-format"),
                "error: Failed to parse /p/a.md.1.py:4:7: Expected an expression\n"
            ),
            Some(ParseError { line: Some(3) })
        );
    }

    #[test]
    fn blacks_cannot_parse_is_a_parse_error_at_its_line() {
        assert_eq!(
            parse_error(
                tool("black"),
                "error: cannot format /p/a.md.1.py: Cannot parse: 2:4: x = (\n"
            ),
            Some(ParseError { line: Some(1) })
        );
    }

    #[test]
    fn other_stderr_is_not_a_parse_error() {
        assert_eq!(parse_error(tool("black"), "error: boom\n"), None);
    }

    #[test]
    fn ruff_checks_stdin_under_the_virtual_path_with_the_relaxations_and_no_fix() {
        let args =
            lint_args(tool("ruff"), Path::new("/p/docs/a.md"), 2).expect("ruff has a lint step");

        assert_eq!(
            args,
            [
                "check",
                "--no-cache",
                "--output-format=json",
                "--stdin-filename",
                "/p/docs/a.md.2.py",
                "--ignore",
                "F821,F401,F841",
                "-"
            ]
        );
        assert!(!args.iter().any(|arg| arg == "--fix"), "{args:?}");
        assert_eq!(
            lint_args(tool("ruff-format"), Path::new("/p/a.md"), 1),
            None
        );
    }

    #[test]
    fn ruffs_json_gives_each_diagnostics_rule_line_and_column() {
        let stdout = br#"[{"code":"E711","message":"Comparison to `None`","location":{"row":2,"column":6},"end_location":{"row":2,"column":10},"filename":"/p/a.md.1.py","fix":null,"url":null,"noqa_row":2},
            {"code":null,"message":"Expected an expression","location":{"row":3,"column":1},"end_location":{"row":3,"column":2},"filename":"/p/a.md.1.py","fix":null,"url":null,"noqa_row":null}]"#;

        assert_eq!(
            diagnostics(stdout),
            Some(vec![
                Diagnostic {
                    line: 2,
                    column: 6,
                    rule: "E711".to_owned(),
                    message: "Comparison to `None`".to_owned(),
                },
                Diagnostic {
                    line: 3,
                    column: 1,
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
