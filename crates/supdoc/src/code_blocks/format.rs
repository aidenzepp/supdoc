//! The format step: a formatter's output compared with the block.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::config::code_blocks::Language;
use crate::finding::{Finding, Fix, Location, Severity};

use super::block::{CodeBlock, replacement_for};
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

/// The code for formatter output `fix` refuses to write, because it would escape the block.
const FIX_UNSAFE: &str = "code-block-fix-unsafe";

/// How many spaces may lead a line that still closes a fence.
const MAX_FENCE_INDENT: usize = 3;

/// How many lines of a failed tool's stderr its finding quotes.
const STDERR_LINES: usize = 5;

/// What the format step made of one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The formatter's output is the block.
    Formatted,
    /// The formatter's output, which differs from the block.
    Unformatted(String),
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
            Self::Unformatted(_) => states::BlockState::Failed(states::Failure::Unformatted),
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

/// Judges a formatter's `result` on `input`: equal output passes, other output is unformatted and
/// kept, output that isn't UTF-8 and a non-zero exit are failures (the exit carrying the first
/// lines of stderr), and a timeout is a timeout.
#[must_use]
pub(crate) fn judge(input: &str, result: Result<Ran, Failure>) -> Verdict {
    match result {
        Ok(ran) if ran.success && ran.stdout == input.as_bytes() => Verdict::Formatted,
        Ok(ran) if ran.success => String::from_utf8(ran.stdout).map_or_else(
            |_| Verdict::ToolFailed("it printed output that isn't UTF-8".to_owned()),
            Verdict::Unformatted,
        ),
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
        Verdict::Unformatted(_) => Finding::new(
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

/// `unformatted`, the `code-block-unformatted` error `tool` gave `block` in `text`, with a fix
/// that replaces exactly the block's body with `output`, the formatter's stdout, through
/// [`replacement_for`], so container and doc-comment prefixes stay. When a line of `output` would
/// close the block's fence, the error keeps no fix and a `code-block-fix-unsafe` warning follows
/// it. The error alone when the block's ranges don't lie inside `text`.
#[must_use]
pub(crate) fn fixed(
    unformatted: Finding,
    block: &CodeBlock,
    text: &str,
    output: &str,
    tool: &Tool,
) -> Vec<Finding> {
    let Some((marker, length)) = fence_of(block, text) else {
        return vec![unformatted];
    };
    let name = tool.name;
    if output.lines().any(|line| closes(line, marker, length)) {
        let unsafe_fix = Finding::new(
            FIX_UNSAFE,
            Severity::Warning,
            format!("`{name}`'s output for the block has a line that would close its fence, so `fix` won't write it"),
        )
        .with_help(format!(
            "lengthen the block's fences past the output's longest run of `{marker}`, or mark the block `ignore(<why>)`"
        ))
        .with_location(block.fence.clone());
        return vec![unformatted, unsafe_fix];
    }
    let body = output.strip_suffix('\n').unwrap_or(output);
    let Some((range, replacement)) = replacement_for(block, text, body) else {
        return vec![unformatted];
    };
    let Some(scanned) = text.get(range.clone()) else {
        return vec![unformatted];
    };
    let doc = block.fence.file.clone();
    let help = format!(
        "run `supdoc fix {} --yes` to apply `{name}`'s output",
        doc.as_str()
    );
    vec![unformatted.with_help(help).with_fix(Fix {
        file: doc,
        range,
        replacement,
        scanned: scanned.into(),
    })]
}

/// The character and length of `block`'s opening fence in `text`: the first run of backticks or
/// tildes on its line. `None` when the fence's line isn't inside `text` or holds neither.
fn fence_of(block: &CodeBlock, text: &str) -> Option<(char, usize)> {
    let rest = text.get(block.fence.range.start..)?;
    let line = rest.split('\n').next()?;
    let start = line.find(['`', '~'])?;
    let marker = line.get(start..)?.chars().next()?;
    let length = line
        .get(start..)?
        .chars()
        .take_while(|c| *c == marker)
        .count();
    Some((marker, length))
}

/// Whether `line` would close a fence of `length` `marker`s: at most three spaces, then at least
/// `length` of the fence's character.
fn closes(line: &str, marker: char, length: usize) -> bool {
    let rest = line.trim_start_matches(' ');
    line.len() - rest.len() <= MAX_FENCE_INDENT
        && rest.chars().take_while(|c| *c == marker).count() >= length
}

#[cfg(test)]
mod tests {
    use super::{Verdict, finding, fixed, judge};
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::code_blocks::block::{CodeBlock, blocks_in};
    use crate::code_blocks::runner::Ran;
    use crate::code_blocks::tools::formatter;
    use crate::config::code_blocks::Language;
    use crate::finding::{Finding, Location};
    use crate::path::AbsolutePath;
    use crate::read::cartridge::SourceFile;

    /// The one code block in the Markdown `text`, as `docs/guide.md`.
    fn block_of(text: &str) -> CodeBlock {
        let path = AbsolutePath::new(std::path::Path::new("/repo/docs/guide.md"))
            .expect("the fixture path is absolute");
        let file = SourceFile::parse(path, text.to_owned(), cartridge())
            .expect("tree-sitter should return a tree");
        let mut blocks = blocks_in(&file, |range| {
            let source = "docs/guide.md"
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        });
        assert_eq!(blocks.len(), 1, "expected one block in {text:?}");
        blocks.remove(0)
    }

    /// What `shfmt` printing `output` makes of the one block in `text`.
    fn fixed_with(text: &str, output: &str) -> Vec<Finding> {
        let block = block_of(text);
        let tool = formatter(Language::Shell).expect("shell has a formatter");
        let verdict = Verdict::Unformatted(output.to_owned());
        let unformatted = finding(&verdict, tool, &block.fence, None)
            .expect("an unformatted block has a finding");
        fixed(unformatted, &block, text, output, tool)
    }

    /// `text` with the one fix in `findings` applied.
    fn applied(text: &str, findings: &[Finding]) -> String {
        let fix = findings
            .iter()
            .find_map(|finding| finding.fix.as_deref())
            .expect("one finding should carry the fix");
        let mut out = text.to_owned();
        out.replace_range(fix.range.clone(), &fix.replacement);
        out
    }

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
            Verdict::Unformatted("echo hi\n".to_owned())
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

    #[test]
    fn the_fix_replaces_exactly_the_body_with_the_output() {
        let text = "# G\n\n```sh\necho  hi\n```\n\nafter\n";

        let findings = fixed_with(text, "echo hi\n");

        assert_eq!(
            findings.len(),
            1,
            "a safe fix adds no warning: {findings:?}"
        );
        assert_eq!(
            applied(text, &findings),
            "# G\n\n```sh\necho hi\n```\n\nafter\n"
        );
    }

    #[test]
    fn the_fix_keeps_a_list_items_indent() {
        let text = "- item\n\n  ```sh\n  echo  hi\n  if true; then\n  :; fi\n  ```\n";

        let findings = fixed_with(text, "echo hi\nif true; then\n\t:\nfi\n");

        assert_eq!(
            applied(text, &findings),
            "- item\n\n  ```sh\n  echo hi\n  if true; then\n  \t:\n  fi\n  ```\n"
        );
    }

    #[test]
    fn output_that_would_close_the_fence_gets_no_fix_and_a_warning() {
        let text = "```sh\necho  hi\n```\n";

        for output in ["echo hi\n```\n", "   ````\nx\n", "```sh\n"] {
            let findings = fixed_with(text, output);

            let codes: Vec<&str> = findings.iter().map(|finding| finding.code).collect();
            assert_eq!(
                codes,
                ["code-block-unformatted", "code-block-fix-unsafe"],
                "{output:?} would close the fence"
            );
            assert!(
                findings.iter().all(|finding| finding.fix.is_none()),
                "{output:?} should get no fix"
            );
        }
    }

    #[test]
    fn output_that_cant_close_the_fence_is_fixed() {
        let text = "````sh\necho  hi\n````\n";

        for output in ["```\n", "    ````\n", "~~~~\n"] {
            let findings = fixed_with(text, output);

            assert!(
                findings.iter().any(|finding| finding.fix.is_some()),
                "{output:?} can't close a four-backtick fence: {findings:?}"
            );
        }
    }
}
