//! superhtml: its syntax errors, its version, and the blocks supdoc never gives it.
//!
//! superhtml reads the block on stdin (`fmt --stdin --syntax-only`) and prints it formatted on
//! stdout. It reads no config file, no `.editorconfig` and no environment variable, and its
//! style is fixed: tabs indent. It honors no comment or other in-block directive that switches
//! formatting off; comments are written back as they are. The format step judges its stdout
//! against the block (see [`super::format::judge`]), and `supdoc fix` writes only that stdout, so
//! a doctype or anything else the block holds changes only how superhtml reads it, never what
//! supdoc writes beyond superhtml's own output for it.
//!
//! A block of only whitespace never reaches it (see [`not_given`]): superhtml prints nothing for
//! it and exits 0, and `fix` would write that nothing over the block.

use std::path::Path;

use super::format::ParseError;
use super::runner;
use super::tools::ToolEnv;

/// Why a block's format step didn't run when it holds only whitespace.
pub(crate) const BLANK: &str = "the block holds only whitespace, and superhtml prints nothing for \
                                such input instead of formatting it";

/// Why `input`, an HTML block as its formatter reads it, isn't given to superhtml: it is empty
/// or holds only whitespace ([`BLANK`]). `None` when superhtml may run.
#[must_use]
pub(crate) fn not_given(input: &str) -> Option<&'static str> {
    input.chars().all(char::is_whitespace).then_some(BLANK)
}

/// What starts each error superhtml prints for a block read on stdin: `<stdin>:<row>:<col>: `.
const ERROR_PREFIX: &str = "<stdin>:";

/// The block line superhtml's first syntax error in `stderr` names. superhtml counts rows from 0
/// (`<stdin>:0:6: missing end tag` is the block's first line), as [`ParseError`] does, so the row
/// is taken as it is. Only a line that starts with `<stdin>:` counts: the source line superhtml
/// quotes under each error is indented. `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    stderr.lines().find_map(|line| {
        let rest = line.strip_prefix(ERROR_PREFIX)?;
        let (row, rest) = rest.split_once(':')?;
        let (col, _) = rest.split_once(':')?;
        if col.is_empty() || !col.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(ParseError {
            line: Some(row.parse::<usize>().ok()?),
        })
    })
}

/// The superhtml versions, as `<major>.<minor>`, supdoc's checks are verified against: supdoc
/// reads superhtml's error wording, and its output may change between minors. A minor is added
/// only after the HTML e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 1] = ["0.7"];

/// How a superhtml whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// The version `superhtml version` printed in `output` (`<major>.<minor>.<patch>` alone on its
/// line) when it is one supdoc is verified against (see [`VERIFIED`]).
///
/// # Errors
///
/// How to name the version to the user: the version it printed, or, when its output isn't a
/// version, that.
pub(crate) fn verified(output: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(output);
    let version = text.lines().next().unwrap_or_default().trim_end();
    let parts: Vec<&str> = version.split('.').collect();
    let readable = parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if !readable {
        return Err(UNREADABLE.to_owned());
    }
    let minor = version.rsplit_once('.').map_or(version, |(minor, _)| minor);
    if VERIFIED.contains(&minor) {
        Ok(version.to_owned())
    } else {
        Err(version.to_owned())
    }
}

/// [`verified`] on what `program`, a superhtml binary, prints for its `version` subcommand, run
/// as any tool is (see [`runner::run`]) under `timeout`. superhtml prints its version on stderr,
/// not stdout (`std.debug.print` in its `src/main.zig`), and has no `--version` flag: it prints
/// its usage for one and exits 1.
///
/// Effect: runs the binary.
///
/// # Errors
///
/// How to name the version to the user, as [`verified`] gives it; a run that failed or exited
/// unsuccessfully couldn't be read.
pub(crate) fn version_of(
    program: &Path,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<std::time::Duration>,
) -> Result<String, String> {
    let call = runner::Call {
        program,
        name: "superhtml",
        args: &[std::ffi::OsStr::new("version")],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Neutral,
        cap: runner::CAP,
    };
    match runner::run(&call, env, root) {
        Ok(ran) if ran.success => verified(&ran.stderr),
        _ => Err(UNREADABLE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{BLANK, not_given, parse_error, verified};
    use crate::code_blocks::format::ParseError;

    #[test]
    fn a_block_of_only_whitespace_is_never_given_to_superhtml() {
        for input in ["", "\n", "  \n\t\n", "\r\n"] {
            assert_eq!(not_given(input), Some(BLANK), "{input:?}");
        }
        for input in ["a", "<!-- note -->\n", "<!doctype html>\n", "<p>a</p>\n"] {
            assert_eq!(not_given(input), None, "{input:?}");
        }
    }

    #[test]
    fn a_syntax_error_keeps_superhtmls_zero_based_row() {
        assert_eq!(
            parse_error("<stdin>:0:6: missing end tag\n   <div><p>hi</div>\n         ^\n"),
            Some(ParseError { line: Some(0) })
        );
        assert_eq!(
            parse_error("<stdin>:2:0: missing end tag\n   <div>\n   ^^^^^\n"),
            Some(ParseError { line: Some(2) })
        );
    }

    #[test]
    fn only_an_unindented_stdin_error_line_names_a_row() {
        assert_eq!(
            parse_error("   <stdin>:4:1: quoted source\n<stdin>:1:3: syntax error: x\n"),
            Some(ParseError { line: Some(1) })
        );
        for stderr in [
            "",
            "error: something\n",
            "a.html:1:1: missing end tag\n",
            "<stdin>:x:1: bad\n",
            "<stdin>:1:: bad\n",
        ] {
            assert_eq!(parse_error(stderr), None, "{stderr:?}");
        }
    }

    #[test]
    fn only_a_verified_minor_passes_the_version_gate() {
        assert_eq!(verified(b"0.7.1\n"), Ok("0.7.1".to_owned()));
        assert_eq!(verified(b"0.7.12\n"), Ok("0.7.12".to_owned()));
        assert_eq!(verified(b"0.8.0\n"), Err("0.8.0".to_owned()));
        assert_eq!(verified(b"0.6.4\n"), Err("0.6.4".to_owned()));
        assert_eq!(verified(b"1.7.0\n"), Err("1.7.0".to_owned()));
        for unreadable in [
            &b"superhtml 0.7.1\n"[..],
            b"",
            b"0.7",
            b"0.7.1-dev\n",
            b"v0.7.1\n",
        ] {
            assert_eq!(
                verified(unreadable),
                Err("(whose version supdoc couldn't read)".to_owned()),
                "{:?}",
                String::from_utf8_lossy(unreadable)
            );
        }
    }
}
