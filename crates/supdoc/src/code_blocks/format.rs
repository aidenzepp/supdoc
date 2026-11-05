//! The format step: a formatter's output compared with the block.

use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::cartridges::{markdown, rust as rust_cartridge};
use crate::finding::{Finding, Fix, LineIndex, Location, Severity};
use crate::hash::LineEndings;
use crate::limits::TimeLimit;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, Deadline, SourceFile};

use super::block::{CodeBlock, has_contiguous_body, replacement_for, try_blocks_in};
use super::excerpt;
use super::project::BuildFailed;
use super::runner::{Failure, Ran};
use super::states;
use super::tools::ToolCartridge;

/// The code for a block its formatter would change.
const UNFORMATTED: &str = "code-block-unformatted";

/// The code for a tool that exited unexpectedly or wrote past the output cap.
const TOOL_FAILED: &str = "code-block-tool-failed";

/// The code for a block its language's tool couldn't parse.
pub(super) const PARSE: &str = "code-block-parse";

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
    /// The formatter reported a syntax error in the block.
    ParseFailed {
        /// The first lines of the tool's stderr.
        detail: String,
        /// The block body line (0-based) the tool named, when it named one.
        line: Option<usize>,
    },
    /// The formatter passed its timeout.
    TimedOut,
}

/// A syntax error a formatter reported: the line it named, when it named one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParseError {
    /// The body line (0-based) the tool named.
    pub(crate) line: Option<usize>,
}

/// The 0-based line of the first `line:column` pair in `text`, `None` when it has none.
#[must_use]
pub(crate) fn first_line(text: &str) -> Option<usize> {
    let mut rest = text;
    while let Some(at) = rest.find(|c: char| c.is_ascii_digit()) {
        let from = rest.get(at..)?;
        let digits = from
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(from.len());
        let after = from.get(digits..)?;
        let column = after.strip_prefix(':');
        if column.is_some_and(|column| column.starts_with(|c: char| c.is_ascii_digit())) {
            return from.get(..digits)?.parse::<usize>().ok()?.checked_sub(1);
        }
        rest = after;
    }
    None
}

impl Verdict {
    /// Whether the run behind this verdict may be cached: a pass or a plain failure, never a
    /// tool failure or a timeout.
    #[must_use]
    pub(crate) const fn cacheable(&self) -> bool {
        matches!(
            self,
            Self::Formatted | Self::Unformatted(_) | Self::ParseFailed { .. }
        )
    }

    /// The state a block with this verdict is in.
    #[must_use]
    pub(crate) const fn state(&self) -> states::BlockState {
        match self {
            Self::Formatted => states::BlockState::Passed,
            Self::Unformatted(_) => states::BlockState::Failed(states::Failure::Unformatted),
            Self::ToolFailed(_) => states::BlockState::Failed(states::Failure::ToolFailed),
            Self::ParseFailed { .. } => states::BlockState::Failed(states::Failure::ParseFailed),
            Self::TimedOut => states::BlockState::Failed(states::Failure::ToolTimeout),
        }
    }
}

/// What a formatter reads for `block`, found in `text`: its body without container prefixes, each
/// line's CR dropped, every line ended by LF. Empty for an empty body. `None` when a line's range
/// isn't inside `text`.
#[must_use]
pub(crate) fn input(block: &CodeBlock, text: &str) -> Option<String> {
    let mut input = excerpt::body(block, text, LineEndings::Normalize)?;
    if !block.body.is_empty() {
        input.push('\n');
    }
    Some(input)
}

