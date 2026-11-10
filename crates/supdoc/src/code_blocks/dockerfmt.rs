//! dockerfmt: its syntax errors, its version, and the blocks supdoc never gives it.
//!
//! dockerfmt reads the block on stdin with `-i 4 -n` (four-space indent, one final newline) and
//! no file argument, so it reads no `.editorconfig`: dockerfmt applies one only to a file path.
//! It reads no config and no environment variable. The format step judges its stdout against the
//! block (see [`super::format::judge`]), and `supdoc fix` writes only that stdout, so a parser
//! directive the block holds (`# escape=`, `# syntax=`) or a `SHELL` instruction changes only how
//! dockerfmt reads the block, never what supdoc writes beyond dockerfmt's own output for it.
//!
//! Three kinds of block never reach it (see [`not_given`]):
//!
//! - one of at most one byte, for which dockerfmt prints its help text on stdout and exits 0,
//!   text the format step would otherwise take for the formatted block;
//! - one holding `dockerfmt-ignore`, which leaves the next instruction as written, so supdoc
//!   couldn't confirm the block is formatted;
//! - one with no instruction, which dockerfmt refuses with a message that names no line.

use std::path::Path;

use super::format::ParseError;
use super::runner;
use super::tools::ToolEnv;

/// The fewest bytes of input dockerfmt formats: with fewer it prints its help text and exits 0.
const FEWEST_BYTES: usize = 2;

/// The comment that makes dockerfmt leave the next instruction as written.
const IGNORE: &str = "dockerfmt-ignore";

/// Why a block's format step didn't run when it is too short for dockerfmt.
pub(crate) const TOO_SHORT: &str = "the block is at most one byte long, and dockerfmt prints its \
                                    help text for such input instead of formatting it";

/// Why a block's format step didn't run when it holds `dockerfmt-ignore`.
pub(crate) const SWITCHED_OFF: &str = "the block holds `dockerfmt-ignore`, so supdoc can't \
                                       confirm dockerfmt formats all of it; drop the comment, or \
                                       mark the block `format(off: <reason>)`";

/// Why a block's format step didn't run when it holds no Dockerfile instruction.
pub(crate) const NO_INSTRUCTION: &str = "the block holds no Dockerfile instruction, only comments or blank lines, so dockerfmt has \
     nothing to format";

/// Why `input`, a Dockerfile block as its formatter reads it, isn't given to dockerfmt: it is at
/// most one byte long ([`TOO_SHORT`]), holds `dockerfmt-ignore` in any case ([`SWITCHED_OFF`]),
/// or has no line but blanks and comments ([`NO_INSTRUCTION`]). `None` when dockerfmt may run.
///
/// The `dockerfmt-ignore` scan reads text, not instructions, so the words inside a string count
/// too, as does a near miss such as `# dockerfmt-ignored`: a near miss switches the step off,
/// never counts as a pass.
#[must_use]
pub(crate) fn not_given(input: &str) -> Option<&'static str> {
    if input.len() < FEWEST_BYTES {
        return Some(TOO_SHORT);
    }
    if input.to_ascii_lowercase().contains(IGNORE) {
        return Some(SWITCHED_OFF);
    }
    input
        .lines()
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with('#'))
        .then_some(NO_INSTRUCTION)
}

/// The block line dockerfmt's syntax error in `stderr` names: `dockerfmt: stdin: line <n>: ...`,
/// 1-based. `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    stderr.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("dockerfmt: stdin: line ")?;
        let (number, _) = rest.split_once(':')?;
        let number = number.parse::<usize>().ok()?;
        Some(ParseError {
            line: number.checked_sub(1),
        })
    })
}

/// The dockerfmt versions, as `<major>.<minor>`, supdoc's checks are verified against: dockerfmt
/// is young, and its output may change between minors. A minor is added only after the
/// Dockerfile e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 1] = ["0.6"];

/// What `dockerfmt version` prints before the version.
const VERSION_PREFIX: &str = "dockerfmt v";

