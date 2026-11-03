//! Shell's format and lint steps.

use std::ffi::OsString;
use std::path::Path;

use super::format::{ParseError, first_line};
use crate::config::code_blocks::Dialect;

/// `shfmt`'s `-ln <dialect>` arguments.
#[must_use]
pub(crate) fn dialect_args(dialect: Dialect) -> [OsString; 2] {
    [
        OsString::from("-ln"),
        OsString::from(dialect.language_name()),
    ]
}

/// `shfmt`'s arguments for the `index`th block (1-based) of `doc`, the doc's absolute path:
/// `--filename <doc dir>/<doc file name>.<index>.sh`. The name's parent is the doc's real
/// directory, so config discovery starts there, while the block itself arrives on stdin.
#[must_use]
pub(crate) fn format_args(doc: &Path, index: usize) -> Vec<OsString> {
    let mut name = doc.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}.sh"));
    vec![
        OsString::from("--filename"),
        doc.with_file_name(name).into_os_string(),
    ]
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
    use std::path::Path;

    use super::{dialect_args, format_args, parse_error};
    use crate::code_blocks::format::ParseError;
    use crate::config::code_blocks::Dialect;

    #[test]
    fn the_filename_names_the_block_by_its_index_in_the_doc() {
        assert_eq!(
            format_args(Path::new("/p/docs/a.md"), 3),
            ["--filename", "/p/docs/a.md.3.sh"]
        );
    }

    #[test]
    fn the_dialect_becomes_shfmts_language_flag() {
        assert_eq!(dialect_args(Dialect::Posix), ["-ln", "posix"]);
    }

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
}