/// Judges a formatter's `result` on `input`: equal output passes, other output is unformatted and
/// kept, output that isn't UTF-8 and a non-zero exit are failures (the exit carrying the first
/// lines of stderr), and a timeout is a timeout. A non-zero exit is a parse failure only when
/// `tool`'s stderr is its syntax-error report (see [`ToolCartridge::parse_error`]; the runner
/// gives no exit status beyond success, so each tool is told by its message alone) and places
/// the error on a line; a report without a location, like any other non-zero exit, is a tool
/// failure. Stderr after exit 0 is never judged.
#[must_use]
pub(crate) fn judge(tool: &ToolCartridge, input: &str, result: Result<Ran, Failure>) -> Verdict {
    match result {
        Ok(ran) if ran.success && ran.stdout == input.as_bytes() => Verdict::Formatted,
        Ok(ran) if ran.success => String::from_utf8(ran.stdout).map_or_else(
            |_| Verdict::ToolFailed("it printed output that isn't UTF-8".to_owned()),
            Verdict::Unformatted,
        ),
        Ok(ran) => {
            let stderr = String::from_utf8_lossy(&ran.stderr);
            let lines: Vec<&str> = stderr.lines().take(STDERR_LINES).collect();
            let located = tool
                .parse_error(&stderr)
                .filter(|error| error.line.is_some());
            if let Some(error) = located {
                return Verdict::ParseFailed {
                    detail: lines.join("\n"),
                    line: error.line,
                };
            }
            Verdict::ToolFailed(exit_detail(&ran.stderr))
        }
        Err(failure) => failed(failure),
    }
}

/// What a tool's non-zero exit says, from its `stderr`: the first lines, or that it printed
/// nothing.
#[must_use]
pub(crate) fn exit_detail(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = stderr.lines().take(STDERR_LINES).collect();
    if lines.is_empty() {
        "it exited unsuccessfully and printed nothing to stderr".to_owned()
    } else {
        lines.join("\n")
    }
}

/// The verdict on a run that reached no exit: a timeout, or a tool failure saying how it failed.
#[must_use]
pub(crate) fn failed(failure: Failure) -> Verdict {
    failure_detail(failure).map_or(Verdict::TimedOut, Verdict::ToolFailed)
}

/// How a run that reached no exit failed, naming the cap it passed; `None` for a timeout.
#[must_use]
pub(crate) fn failure_detail(failure: Failure) -> Option<String> {
    match failure {
        Failure::TimedOut => None,
        Failure::OverCap { stream, cap } => {
            Some(format!("it wrote more than {} to {stream}", size(cap)))
        }
        Failure::Spawn(error) => Some(format!("it couldn't start: {error}")),
    }
}

