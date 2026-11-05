//! Go's formatter: gofmt's environment, where its syntax errors sit, and when its positions are
//! the block's own.
//!
//! gofmt reads the block on stdin with no arguments. It has no config file and no stdin file
//! name, so no file of the project or the user reaches it, wherever it runs.
//!
//! gofmt tries its input as a file, then as a declaration list behind `package p;`, then as a
//! statement list inside `package p; func _() {` … `}`. Its error positions are the block's own
//! only in the first case: for a fragment they count the wrapper's text, so such an error sits at
//! the block's fence line with gofmt's message and no position.

use super::format::{ParseError, first_line};

/// The variables gofmt gets beyond the tool environment: `GOTOOLCHAIN=local`, so a `go` front
/// end never downloads a toolchain to run it.
pub(crate) const VARS: &[(&str, &str)] = &[("GOTOOLCHAIN", "local")];

/// gofmt's syntax-error report: a line `<standard input>:L:C: <message>`. `None` for any other
/// stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let first = stderr.lines().next()?;
    let line = first_line(first)?;
    first
        .starts_with("<standard input>:")
        .then_some(ParseError { line: Some(line) })
}

/// Whether gofmt reads `input` as a whole file, so its error positions are the block's own: the
/// first token past comments is `package`, and no `//line` or `/*line` directive renames the
/// positions it reports.
#[must_use]
pub(crate) fn is_file(input: &str) -> bool {
    if input.contains("//line ") || input.contains("/*line ") {
        return false;
    }
    let mut rest = input;
    loop {
        rest = rest.trim_start();
        if let Some(comment) = rest.strip_prefix("//") {
            rest = comment.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
    }
    rest.strip_prefix("package")
        .is_some_and(|after| after.starts_with(|c: char| c.is_whitespace() || c == '/'))
}

#[cfg(test)]
mod tests {
    use super::{is_file, parse_error};
    use crate::code_blocks::format::ParseError;

    #[test]
    fn a_standard_input_position_is_a_parse_error_at_that_line() {
        assert_eq!(
            parse_error("<standard input>:4:7: expected operand, found '<'\n"),
            Some(ParseError { line: Some(3) })
        );
    }

    #[test]
    fn stderr_without_a_standard_input_position_is_not_a_parse_error() {
        assert_eq!(parse_error("gofmt: read error\n"), None);
        assert_eq!(parse_error("/p/a.go:1:2: x\n"), None);
        assert_eq!(parse_error(""), None);
    }

    #[test]
    fn a_block_opening_with_a_package_clause_is_a_file() {
        assert!(is_file("package main\n\nfunc main() {}\n"));
        assert!(is_file(
            "// Package p does a thing.\n/* more */\npackage p\n"
        ));
        assert!(is_file("package/* c */p\n"));
    }

    #[test]
    fn a_fragment_or_a_renamed_position_is_not_a_file() {
        assert!(!is_file("x := 1\nfmt.Println(x)\n"));
        assert!(!is_file("func f() {}\n"));
        assert!(!is_file("packages := 1\n"));
        assert!(!is_file("// package p\nx := 1\n"));
        assert!(!is_file("package p\n//line other.go:10\nfunc f() {\n"));
        assert!(!is_file(""));
    }
}
