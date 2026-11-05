//! Go's formatter: gofmt's environment, where its syntax errors sit, and when its positions are
//! the block's own.
//!
//! gofmt reads the block on stdin with no arguments. It has no config file and no stdin file
//! name, so no file of the project or the user reaches it, wherever it runs.
//!
//! gofmt tries its input as a file, then as a declaration list behind `package p;`, then as a
//! statement list inside `package p; func _() {` … `}`. Each wrapper opens on the block's first
//! line, so a fragment's error lines are the block's own while a column on line 1 counts the
//! wrapper too, and the statement wrapper's closing lines sit past the block's end: a fragment's
//! error is placed by its line alone, and one past the block is the wrapper's. A `//line` or
//! `/*line` directive in the block renames every position gofmt reports, so such a block's error
//! sits at its fence.

use super::format::{ParseError, Positions, first_line};

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

/// How far gofmt's error positions on `input` are the block's own: renamed by a `//line` or
/// `/*line` directive, wholly for a file, and by line alone for a fragment.
#[must_use]
pub(crate) fn positions(input: &str) -> Positions {
    if input.contains("//line ") || input.contains("/*line ") {
        Positions::Renamed
    } else if is_file(input) {
        Positions::Own
    } else {
        Positions::Lines
    }
}

/// Whether gofmt reads `input` as a whole file: the first token past comments is `package`.
fn is_file(input: &str) -> bool {
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
    use super::{is_file, parse_error, positions};
    use crate::code_blocks::format::{ParseError, Positions};

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
    fn a_line_directive_renames_positions_and_a_fragment_keeps_its_lines() {
        assert_eq!(positions("package p\n"), Positions::Own);
        assert_eq!(positions("x := 1\n"), Positions::Lines);
        assert_eq!(positions("package p\n//line a.go:9\n"), Positions::Renamed);
        assert_eq!(positions("x := 1 /*line :4*/\n"), Positions::Renamed);
    }

    #[test]
    fn a_fragment_is_not_a_file() {
        assert!(!is_file("x := 1\nfmt.Println(x)\n"));
        assert!(!is_file("func f() {}\n"));
        assert!(!is_file("packages := 1\n"));
        assert!(!is_file("// package p\nx := 1\n"));
        assert!(!is_file(""));
    }
}