/// `bytes` in the largest whole binary unit: `16 MiB`, `64 KiB` or `100 bytes`.
fn size(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    if bytes != 0 && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes != 0 && bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// The finding for `verdict`, from `tool` on the block at `at`, run under `timeout`. `None` when
/// the block is formatted.
#[must_use]
pub(crate) fn finding(
    verdict: &Verdict,
    tool: &ToolCartridge,
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
        Verdict::ParseFailed { detail, .. } => Finding::new(
            PARSE,
            Severity::Error,
            format!("`{name}` couldn't parse the block: {detail}"),
        )
        .with_help(format!(
            "fix the code, or mark the block `ignore(<why>)` or `format(off: <why>)`; \
             `{name}` doesn't run on a block it can't parse"
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

/// The run's one error, at `at`, for a project build that `failed` under `timeout`: a
/// `code-block-tool-failed` quoting cargo's detail, or a `code-block-tool-timeout`. Every block
/// that needed the build fails with it, so this is reported once rather than per block.
#[must_use]
pub(crate) fn build_finding(
    failed: &BuildFailed,
    at: &Location,
    timeout: Option<Duration>,
) -> Finding {
    let off = "`lint = \"off\"` in `[features.code-blocks.rust.clippy]`";
    let finding = match failed {
        BuildFailed::ToolFailed(detail) => Finding::new(
            TOOL_FAILED,
            Severity::Error,
            format!("`cargo check` failed building the project for clippy: {detail}"),
        )
        .with_help(format!(
            "run `cargo check` in the project and fix the build, or set {off}"
        )),
        BuildFailed::TimedOut => Finding::new(
            TOOL_TIMEOUT,
            Severity::Error,
            format!(
                "`cargo check` ran longer than {} building the project for clippy",
                timeout.map_or_else(|| "its limit".to_owned(), |limit| format!("{limit:?}"))
            ),
        )
        .with_help(format!(
            "raise the limit with `--code-block-build-timeout 30m`, or set {off}"
        )),
    };
    finding.with_location(at.clone())
}

/// `unformatted`, the `code-block-unformatted` error `tool` gave `block` in `doc`, with the fix
/// that makes the block's body `output`, the formatter's stdout, when [`safe_fix`] allows it.
/// When [`safe_fix`] refuses, the error keeps no fix and its `code-block-fix-unsafe` warning
/// follows. The error alone when the block's ranges don't lie inside `doc`.
#[must_use]
pub(crate) fn fixed(
    unformatted: Finding,
    block: &CodeBlock,
    doc: &FixDoc,
    output: &str,
    tool: &ToolCartridge,
) -> Vec<Finding> {
    let name = tool.name;
    let what = format!("`{name}`'s output");
    let output = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    match safe_fix(block, doc, output, &what) {
        Guarded::Fix(fix) => {
            let help = format!(
                "run `supdoc fix {} --yes` to apply `{name}`'s output",
                fix.file.as_str()
            );
            vec![unformatted.with_help(help).with_fix(fix)]
        }
        Guarded::Unplaced => vec![unformatted],
        Guarded::Refused(unsafe_fix) => vec![unformatted, unsafe_fix],
    }
}

/// A doc a fix may edit: its text and, parsed once when a fix first needs them, its fenced
/// blocks, which every fix to one of its blocks is checked against.
#[derive(Debug)]
pub(crate) struct FixDoc {
    /// The doc's text.
    text: Arc<str>,
    /// How long one parse of the doc may take: `[limits] parse-timeout`.
    limit: TimeLimit,
    /// The doc's fenced blocks, `None` when the parse failed. Unset until a fix needs them.
    fences: OnceLock<Option<Vec<Fence>>>,
}

impl FixDoc {
    /// `text`, a doc, parsed under `limit` when a fix to one of its blocks is checked.
    #[must_use]
    pub(crate) fn new(text: Arc<str>, limit: TimeLimit) -> Self {
        Self {
            text,
            limit,
            fences: OnceLock::new(),
        }
    }

    /// The doc's text.
    #[must_use]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

/// What [`safe_fix`] made of a fix.
#[derive(Debug)]
pub(crate) enum Guarded {
    /// The fix, which can't end the block early.
    Fix(Fix),
    /// No fix: the block's ranges don't lie inside its doc.
    Unplaced,
    /// No fix: the `code-block-fix-unsafe` warning saying why it could end the block early.
    Refused(Finding),
}

/// Why [`safe_fix`] refused a fix.
enum Refusal {
    /// The body holds a control character Markdown could read as a line ending, or that hides one.
    ControlCharacter,
    /// A line of the body would close the block's fence.
    ClosesFence,
    /// With the fix applied, the doc's fenced blocks are no longer the same.
    ChangesBlocks,
    /// The doc couldn't be parsed again within `parse-timeout` to check the fix.
    Unchecked,
    /// A line that isn't part of the block, such as a `//` comment in a doc-comment run, lies
    /// between its body lines, and replacing the body would delete it.
    SkipsLines,
}

/// The fix that makes `block`'s body in `doc` read `body`, which holds no final line ending,
/// through [`replacement_for`], so container and doc-comment prefixes stay. `what` names where
/// `body` came from, as in "`shfmt`'s output". Every fix a code block gets passes this guard.
///
/// [`Guarded::Unplaced`] when the block's ranges don't lie inside `doc`. A
/// `code-block-fix-unsafe` warning at the block, [`Guarded::Refused`], when the fix could end the
/// block early and write the rest of `body` into the doc as Markdown: `body` holds a C0 control
/// character other than tab and LF (a CR only before LF); a line of it would close the fence (a
/// cheap first pass); or the doc, parsed again with the fix applied the way its blocks were
/// found, no longer holds the same fenced blocks, each opening with the same line and spanning
/// the same text. Also when either parse fails, and when a line that isn't part of the block lies
/// between its body lines (a `//` line in a doc comment), which the fix would delete.
#[must_use]
pub(crate) fn safe_fix(block: &CodeBlock, doc: &FixDoc, body: &str, what: &str) -> Guarded {
    let text = doc.text();
    let Some((marker, length)) = fence_of(block, text) else {
        return Guarded::Unplaced;
    };
    let refuse = |refusal| Guarded::Refused(refused(block, &refusal, what, marker));
    if has_control_character(body) {
        return refuse(Refusal::ControlCharacter);
    }
    if body.lines().any(|line| closes(line, marker, length)) {
        return refuse(Refusal::ClosesFence);
    }
    if !has_contiguous_body(block, text) {
        return refuse(Refusal::SkipsLines);
    }
    let Some((range, replacement)) = replacement_for(block, text, body) else {
        return Guarded::Unplaced;
    };
    let Some(scanned) = text.get(range.clone()) else {
        return Guarded::Unplaced;
    };
    match keeps_blocks(block, doc, &range, &replacement) {
        Some(true) => Guarded::Fix(Fix {
            file: block.fence.file.clone(),
            range,
            replacement,
            scanned: scanned.into(),
        }),
        Some(false) => refuse(Refusal::ChangesBlocks),
        None => refuse(Refusal::Unchecked),
    }
}

/// The `code-block-fix-unsafe` warning at `block` for `refusal` of `what`, the block's new body,
/// whose fence is a run of `marker`.
fn refused(block: &CodeBlock, refusal: &Refusal, what: &str, marker: char) -> Finding {
    let lengthen = format!(
        "lengthen the block's fences past the longest run of `{marker}` in {what}, or mark the block `ignore(<why>)`"
    );
    let by_hand = "edit the block by hand, or mark it `ignore(<why>)`".to_owned();
    let (why, help) = match refusal {
        Refusal::ControlCharacter => (
            "has a control character other than a tab or a line ending",
            by_hand,
        ),
        Refusal::ClosesFence => ("has a line that would close its fence", lengthen),
        Refusal::ChangesBlocks => (
            "would end its fence early or change the doc's other blocks",
            lengthen,
        ),
        Refusal::Unchecked => (
            "couldn't be checked, because the doc didn't parse again within `parse-timeout`",
            "raise the limit with `--parse-timeout 30s`, or edit the block by hand".to_owned(),
        ),
        Refusal::SkipsLines => (
            "would delete a line inside the block that isn't part of it, such as a `//` comment",
            "move that line out of the block or make it a doc-comment line, or edit the block by hand"
                .to_owned(),
        ),
    };
    Finding::new(
        FIX_UNSAFE,
        Severity::Warning,
        format!("{what} for the block {why}, so `fix` won't write it"),
    )
    .with_help(help)
    .with_location(block.fence.clone())
}

/// Whether `body` holds a C0 control character other than tab and LF, counting a CR only when
/// no LF follows it. Markdown reads a bare CR as a line ending, which [`str::lines`] doesn't,
/// and a NUL can hide a line from supdoc's parse.
fn has_control_character(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.iter().enumerate().any(|(at, byte)| match byte {
        b'\t' | b'\n' => false,
        b'\r' => bytes.get(at + 1) != Some(&b'\n'),
        byte => *byte < 0x20,
    })
}

/// One fenced block as the re-parse check compares it.
#[derive(Debug, PartialEq, Eq)]
struct Fence {
    /// From the opening fence through the closing one, as [`CodeBlock::fence`] spans it.
    range: Range<usize>,
    /// The opening fence's line: its fence and info string.
    opening: String,
}

/// Whether `doc`, `block`'s doc, still holds the same fenced blocks once `range` reads
/// `replacement`: as many, each opening with the same line and spanning the same text, shifted
/// past the edit. So `block`'s new body neither closes it early nor spills into the doc. Both
/// texts are parsed the way their blocks were found, the unedited one once per doc. `None` when
/// either can't be parsed.
fn keeps_blocks(
    block: &CodeBlock,
    doc: &FixDoc,
    range: &Range<usize>,
    replacement: &str,
) -> Option<bool> {
    let before = doc
        .fences
        .get_or_init(|| fences(block, doc.text(), doc.limit))
        .as_ref()?;
    let mut edited = doc.text().to_owned();
    edited.replace_range(range.clone(), replacement);
    let after = fences(block, &edited, doc.limit)?;
    let shift = |offset: usize| {
        if offset >= range.end {
            offset - range.len() + replacement.len()
        } else {
            offset
        }
    };
    Some(
        before.len() == after.len()
            && before.iter().zip(&after).all(|(old, new)| {
                new.range == (shift(old.range.start)..shift(old.range.end))
                    && new.opening == old.opening
            }),
    )
}

/// The fenced blocks of `text`, parsed under `limit` as `block`'s doc is: as Markdown, or for
/// its doc comments when the doc is a Rust file. `None` when the parse fails.
fn fences(block: &CodeBlock, text: &str, limit: TimeLimit) -> Option<Vec<Fence>> {
    let file = &block.fence.file;
    let language: &'static dyn Cartridge = if Path::new(file.as_str())
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
    {
        rust_cartridge::cartridge().ok()?
    } else {
        markdown::cartridge::cartridge()
    };
    // The path only names the doc in a parse error, which this check discards.
    let path = AbsolutePath::new(Path::new("/"))?.join_path(Path::new(file.as_str()));
    let parsed =
        SourceFile::from_text(path, text.to_owned(), language, Deadline::start(limit)).ok()?;
    let index = LineIndex::new(text);
    let fences = try_blocks_in(&parsed, |range| {
        Location::in_indexed(file.clone(), text, &index, range)
    })
    .ok()?
    .into_iter()
    .map(|found| {
        let range = found.fence.range;
        let rest = text.get(range.start..).unwrap_or_default();
        let opening = rest.split('\n').next().unwrap_or_default().to_owned();
        Fence { range, opening }
    })
    .collect();
    Some(fences)
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
    use std::sync::Arc;

    use super::{FixDoc, Verdict, failed, finding, fixed, judge};
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::cartridges::rust;
    use crate::code_blocks::block::{CodeBlock, blocks_in};
    use crate::code_blocks::runner::Failure;
    use crate::code_blocks::runner::Ran;
    use crate::code_blocks::tools::formatter;
    use crate::config::code_blocks::Language;
    use crate::finding::{Finding, Location};
    use crate::limits::Limits;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, SourceFile};

    /// The one code block in `text`, as the doc `doc` (relative to `/repo`), parsed by `language`.
    fn block_of(doc: &str, text: &str, language: &'static dyn Cartridge) -> CodeBlock {
        let path = AbsolutePath::new(&std::path::Path::new("/repo").join(doc))
            .expect("the fixture path is absolute");
        let file = SourceFile::parse(path, text.to_owned(), language)
            .expect("tree-sitter should return a tree");
        let mut blocks = blocks_in(&file, |range| {
            let source = doc.parse().expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        });
        assert_eq!(blocks.len(), 1, "expected one block in {text:?}");
        blocks.remove(0)
    }

    #[test]
    fn an_over_cap_failure_names_the_cap_it_passed() {
        let verdicts = [1024 * 1024, 16 * 1024 * 1024, 2048, 100].map(|cap| {
            failed(Failure::OverCap {
                stream: "stdout",
                cap,
            })
        });

        assert_eq!(
            verdicts,
            [
                "it wrote more than 1 MiB to stdout",
                "it wrote more than 16 MiB to stdout",
                "it wrote more than 2 KiB to stdout",
                "it wrote more than 100 bytes to stdout",
            ]
            .map(|detail| Verdict::ToolFailed(detail.to_owned()))
        );
    }

    /// What `shfmt` printing `output` makes of the one block in `text`, a Markdown doc.
    fn fixed_with(text: &str, output: &str) -> Vec<Finding> {
        fixed_in(&block_of("docs/guide.md", text, cartridge()), text, output)
    }

    /// What `shfmt` printing `output` makes of `block`, found in `text`.
    fn fixed_in(block: &CodeBlock, text: &str, output: &str) -> Vec<Finding> {
        let tool = formatter(Language::Shell).expect("shell has a formatter");
        let verdict = Verdict::Unformatted(output.to_owned());
        let unformatted = finding(&verdict, tool, &block.fence, None)
            .expect("an unformatted block has a finding");
        let doc = FixDoc::new(Arc::from(text), Limits::PARSE_TIMEOUT);
        fixed(unformatted, block, &doc, output, tool)
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

    /// The shell formatter.
    fn shfmt() -> &'static crate::code_blocks::tools::ToolCartridge {
        formatter(Language::Shell).expect("shell has a formatter")
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
            judge(shfmt(), "echo hi\n", Ok(printed("echo hi\n"))),
            Verdict::Formatted
        );
        assert_eq!(
            judge(shfmt(), "echo  hi\n", Ok(printed("echo hi\n"))),
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
            judge(shfmt(), "x\n", ran),
            Verdict::ToolFailed("1\n2\n3\n4\n5".to_owned())
        );
    }

    /// The Rust formatter.
    fn rustfmt() -> &'static crate::code_blocks::tools::ToolCartridge {
        formatter(Language::Rust).expect("Rust has a formatter")
    }

    /// A run that exited with `success` and wrote `stdout` and `stderr`.
    fn exited(success: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_rustfmt_error_without_a_stdin_location_is_tool_failed() {
        let ran = Ok(exited(false, "", "error: couldn't read the config\n"));

        assert_eq!(
            judge(rustfmt(), "fn f() {}\n", ran),
            Verdict::ToolFailed("error: couldn't read the config".to_owned())
        );
    }

    #[test]
    fn a_rustfmt_error_at_a_stdin_location_is_a_parse_failure() {
        let stderr = "error: expected one of `;`\n --> <stdin>:3:9\n";
        let ran = Ok(exited(false, "", stderr));

        assert_eq!(
            judge(rustfmt(), "fn f() {\n\nlet x = 1\n}\n", ran),
            Verdict::ParseFailed {
                detail: "error: expected one of `;`\n --> <stdin>:3:9".to_owned(),
                line: Some(2),
            }
        );
    }

    #[test]
    fn exit_zero_with_changed_output_is_unformatted_whatever_stderr_says() {
        let ran = Ok(exited(true, "fn f() {}\n", "error: x\n --> <stdin>:1:1\n"));

        assert_eq!(
            judge(rustfmt(), "fn  f() {}\n", ran),
            Verdict::Unformatted("fn f() {}\n".to_owned())
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

    /// Asserts `findings` are the error and the `code-block-fix-unsafe` warning, neither with a fix.
    fn refused(findings: &[Finding], why: &str) {
        let codes: Vec<&str> = findings.iter().map(|finding| finding.code).collect();
        assert_eq!(
            codes,
            ["code-block-unformatted", "code-block-fix-unsafe"],
            "{why}"
        );
        assert!(
            findings.iter().all(|finding| finding.fix.is_none()),
            "{why}: no finding should carry a fix"
        );
    }

    #[test]
    fn a_nul_before_a_tab_indented_fence_is_refused_in_a_list_and_a_quote() {
        for text in [
            "- item\n\n  ```sh\n  echo  hi\n  ```\n",
            "> ```sh\n> echo  hi\n> ```\n",
        ] {
            let findings = fixed_with(text, "echo hi\0\n\t```\nINJECTED\n");

            refused(&findings, "a NUL can hide the fence line from the parse");
        }
    }

    #[test]
    fn a_nul_before_a_tab_indented_fence_in_a_rust_doc_comment_is_refused() {
        let text = "/// - item\n///\n///   ```sh\n///   echo  hi\n///   ```\nfn f() {}\n";
        let language = rust::cartridge().expect("the built-in Rust queries should compile");
        let block = block_of("src/lib.rs", text, language);

        let findings = fixed_in(&block, text, "echo hi\0\n\t```\nINJECTED\n");

        refused(&findings, "a NUL can hide the fence line from the parse");
    }

    #[test]
    fn output_ending_in_crlf_is_not_read_as_a_bare_cr() {
        let text = "```sh\necho  hi\n```\n";

        let findings = fixed_with(text, "echo hi\r\n");

        assert_eq!(applied(text, &findings), "```sh\necho hi\n```\n");
    }

    #[test]
    fn a_bare_cr_before_a_fence_line_is_refused() {
        let findings = fixed_with("```sh\necho  hi\n```\n", "echo hi\r```\nrm -rf ~\n");

        refused(&findings, "a bare CR ends a line in Markdown");
    }

    #[test]
    fn a_tab_indented_fence_that_closes_the_block_is_refused() {
        let text = "- item\n\n  ```sh\n  echo  hi\n  ```\n";

        let findings = fixed_with(text, "echo hi\n\t```\nrm -rf ~\n");

        refused(
            &findings,
            "two spaces and a tab reach column 4, two past the item's indent",
        );
    }

    #[test]
    fn a_tab_indented_fence_in_an_ordered_list_item_is_refused() {
        let text = "1. item\n\n   ```sh\n   echo  hi\n   ```\n";

        let findings = fixed_with(text, "echo hi\n\t```\n");

        refused(
            &findings,
            "the item's indent and a tab leave the fence one column in",
        );
    }

    #[test]
    fn a_tab_indented_fence_in_a_quote_is_refused() {
        let text = "> ```sh\n> echo  hi\n> ```\n";

        let findings = fixed_with(text, "echo hi\n\t```\n");

        refused(
            &findings,
            "after `> `, a tab reaches column 4, two past the quote's",
        );
    }

    #[test]
    fn a_tab_indented_fence_in_a_rust_doc_comment_is_refused() {
        let text = "/// - item\n///\n///   ```sh\n///   echo  hi\n///   ```\nfn f() {}\n";
        let language = rust::cartridge().expect("the built-in Rust queries should compile");
        let block = block_of("src/lib.rs", text, language);

        let findings = fixed_in(&block, text, "echo hi\n\t```\n");

        refused(&findings, "the doc comment's list item closes on the tab");
    }

    #[test]
    fn a_normal_reformat_in_a_rust_doc_comment_is_fixed() {
        let text = "/// ```sh\n/// echo  hi\n/// ```\nfn f() {}\n";
        let language = rust::cartridge().expect("the built-in Rust queries should compile");
        let block = block_of("src/lib.rs", text, language);

        let findings = fixed_in(&block, text, "echo hi\n\techo\n");

        assert_eq!(
            applied(text, &findings),
            "/// ```sh\n/// echo hi\n/// \techo\n/// ```\nfn f() {}\n"
        );
    }

    #[test]
    fn a_tab_indented_body_line_in_a_quote_is_fixed() {
        let text = "> ```sh\n> echo  hi\n> ```\n";

        let findings = fixed_with(text, "if true; then\n\t:\nfi\n");

        assert_eq!(
            applied(text, &findings),
            "> ```sh\n> if true; then\n> \t:\n> fi\n> ```\n"
        );
    }
}
