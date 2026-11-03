//! The format step: a formatter's output compared with the block.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::config::code_blocks::Language;
use crate::finding::{Finding, Location, Severity};

use super::block::CodeBlock;
use super::runner::{Failure, Ran};
use super::states;
use super::tools::Tool;
use super::{python, rust, shell};

/// The code for a block its formatter would change.
const UNFORMATTED: &str = "code-block-unformatted";

/// The code for a tool that exited unexpectedly or wrote past the output cap.
const TOOL_FAILED: &str = "code-block-tool-failed";

/// The code for a tool that passed the `[limits] code-block-timeout` limit.
const TOOL_TIMEOUT: &str = "code-block-tool-timeout";

/// How many lines of a failed tool's stderr its finding quotes.
const STDERR_LINES: usize = 5;

/// What the format step made of one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The formatter's output is the block.
    Formatted,
    /// The formatter's output differs from the block.
    Unformatted,
    /// The formatter failed; the detail says how.
    ToolFailed(String),
    /// The formatter passed its timeout.
    TimedOut,
}

impl Verdict {
    /// The state a block with this verdict is in.
    #[must_use]
    pub(crate) const fn state(&self) -> states::BlockState {
        match self {
            Self::Formatted => states::BlockState::Passed,
            Self::Unformatted => states::BlockState::Failed(states::Failure::Unformatted),
            Self::ToolFailed(_) => states::BlockState::Failed(states::Failure::ToolFailed),
            Self::TimedOut => states::BlockState::Failed(states::Failure::ToolTimeout),
        }
    }
}

/// The formatter's arguments for the `index`th block (1-based) of `doc`, the doc's absolute path.
/// `None` while `language` has no format step, which leaves its blocks pending.
#[must_use]
pub(crate) fn args(language: Language, doc: &Path, index: usize) -> Option<Vec<OsString>> {
    match language {
        Language::Shell => Some(shell::format_args(doc, index)),
        Language::Rust => rust::format_args(doc, index),
        Language::Python => python::format_args(doc, index),
    }
}

/// What a formatter reads for `block`, found in `text`: its body without container prefixes, each
/// line's CR dropped, every line ended by LF. Empty for an empty body. `None` when a line's range
/// isn't inside `text`.
#[must_use]
pub(crate) fn input(block: &CodeBlock, text: &str) -> Option<String> {
    let mut input = String::new();
    for line in &block.body {
        let content = text.get(line.content.clone())?;
        input.push_str(content.strip_suffix('\r').unwrap_or(content));
        input.push('\n');
    }
    Some(input)
}

/// Judges a formatter's `result` on `input`: equal output passes, other output is unformatted, a
/// non-zero exit is a failure carrying the first lines of stderr, and a timeout is a timeout.
#[must_use]
pub(crate) fn judge(input: &str, result: Result<Ran, Failure>) -> Verdict {
    match result {
        Ok(ran) if ran.success && ran.stdout == input.as_bytes() => Verdict::Formatted,
        Ok(ran) if ran.success => Verdict::Unformatted,
        Ok(ran) => {
            let stderr = String::from_utf8_lossy(&ran.stderr);
            let lines: Vec<&str> = stderr.lines().take(STDERR_LINES).collect();
            Verdict::ToolFailed(if lines.is_empty() {
                "it exited unsuccessfully and printed nothing to stderr".to_owned()
            } else {
                lines.join("\n")
            })
        }
        Err(Failure::TimedOut) => Verdict::TimedOut,
        Err(Failure::OverCap(stream)) => {
            Verdict::ToolFailed(format!("it wrote more than 1 MiB to {stream}"))
        }
        Err(Failure::Spawn(error)) => Verdict::ToolFailed(format!("it couldn't start: {error}")),
    }
}

/// The finding for `verdict`, from `tool` on the block at `at`, run under `timeout`. `None` when
/// the block is formatted.
#[must_use]
pub(crate) fn finding(
    verdict: &Verdict,
    tool: &Tool,
    at: &Location,
    timeout: Option<Duration>,
) -> Option<Finding> {
    let name = tool.name;
    let finding = match verdict {
        Verdict::Formatted => return None,
        Verdict::Unformatted => Finding::new(
            UNFORMATTED,
            Severity::Error,
            format!("the block isn't formatted as `{name}` formats it"),
        )
        .with_help(format!(
            "replace the block's body with what `{name}` prints for it"
        )),
        Verdict::ToolFailed(detail) => Finding::new(
            TOOL_FAILED,
            Severity::Error,
            format!("`{name}` failed on the block: {detail}"),
        )
        .with_help(format!(
            "fix what `{name}` reports, or mark the block `ignore(<why>)`"
        )),
        Verdict::TimedOut => Finding::new(
            TOOL_TIMEOUT,
            Severity::Error,
            format!(
                "`{name}` ran longer than {} on the block",
                timeout.map_or_else(|| "its limit".to_owned(), |limit| format!("{limit:?}"))
            ),
        )
        .with_help(
            "raise the limit with `--code-block-timeout 30s`, or mark the block `ignore(<why>)`",
        ),
    };
    Some(finding.with_location(at.clone()))
}

#[cfg(test)]
mod tests {
    use super::{Verdict, judge};
    use crate::code_blocks::runner::Ran;

    /// A successful run that printed `stdout`.
    fn printed(stdout: &str) -> Ran {
        Ran {
            success: true,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn equal_output_passes_and_other_output_is_unformatted() {
        assert_eq!(
            judge("echo hi\n", Ok(printed("echo hi\n"))),
            Verdict::Formatted
        );
        assert_eq!(
            judge("echo  hi\n", Ok(printed("echo hi\n"))),
            Verdict::Unformatted
        );
    }

    #[test]
    fn a_non_zero_exit_quotes_the_first_five_lines_of_stderr() {
        let ran = Ok(Ran {
            success: false,
            stdout: Vec::new(),
            stderr: b"1\n2\n3\n4\n5\n6\n".to_vec(),
        });

        assert_eq!(
            judge("x\n", ran),
            Verdict::ToolFailed("1\n2\n3\n4\n5".to_owned())
        );
    }
}
