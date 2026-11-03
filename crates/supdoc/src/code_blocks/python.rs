//! Python's format and lint steps.

use std::ffi::OsString;
use std::path::Path;

use super::format::{ParseError, first_line};
use super::tools::Tool;

/// `ruff format` or `black`'s arguments for the `index`th block (1-based) of `doc`, the doc's
/// absolute path, as chosen by `tool`: the block arrives on stdin, named
/// `<doc dir>/<doc file name>.<index>.py` so the tool finds its config walking up from the doc's
/// real directory. Neither writes a file or, for ruff, a cache. `None` for any other tool.
#[must_use]
pub(crate) fn format_args(tool: &Tool, doc: &Path, index: usize) -> Option<Vec<OsString>> {
    let mut name = doc.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}.py"));
    let virtual_path = doc.with_file_name(name).into_os_string();
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

    use super::{format_args, parse_error};
    use crate::code_blocks::format::ParseError;
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
}