/// How a dockerfmt whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// The version `dockerfmt version` printed in `stdout` (`dockerfmt v<major>.<minor>.<patch>`)
/// when it is one supdoc is verified against (see [`VERIFIED`]).
///
/// # Errors
///
/// How to name the version to the user: the version it printed, or, when its output isn't a
/// version, that.
pub(crate) fn verified(stdout: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(stdout);
    let version = text
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end()
        .strip_prefix(VERSION_PREFIX)
        .filter(|version| {
            let parts: Vec<&str> = version.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
        .ok_or_else(|| UNREADABLE.to_owned())?;
    let minor = version.rsplit_once('.').map_or(version, |(minor, _)| minor);
    if VERIFIED.contains(&minor) {
        Ok(version.to_owned())
    } else {
        Err(version.to_owned())
    }
}

/// [`verified`] on what `program`, a dockerfmt binary, prints for its `version` subcommand, run
/// as any tool is (see [`runner::run`]) under `timeout`.
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
        name: "dockerfmt",
        args: &[std::ffi::OsStr::new("version")],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Neutral,
        cap: runner::CAP,
    };
    match runner::run(&call, env, root) {
        Ok(ran) if ran.success => verified(&ran.stdout),
        _ => Err(UNREADABLE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{NO_INSTRUCTION, SWITCHED_OFF, TOO_SHORT, not_given, parse_error, verified};
    use crate::code_blocks::format::ParseError;

    #[test]
    fn a_block_of_at_most_one_byte_is_never_given_to_dockerfmt() {
        for input in ["", "\n", "F"] {
            assert_eq!(not_given(input), Some(TOO_SHORT), "{input:?}");
        }
        assert_eq!(not_given("F\n"), None, "two bytes are formatted");
    }

    #[test]
    fn dockerfmt_ignore_in_any_case_or_near_miss_switches_the_step_off() {
        for input in [
            "# dockerfmt-ignore\nRUN a\n",
            "FROM a\n  #   DOCKERFMT-IGNORE\nRUN b\n",
            "FROM a\n# dockerfmt-ignored\n",
            "RUN echo \"dockerfmt-ignore\"\n",
        ] {
            assert_eq!(not_given(input), Some(SWITCHED_OFF), "{input:?}");
        }
        assert_eq!(not_given("FROM a\n# dockerfmt ignore\n"), None);
    }

    #[test]
    fn a_block_of_only_comments_and_blanks_is_never_given_to_dockerfmt() {
        assert_eq!(not_given("# escape=`\n\n  # note\n"), Some(NO_INSTRUCTION));
        assert_eq!(not_given("# note\nFROM a\n"), None);
    }

    #[test]
    fn a_syntax_error_is_placed_at_its_zero_based_line() {
        assert_eq!(
            parse_error("dockerfmt: stdin: line 2: unterminated heredoc\n"),
            Some(ParseError { line: Some(1) })
        );
        assert_eq!(
            parse_error("dockerfmt: stdin: file with no instructions\n"),
            None
        );
        assert_eq!(parse_error("stdin:3: left unformatted: x\n"), None);
    }

    #[test]
    fn only_a_verified_minor_passes_the_version_gate() {
        assert_eq!(verified(b"dockerfmt v0.6.0\n"), Ok("0.6.0".to_owned()));
        assert_eq!(verified(b"dockerfmt v0.6.12\n"), Ok("0.6.12".to_owned()));
        assert_eq!(verified(b"dockerfmt v0.7.0\n"), Err("0.7.0".to_owned()));
        assert_eq!(verified(b"dockerfmt v0.5.9\n"), Err("0.5.9".to_owned()));
        for unreadable in [&b"dockerfmt dev\n"[..], b"", b"v0.6.0", b"dockerfmt v0.6\n"] {
            assert_eq!(
                verified(unreadable),
                Err("(whose version supdoc couldn't read)".to_owned()),
                "{:?}",
                String::from_utf8_lossy(unreadable)
            );
        }
    }
}
