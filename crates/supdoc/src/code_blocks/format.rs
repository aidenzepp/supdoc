//! The format step: a formatter's output compared with the block.

use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::cartridges::{go, js, markdown, python, rust as rust_cartridge, ts, tsx};
use crate::finding::{Finding, Fix, LineIndex, Location, Severity};
use crate::hash::LineEndings;
use crate::limits::TimeLimit;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, Deadline, SourceFile};

use super::block::{CodeBlock, has_contiguous_body, replacement_for, try_blocks_in};
use super::excerpt;
use super::fragment::{Frame, Kind};
use super::project::BuildFailed;
use super::runner::{Failure, Ran};
use super::states;
use super::tools::{Step, ToolCartridge};
use super::yaml;

/// The code for a block its formatter would change.
pub(super) const UNFORMATTED: &str = "code-block-unformatted";

/// The code for a tool that exited unexpectedly or wrote past the output cap.
pub(super) const TOOL_FAILED: &str = "code-block-tool-failed";

/// The code for a block its language's tool couldn't parse.
pub(super) const PARSE: &str = "code-block-parse";

/// The code for a tool that passed the `[limits] code-block-timeout` limit.
pub(super) const TOOL_TIMEOUT: &str = "code-block-tool-timeout";

/// The code for formatter output `fix` refuses to write, because it would escape the block.
const FIX_UNSAFE: &str = "code-block-fix-unsafe";

/// The block tag an unfenced block's fence starts with: an `@example` section with no fence in a
/// `/** */` comment.
const EXAMPLE: &str = "@example";

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
    /// supdoc didn't give the block to its tool, for a reason in its text that makes the block
    /// fail as one that doesn't parse: `detail` says why, in supdoc's words, naming no tool.
    Refused {
        /// Why, as the finding's message.
        detail: String,
        /// The block body line (0-based) the reason is on.
        line: usize,
    },
    /// The formatter passed its timeout.
    TimedOut,
    /// The formatter reported a syntax error in a block it reads inside a frame: the block isn't
    /// `kind`. Its message is supdoc's, never the tool's, which can name what the frame expected.
    NotKind {
        /// The kind the block declares.
        kind: Kind,
        /// The block body line (0-based) the error is on; `None` for a frame line, or a report
        /// that named no line.
        line: Option<usize>,
    },
    /// The formatter's output around a block of this kind doesn't keep the frame it is expected
    /// to print back, or the block's lines inside it lack the frame's indent.
    FrameReformatted(Kind),
    /// The formatter printed a block of this kind on the frame's own line with whitespace at the
    /// frame's edge (biome turns a `{" "}` there into a space), which supdoc can't compare with
    /// the block's lines: the step fails with no fix rather than guess.
    Uncompared(Kind),
    /// The formatter exited 0 and printed nothing, or only whitespace, for a block that isn't
    /// blank: it didn't format the block (its config can switch formatting off), so the step
    /// didn't run. Never a failure and never a fix, which would empty the block. A framed run
    /// that prints the frame alone around such a block is the same.
    PrintedNothing,
}

/// Whether `text` is nothing, or only whitespace by Unicode's measure.
fn blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// Whether a formatter's `output` is nothing, or only whitespace: no formatting of a block that
/// isn't blank. Output that isn't UTF-8 isn't blank.
fn printed_blank(output: &[u8]) -> bool {
    std::str::from_utf8(output).is_ok_and(blank)
}

/// Why a block's format step didn't run when its formatter exited 0 and printed nothing for it.
pub(crate) const PRINTED_NOTHING: &str =
    "the formatter printed nothing for the block, so it didn't format it";

/// Judges a formatter's `result` on a block whose body as written is `input`, run inside
/// `frame` with each non-empty line led by [`depth`] units of `indent` (see
/// [`Frame::wrap_indented`]). Output that begins with the frame's `out_before`, ends with its
/// `out_after` and leads every non-empty line between them with those units is read for those
/// lines, the units removed, and they are compared with `input` as [`judge`] compares a whole
/// block's output. Since every unit removed is one supdoc added, a line the formatter copies
/// verbatim (a template literal's or a block comment's continuation) comes back exact. Output
/// that is `before`, the block's content and `after` on one line, as biome prints a children
/// block with no element in it, is read for that content ([`collapsed`]). Other output is
/// [`Verdict::FrameReformatted`], never a guess. A blank block passes when the output is the
/// frame alone ([`Frame::alone`]). A syntax error anywhere is [`Verdict::NotKind`], at the block
/// line it names when that is one of the block's. A tool failure's detail is in the block's
/// coordinates and shows no line of the frame. Exit 0 with nothing printed, or the frame alone,
/// for a block that isn't blank is [`Verdict::PrintedNothing`].
#[must_use]
pub(crate) fn judge_framed(
    tool: &ToolCartridge,
    frame: Frame,
    indent: &str,
    input: &str,
    result: Result<Ran, Failure>,
) -> Verdict {
    let lines = input.lines().count();
    let lead = indent.repeat(depth(frame));
    match result {
        Ok(ran) if ran.success && printed_blank(&ran.stdout) && !blank(input) => {
            Verdict::PrintedNothing
        }
        Ok(ran) if ran.success => match String::from_utf8(ran.stdout) {
            Err(_) => Verdict::ToolFailed("it printed output that isn't UTF-8".to_owned()),
            Ok(output) => {
                let blank_block = blank(input);
                match collapsed(frame, &output) {
                    Some("") if blank_block => return Verdict::Formatted,
                    Some("") => return Verdict::FrameReformatted(frame.kind),
                    Some(content) if content.trim() != content => {
                        return Verdict::Uncompared(frame.kind);
                    }
                    Some(content) => {
                        let content = format!("{content}\n");
                        return if content == input {
                            Verdict::Formatted
                        } else {
                            Verdict::Unformatted(content)
                        };
                    }
                    None => {}
                }
                let output = canonical(frame, &output, indent);
                if blank_block && frame.alone(&output) {
                    return Verdict::Formatted;
                }
                match frame.inner(&output, &lead) {
                    None => Verdict::FrameReformatted(frame.kind),
                    Some(inner) if inner == input => Verdict::Formatted,
                    Some(inner) if blank(&inner) => Verdict::PrintedNothing,
                    Some(inner) => Verdict::Unformatted(inner),
                }
            }
        },
        result => match judge(tool, input, result) {
            Verdict::ParseFailed { line, .. } => Verdict::NotKind {
                kind: frame.kind,
                line: line
                    .and_then(|line| line.checked_sub(frame.lead()))
                    .filter(|line| *line < lines),
            },
            failed => hidden(frame, &failed, lines, &lead),
        },
    }
}

/// How many indent units a formatter puts before the block's lines inside `frame`: one more
/// than the first line of its `out_after` starts with, each a tab there. One for a frame that
/// closes at the left margin; two for biome's children frame, whose block sits inside a `<div>`
/// that biome indents inside parentheses. supdoc leads each of the block's lines with as many,
/// so a line the formatter copies verbatim sits as deep as the lines it lays out.
#[must_use]
pub(crate) fn depth(frame: Frame) -> usize {
    let closing = frame.out_after.lines().next().unwrap_or_default();
    1 + closing.len() - closing.trim_start_matches('\t').len()
}

/// `output`, a formatter's stdout for a block run inside `frame`, in the form [`Frame::inner`]
/// reads: each frame line's leading `indent` units written as the tabs `frame` writes them with.
/// The block's lines are left as they are. Output too short to hold the frame's lines is returned
/// as it is, for [`Frame::alone`] and [`Frame::inner`] to judge.
fn canonical(frame: Frame, output: &str, indent: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let head = frame.out_before.lines().count();
    let tail = frame.out_after.lines().count();
    if lines.len() < head + tail || indent.is_empty() {
        return output.to_owned();
    }
    let mut canonical = String::with_capacity(output.len());
    for (at, line) in lines.iter().enumerate() {
        let mut rest = *line;
        if at < head || at >= lines.len() - tail {
            while let Some(after) = rest.strip_prefix(indent) {
                canonical.push('\t');
                rest = after;
            }
        }
        canonical.push_str(rest);
        canonical.push('\n');
    }
    canonical
}

/// The block's content in `output` when a formatter printed `frame`'s one-line `before`, that
/// content and its one-line `after` all on one line, as biome prints a `<div>` holding no element
/// (nothing, text, or one `{expression}`). Only for a frame the formatter otherwise lays out
/// other than as written (its `out_before` isn't its `before`), where that line is its other
/// layout of the frame; for a frame printed back as written, such a line is the frame reshaped.
/// `None` for any other output, and for a frame whose `before` or `after` takes more than one
/// line.
fn collapsed(frame: Frame, output: &str) -> Option<&str> {
    let mut lines = output.lines();
    let (line, None) = (lines.next()?, lines.next()) else {
        return None;
    };
    if frame.before.contains('\n') || frame.after.contains('\n') || frame.out_before == frame.before
    {
        return None;
    }
    line.strip_prefix(frame.before)?.strip_suffix(frame.after)
}

/// `failed`, a verdict on a block of `lines` lines run inside `frame`, each non-empty line led
/// by `indent`, with its detail in the block's coordinates and no line of it showing the frame
/// (see [`Frame::hidden`]).
#[must_use]
pub(crate) fn hidden(frame: Frame, failed: &Verdict, lines: usize, indent: &str) -> Verdict {
    match failed {
        Verdict::ToolFailed(detail) => Verdict::ToolFailed(frame.hidden(detail, lines, indent)),
        Verdict::ParseFailed { detail, line } => Verdict::ParseFailed {
            detail: frame.hidden(detail, lines, indent),
            line: *line,
        },
        other => other.clone(),
    }
}

/// How far a formatter's syntax-error positions on a block are the block's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Positions {
    /// Lines and columns both: the tool read the block as written.
    Own,
    /// Lines only: the tool read the block inside a wrapper that keeps its line numbers, so a
    /// column may count the wrapper's text and a line past the block's end is the wrapper's.
    Lines,
    /// Neither: a directive in the block renames the positions the tool reports.
    Renamed,
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
            Self::Formatted
                | Self::Unformatted(_)
                | Self::ParseFailed { .. }
                | Self::NotKind { .. }
                | Self::FrameReformatted(_)
                | Self::Uncompared(_)
        )
    }

    /// The state a block with this verdict is in.
    #[must_use]
    pub(crate) const fn state(&self) -> states::BlockState {
        match self {
            Self::Formatted => states::BlockState::Passed,
            Self::Unformatted(_) => states::BlockState::Failed(states::Failure::Unformatted),
            Self::ToolFailed(_) | Self::FrameReformatted(_) | Self::Uncompared(_) => {
                states::BlockState::Failed(states::Failure::ToolFailed)
            }
            Self::ParseFailed { .. } | Self::NotKind { .. } | Self::Refused { .. } => {
                states::BlockState::Failed(states::Failure::ParseFailed)
            }
            Self::TimedOut => states::BlockState::Failed(states::Failure::ToolTimeout),
            Self::PrintedNothing => states::BlockState::NotChecked(states::NotChecked::NoStepRan),
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
/// failure. How far the report's positions are the block's own is the tool's
/// [`ToolCartridge::positions`] on `input`: a report whose line numbers alone are the block's is
/// placed by them ([`in_block_lines`]), and one whose positions name another text is a parse
/// failure with no line, its positions dropped. Stderr after exit 0 is never judged. Exit 0 with
/// nothing printed for a block that isn't blank is [`Verdict::PrintedNothing`]: the formatter
/// didn't format it.
#[must_use]
pub(crate) fn judge(tool: &ToolCartridge, input: &str, result: Result<Ran, Failure>) -> Verdict {
    match result {
        Ok(ran) if ran.success => printed(input, ran.stdout),
        Ok(ran) => {
            let stderr = String::from_utf8_lossy(&ran.stderr);
            let lines: Vec<&str> = stderr.lines().take(STDERR_LINES).collect();
            match tool.positions(input) {
                Positions::Own => {
                    if let Some(error) = tool
                        .parse_error(&stderr)
                        .filter(|error| error.line.is_some())
                    {
                        return Verdict::ParseFailed {
                            detail: lines.join("\n"),
                            line: error.line,
                        };
                    }
                }
                Positions::Lines => {
                    if tool.parse_error(&stderr).is_some() {
                        return in_block_lines(&lines, input.lines().count());
                    }
                }
                Positions::Renamed => {
                    if lines.first().is_some_and(|line| positioned(line).is_some()) {
                        return Verdict::ParseFailed {
                            detail: lines
                                .iter()
                                .map(|line| unplaced(line))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            line: None,
                        };
                    }
                }
            }
            Verdict::ToolFailed(exit_detail(&ran.stderr))
        }
        Err(failure) => failed(failure),
    }
}

/// What a formatter's `stdout`, after exit 0, says of a block whose body as written is `input`
/// (line endings normalized to `\n`, ended by one unless empty). The output is read as a fix
/// writes it ([`fixed_as`]): its CRLF line endings as `\n` and its final line break, if any,
/// dropped. Output that reads as the block passes, so a fix never leaves the doc as it was;
/// other output is unformatted and kept in that reading. Nothing or only whitespace for a block
/// that isn't blank is [`Verdict::PrintedNothing`]; output that isn't UTF-8 is a tool failure.
#[must_use]
pub(super) fn printed(input: &str, stdout: Vec<u8>) -> Verdict {
    if printed_blank(&stdout) && !blank(input) {
        return Verdict::PrintedNothing;
    }
    let Ok(output) = String::from_utf8(stdout) else {
        return Verdict::ToolFailed("it printed output that isn't UTF-8".to_owned());
    };
    let output = output.replace("\r\n", "\n");
    let same =
        output.strip_suffix('\n').unwrap_or(&output) == input.strip_suffix('\n').unwrap_or(input);
    if same {
        Verdict::Formatted
    } else {
        Verdict::Unformatted(output)
    }
}

/// The parse failure in `lines`, a syntax-error report whose line numbers are those of a block of
/// `count` lines but whose columns may not be: each report line on one of the block's lines,
/// its position dropped, and the first one's line. A report line past the block (a wrapper's end
/// the tool added) is dropped too, unless no line is the block's: then every line is kept and the
/// failure names no line.
fn in_block_lines(lines: &[&str], count: usize) -> Verdict {
    let read: Vec<(Option<usize>, &str)> = lines
        .iter()
        .map(|line| positioned(line).map_or((None, *line), |(row, message)| (Some(row), message)))
        .collect();
    let inside: Vec<&(Option<usize>, &str)> = read
        .iter()
        .filter(|(row, _)| row.is_some_and(|row| row < count))
        .collect();
    let kept: Vec<&str> = if inside.is_empty() {
        read.iter().map(|(_, message)| *message).collect()
    } else {
        inside.iter().map(|(_, message)| *message).collect()
    };
    Verdict::ParseFailed {
        detail: kept.join("\n"),
        line: inside.first().and_then(|(row, _)| *row),
    }
}

/// The 0-based line and the message of `line`, a syntax-error report line led by a position:
/// `<name>:L:C: `, `<name>:L: ` or `L: `. `None` when it has no such position.
fn positioned(line: &str) -> Option<(usize, &str)> {
    let (at, message) = line.split_once(": ")?;
    let numbers: Vec<&str> = at
        .rsplit(':')
        .take(2)
        .take_while(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    let row = numbers.last()?.parse::<usize>().ok()?.checked_sub(1)?;
    Some((row, message))
}

/// `line` of a syntax-error report without its leading position (see [`positioned`]), which
/// counts a text other than the block's. A line with no such position is kept whole.
fn unplaced(line: &str) -> &str {
    positioned(line).map_or(line, |(_, message)| message)
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

/// `text` without its ANSI escape sequences (`ESC [ … <letter>`), which some tools write even to
/// a pipe, so quoted output never colors or moves the terminal it is printed to.
#[must_use]
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
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
        Verdict::Formatted | Verdict::PrintedNothing => return None,
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
        .with_help(match tool.step() {
            Step::Format => format!(
                "fix the code, or mark the block `ignore(<why>)` or `format(off: <why>)`; \
                 `{name}` doesn't run on a block it can't parse"
            ),
            Step::Lint => format!(
                "fix the code, or mark the block `ignore(<why>)` or `lint(off: <why>)`; \
                 `{name}` reports nothing else for a block with a syntax error"
            ),
        }),
        Verdict::Refused { detail, .. } => Finding::new(PARSE, Severity::Error, detail.clone())
            .with_help("remove the character from the block, or mark it `ignore(<why>)`"),
        Verdict::TimedOut => Finding::new(
            TOOL_TIMEOUT,
            Severity::Error,
            format!(
                "`{name}` ran longer than {} on the block",
                timeout.map_or_else(|| "its limit".to_owned(), |limit| format!("{limit:?}"))
            ),
        )
        .with_help(
            "raise the limit with `--code-block-timeout 30s` or `SUPDOC_CODE_BLOCK_TIMEOUT=30s` \
             (`code-block-timeout` under `[limits]` raises it only in a checkout you trust), or \
             mark the block `ignore(<why>)`",
        ),
        Verdict::NotKind { kind, .. } => Finding::new(
            PARSE,
            Severity::Error,
            format!("`{name}` couldn't parse the block as `{}`", kind.name()),
        )
        .with_help(format!(
            "fix the code so it reads as {} fragment, write the lines around the code in the \
             block and drop `fragment(...)`, or mark the block `format(off: <reason>)`",
            kind.with_article()
        )),
        Verdict::Uncompared(kind) => Finding::new(
            TOOL_FAILED,
            Severity::Error,
            format!(
                "supdoc couldn't compare `{name}`'s output with this {} block: `{name}` printed it \
                 on one line with whitespace at its edge",
                kind.name()
            ),
        )
        .with_help(
            "write the whitespace at the block's start or end another way, write the lines around \
             the code in the block and drop `fragment(...)`, or mark the block \
             `format(off: <reason>)`",
        ),
        Verdict::FrameReformatted(kind) => Finding::new(
            TOOL_FAILED,
            Severity::Error,
            format!("`{name}` reformatted the fragment's frame"),
        )
        .with_help(format!(
            "check that the block is {} fragment, write the lines around the code in the block \
             and drop `fragment(...)`, or mark the block `format(off: <reason>)`",
            kind.with_article()
        )),
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
            "raise the limit with `--code-block-build-timeout 30m` or \
             `SUPDOC_CODE_BLOCK_BUILD_TIMEOUT=30m` (`code-block-build-timeout` under `[limits]` \
             raises it only in a checkout you trust), or set {off}"
        )),
    };
    finding.with_location(at.clone())
}

/// `unformatted`, the `code-block-unformatted` error `tool` gave `block` in `doc`, with the fix
/// that makes the block's body `output`, the formatter's stdout, when [`safe_fix`] allows it.
/// When [`safe_fix`] refuses, the error keeps no fix and its `code-block-fix-unsafe` warning
/// follows. The error alone when the block's ranges don't lie inside `doc`. A YAML block's fix is
/// refused the same way unless `output` parses to the same YAML structure as the block (see
/// [`yaml::same_structure`]): yamlfmt rewrites YAML it reads as something else.
#[must_use]
pub(crate) fn fixed(
    unformatted: Finding,
    block: &CodeBlock,
    doc: &FixDoc,
    output: &str,
    tool: &ToolCartridge,
) -> Vec<Finding> {
    fixed_as(
        unformatted,
        block,
        doc,
        output,
        &format!("`{}`", tool.name),
        tool.name == "yamlfmt",
    )
}

/// `unformatted` with its fix, as [`fixed`] gives it, for the formatter `named` (as messages
/// name it, quoted), whose output must keep the block's YAML structure when `yaml`.
#[must_use]
pub(crate) fn fixed_as(
    unformatted: Finding,
    block: &CodeBlock,
    doc: &FixDoc,
    output: &str,
    named: &str,
    yaml: bool,
) -> Vec<Finding> {
    let what = format!("{named}'s output");
    let output = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    if yaml {
        let Some((marker, _)) = fence_of(block, doc.text()) else {
            return vec![unformatted];
        };
        // Both sides as `input` reads a body: the fix writes `output` as the body's lines, each
        // ended by a line break, so its last line ends in one too.
        let after = if output.is_empty() {
            String::new()
        } else {
            format!("{output}\n")
        };
        let keeps = input(block, doc.text())
            .is_some_and(|before| yaml::same_structure(&before, &after, doc.limit));
        if !keeps {
            let unsafe_fix = refused(block, &Refusal::ChangesYaml, &what, Some(marker));
            return vec![unformatted, unsafe_fix];
        }
    }
    match safe_fix(block, doc, output, &what) {
        Guarded::Fix(fix) => {
            let help = format!(
                "run `supdoc fix {} --yes` to apply {what}",
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
    /// The YAML the fix writes doesn't parse to the same structure as the block's.
    ChangesYaml,
    /// A line of an unfenced `@example` block would start with `@` or hold `*/`, which would end
    /// its section or its comment.
    EndsExample,
    /// An unfenced `@example` block starts on its tag's line, which the fix would rewrite.
    SharesTagLine,
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
///
/// An `@example` block with no fence has no fence to close. Its fix is refused instead when a
/// line of `body` would start with `@` or hold `*/`, ending its section or its comment, and when
/// the block starts on its tag's line, which [`replacement_for`] would rewrite.
#[must_use]
pub(crate) fn safe_fix(block: &CodeBlock, doc: &FixDoc, body: &str, what: &str) -> Guarded {
    let text = doc.text();
    let fence = if is_unfenced(block, text) {
        None
    } else {
        let Some(fence) = fence_of(block, text) else {
            return Guarded::Unplaced;
        };
        Some(fence)
    };
    let refuse = |refusal| {
        Guarded::Refused(refused(
            block,
            &refusal,
            what,
            fence.map(|(marker, _)| marker),
        ))
    };
    if has_control_character(body) {
        return refuse(Refusal::ControlCharacter);
    }
    if let Some((marker, length)) = fence {
        if body.lines().any(|line| closes(line, marker, length)) {
            return refuse(Refusal::ClosesFence);
        }
    } else if body.lines().any(ends_example) {
        return refuse(Refusal::EndsExample);
    } else if block
        .body
        .first()
        .is_some_and(|line| line.prefix.start <= block.fence.range.start)
    {
        return refuse(Refusal::SharesTagLine);
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
/// whose fence is a run of `marker`, or which is an unfenced `@example` block when `marker` is
/// `None`.
fn refused(block: &CodeBlock, refusal: &Refusal, what: &str, marker: Option<char>) -> Finding {
    let as_fence = "write the example as a fenced block, or edit it by hand".to_owned();
    let lengthen = marker.map_or_else(|| as_fence.clone(), |marker| format!(
        "lengthen the block's fences past the longest run of `{marker}` in {what}, or mark the block `ignore(<why>)`"
    ));
    let by_hand = if marker.is_some() {
        "edit the block by hand, or mark it `ignore(<why>)`".to_owned()
    } else {
        as_fence.clone()
    };
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
        Refusal::ChangesYaml => (
            "says something else in YAML, not only lays it out differently",
            "edit the block by hand, or mark it `format(off: <why>)`".to_owned(),
        ),
        Refusal::EndsExample => (
            "has a line that starts with `@` or holds `*/`, which would end its `@example` section or its comment",
            as_fence,
        ),
        Refusal::SharesTagLine => (
            "would rewrite its `@example` tag's line, where the block starts",
            "move the example's code to the line after `@example`, or edit it by hand".to_owned(),
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

/// Whether `block` in `text` is an `@example` section with no fence: its fence starts at the
/// tag, not at a run of backticks or tildes.
fn is_unfenced(block: &CodeBlock, text: &str) -> bool {
    text.get(block.fence.range.start..)
        .is_some_and(|rest| rest.starts_with(EXAMPLE))
}

/// Whether `line` of an unfenced `@example` block's new body would end it: its text starts with
/// `@`, a block tag, or holds `*/`, which closes the comment.
fn ends_example(line: &str) -> bool {
    line.trim_start_matches([' ', '\t']).starts_with('@') || line.contains("*/")
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

/// The blocks of `text`, parsed under `limit` with the cartridge [`cartridge_for`] picks for
/// `block`'s doc, the one its blocks were read with. `None` when the parse fails.
fn fences(block: &CodeBlock, text: &str, limit: TimeLimit) -> Option<Vec<Fence>> {
    let file = &block.fence.file;
    let language = cartridge_for(file.as_str())?;
    // The path only names the doc in a parse error, which this check discards.
    let path = AbsolutePath::new(Path::new("/"))?.beneath(file);
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

/// The cartridge whose blocks [`try_blocks_in`] reads from `file`: its doc-comment language's
/// when it is a Rust, Python, JavaScript, TypeScript, TSX or Go file, else Markdown. `None` when
/// that cartridge's queries don't compile.
fn cartridge_for(file: &str) -> Option<&'static dyn Cartridge> {
    let extension = Path::new(file)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    let language: &'static dyn Cartridge = match extension.as_deref() {
        Some(extension) if rust_cartridge::EXTENSIONS.contains(&extension) => {
            rust_cartridge::cartridge().ok()?
        }
        Some(extension) if python::EXTENSIONS.contains(&extension) => python::cartridge().ok()?,
        Some(extension) if js::EXTENSIONS.contains(&extension) => js::cartridge().ok()?,
        Some(extension) if ts::EXTENSIONS.contains(&extension) => ts::cartridge().ok()?,
        Some(extension) if tsx::EXTENSIONS.contains(&extension) => tsx::cartridge().ok()?,
        Some(extension) if go::EXTENSIONS.contains(&extension) => go::cartridge().ok()?,
        _ => markdown::cartridge::cartridge(),
    };
    Some(language)
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

    use super::{
        FixDoc, Frame, Verdict, cartridge_for, failed, finding, fixed, judge, judge_framed,
    };
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::cartridges::rust;
    use crate::code_blocks::block::{CodeBlock, blocks_in};
    use crate::code_blocks::fragment::Kind;
    use crate::code_blocks::runner::Failure;
    use crate::code_blocks::runner::Ran;
    use crate::code_blocks::tools::formatter;
    use crate::config::code_blocks::Language;
    use crate::finding::{Finding, Location};
    use crate::limits::Limits;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, Languages, SourceFile};

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

    /// What `shfmt` printing `output` makes of the one block in `text`, a TypeScript file.
    fn fixed_in_ts(text: &str, output: &str) -> Vec<Finding> {
        let ts = crate::cartridges::ts::cartridge()
            .expect("the built-in TypeScript queries should compile");
        fixed_in(&block_of("src/lib.ts", text, ts), text, output)
    }

    #[test]
    fn an_unfenced_example_gets_a_fix_behind_its_gutter() {
        let text = "/**\n * @example\n * old\n * @returns x\n */\n";

        let findings = fixed_in_ts(text, "a\nb\n");

        assert_eq!(
            findings.len(),
            1,
            "a safe fix adds no warning: {findings:?}"
        );
        assert_eq!(
            applied(text, &findings),
            "/**\n * @example\n * a\n * b\n * @returns x\n */\n"
        );
    }

    #[test]
    fn an_unfenced_examples_fix_is_refused_when_a_line_would_end_it() {
        let text = "/**\n * @example\n * old\n */\n";

        for output in ["  @tag\n", "a */ b\n"] {
            let findings = fixed_in_ts(text, output);

            refused(&findings, output);
        }
    }

    #[test]
    fn an_unfenced_example_starting_on_its_tag_line_gets_no_fix() {
        let text = "/**\n * @example old\n * more\n */\n";

        let findings = fixed_in_ts(text, "a\nb\n");

        refused(&findings, "the fix would rewrite the `@example` line");
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

    /// rustfmt's statements frame, as a frame-pair unit would list it.
    const STATEMENTS: Frame = Frame {
        kind: Kind::Statements,
        before: "fn main() {",
        after: "}",
        out_before: "fn main() {",
        out_after: "}",
    };

    /// rustfmt's default indent unit.
    const FOUR: &str = "    ";

    #[test]
    fn a_framed_run_that_prints_the_frame_alone_for_code_printed_nothing() {
        assert_eq!(
            judge_framed(
                rustfmt(),
                STATEMENTS,
                FOUR,
                "let x = 1;\n",
                Ok(printed("fn main() {\n}\n"))
            ),
            Verdict::PrintedNothing,
            "the frame alone around code must never become an empty fix"
        );
        assert_eq!(
            judge_framed(rustfmt(), STATEMENTS, FOUR, "let x = 1;\n", Ok(printed(""))),
            Verdict::PrintedNothing
        );
    }

    #[test]
    fn output_of_unicode_whitespace_alone_printed_nothing() {
        assert_eq!(
            judge(shfmt(), "echo hi\n", Ok(printed("\u{3000}\u{a0}\n"))),
            Verdict::PrintedNothing
        );
        assert_eq!(
            judge(shfmt(), "echo hi\n", Ok(printed("\n"))),
            Verdict::PrintedNothing
        );
        assert_eq!(
            judge(shfmt(), "\u{3000}\n", Ok(printed("\n"))),
            Verdict::Unformatted("\n".to_owned()),
            "a blank block that formats to nothing is a formatting change, not nothing printed"
        );
    }

    /// The Go formatter.
    fn gofmt() -> &'static crate::code_blocks::tools::ToolCartridge {
        formatter(Language::Go).expect("Go has a formatter")
    }

    #[test]
    fn a_go_fragments_error_sits_at_its_line_and_drops_the_wrappers_end() {
        let stderr = "<standard input>:3:6: expected operand, found '<'\n\
                      <standard input>:6:2: expected ';', found 'EOF'\n";

        assert_eq!(
            judge(
                gofmt(),
                "a := 1\nb := 2\nc := <\n",
                Ok(exited(false, "", stderr))
            ),
            Verdict::ParseFailed {
                detail: "expected operand, found '<'".to_owned(),
                line: Some(2),
            }
        );
    }

    #[test]
    fn a_go_fragments_error_only_past_the_block_names_no_line_and_keeps_the_message() {
        let stderr = "<standard input>:4:1: expected operand, found '}'\n";

        assert_eq!(
            judge(gofmt(), "a+\n", Ok(exited(false, "", stderr))),
            Verdict::ParseFailed {
                detail: "expected operand, found '}'".to_owned(),
                line: None,
            }
        );
    }

    #[test]
    fn a_go_block_with_a_line_directive_is_a_parse_failure_with_no_position() {
        let block = "x := 1\n//line foo.go:10\ny := (\n";

        for stderr in [
            "foo.go:13: expected operand, found '}'\n",
            "13: expected operand, found '}'\n",
            "foo.go:13:2: expected operand, found '}'\n",
        ] {
            assert_eq!(
                judge(gofmt(), block, Ok(exited(false, "", stderr))),
                Verdict::ParseFailed {
                    detail: "expected operand, found '}'".to_owned(),
                    line: None,
                },
                "{stderr}"
            );
        }
    }

    #[test]
    fn a_go_file_keeps_its_error_position() {
        let stderr = "<standard input>:4:7: expected operand, found '<'\n";

        assert_eq!(
            judge(
                gofmt(),
                "package p\n\nfunc f() {\n\tx := <\n}\n",
                Ok(exited(false, "", stderr))
            ),
            Verdict::ParseFailed {
                detail: "<standard input>:4:7: expected operand, found '<'".to_owned(),
                line: Some(3),
            }
        );
    }

    /// biome's entries frame, whose closing line the formatter ends with `;`.
    const ENTRIES: Frame = Frame {
        kind: Kind::Entries,
        before: "({",
        after: "})",
        out_before: "({",
        out_after: "});",
    };

    #[test]
    fn frame_format_output_keeping_the_frame_is_compared_on_its_inner_lines() {
        let nested = "fn main() {\n    if x {\n        y();\n    }\n\n    z();\n}\n";

        assert_eq!(
            judge_framed(
                rustfmt(),
                STATEMENTS,
                FOUR,
                "if x {\n    y();\n}\n\nz();\n",
                Ok(printed(nested))
            ),
            Verdict::Formatted
        );
        assert_eq!(
            judge_framed(
                rustfmt(),
                STATEMENTS,
                FOUR,
                "let x=1;\n",
                Ok(printed("fn main() {\n    let x = 1;\n}\n"))
            ),
            Verdict::Unformatted("let x = 1;\n".to_owned())
        );
        assert_eq!(
            judge_framed(
                rustfmt(),
                ENTRIES,
                "\t",
                "a: 1\n",
                Ok(printed("({\n\ta: 1,\n});\n"))
            ),
            Verdict::Unformatted("a: 1,\n".to_owned())
        );
    }

    #[test]
    fn frame_format_output_losing_the_frame_or_its_indent_is_frame_reformatted() {
        let outputs = [
            "let x = 1;\n",
            "fn main() {\nlet x = 1;\n}\n",
            "fn main() {\n    let x = 1;\n}\nfn f() {}\n",
            "fn main() { let x = 1; }\n",
        ];

        for output in outputs {
            assert_eq!(
                judge_framed(
                    rustfmt(),
                    STATEMENTS,
                    FOUR,
                    "let x = 1;\n",
                    Ok(printed(output))
                ),
                Verdict::FrameReformatted(Kind::Statements),
                "output {output:?} should not pass as the frame"
            );
        }
        assert_eq!(
            judge_framed(
                rustfmt(),
                ENTRIES,
                "\t",
                "a: 1,\n",
                Ok(printed("({\n\ta: 1,\n})\n"))
            ),
            Verdict::FrameReformatted(Kind::Entries),
            "biome's frame must end with the `}});` it prints"
        );
    }

    #[test]
    fn frame_format_a_parse_error_anywhere_is_not_the_kind_at_the_block_line_it_names() {
        let inside = "error: expected `;`\n --> <stdin>:3:9\n  |\n1 | fn main() {\n";
        let on_frame = "error: unexpected closing delimiter\n --> <stdin>:4:1\n";
        let body = "let x = 1;\nlet y = 2\n";

        assert_eq!(
            judge_framed(
                rustfmt(),
                STATEMENTS,
                FOUR,
                body,
                Ok(exited(false, "", inside))
            ),
            Verdict::NotKind {
                kind: Kind::Statements,
                line: Some(1),
            },
            "a syntax error on a block line is the block not being its kind, at that line, \
             never the tool's message"
        );
        assert_eq!(
            judge_framed(
                rustfmt(),
                STATEMENTS,
                FOUR,
                body,
                Ok(exited(false, "", on_frame))
            ),
            Verdict::NotKind {
                kind: Kind::Statements,
                line: None
            }
        );
        assert_eq!(
            judge_framed(
                rustfmt(),
                STATEMENTS,
                FOUR,
                body,
                Ok(exited(false, "", "4 | }\n}\nboom\n"))
            ),
            Verdict::ToolFailed(
                "<the frame supdoc adds>\n<the frame supdoc adds>\nboom".to_owned()
            ),
            "a frame line is hidden by its number, and one with no number by its text"
        );
    }

    #[test]
    fn frame_format_findings_name_the_kind_and_never_the_frame() {
        let at = block_of(
            "docs/guide.md",
            "```rust fragment(statements)\nx\n```\n",
            cartridge(),
        )
        .fence;
        let findings = [
            Verdict::NotKind {
                kind: Kind::Statements,
                line: Some(0),
            },
            Verdict::FrameReformatted(Kind::Statements),
        ]
        .map(|verdict| finding(&verdict, rustfmt(), &at, None).expect("a failure has a finding"));

        assert_eq!(
            findings
                .each_ref()
                .map(|finding| (finding.code, finding.message.as_str())),
            [
                (
                    "code-block-parse",
                    "`rustfmt` couldn't parse the block as `statements`"
                ),
                (
                    "code-block-tool-failed",
                    "`rustfmt` reformatted the fragment's frame"
                ),
            ]
        );
        assert_eq!(
            findings
                .each_ref()
                .map(|finding| finding.help.as_deref().unwrap_or_default()),
            [
                "fix the code so it reads as a statements fragment, write the lines around the \
                 code in the block and drop `fragment(...)`, or mark the block \
                 `format(off: <reason>)`",
                "check that the block is a statements fragment, write the lines around the code \
                 in the block and drop `fragment(...)`, or mark the block \
                 `format(off: <reason>)`",
            ]
        );
        for finding in &findings {
            let help = finding.help.as_deref().unwrap_or_default();
            assert!(
                !finding.message.contains("supdoc_fragment") && !help.contains("supdoc_fragment"),
                "{finding:?} should hold no frame line"
            );
        }
    }

    #[test]
    fn a_parse_failure_names_the_off_switch_of_the_step_that_failed() {
        let at = block_of("docs/guide.md", "```rust\nx\n```\n", cartridge()).fence;
        let verdict = Verdict::ParseFailed {
            detail: "error: expected item".to_owned(),
            line: None,
        };
        let biome_lint = crate::code_blocks::tools::CARTRIDGES
            .iter()
            .find(|tool| tool.name == "biome-lint" && tool.language == Language::TypeScript)
            .expect("the cartridges should include biome-lint for TypeScript");
        let help = |tool| {
            finding(&verdict, tool, &at, None)
                .and_then(|finding| finding.help)
                .expect("a parse failure has help")
        };

        assert_eq!(
            help(rustfmt()),
            "fix the code, or mark the block `ignore(<why>)` or `format(off: <why>)`; \
             `rustfmt` doesn't run on a block it can't parse"
        );
        assert_eq!(
            help(biome_lint),
            "fix the code, or mark the block `ignore(<why>)` or `lint(off: <why>)`; \
             `biome-lint` reports nothing else for a block with a syntax error"
        );
    }

    #[test]
    fn frame_format_the_fix_rewrites_only_the_authors_lines() {
        let text = "# G\n\n```rust fragment(statements)\nlet x=1;\n```\n";
        let block = block_of("docs/guide.md", text, cartridge());
        let output = "fn main() {\n    let x = 1;\n}\n";
        let Verdict::Unformatted(inner) = judge_framed(
            rustfmt(),
            STATEMENTS,
            FOUR,
            "let x=1;\n",
            Ok(printed(output)),
        ) else {
            panic!("a changed statement should be unformatted");
        };
        let unformatted = finding(
            &Verdict::Unformatted(inner.clone()),
            rustfmt(),
            &block.fence,
            None,
        )
        .expect("an unformatted block has a finding");
        let doc = FixDoc::new(Arc::from(text), Limits::PARSE_TIMEOUT);

        let findings = fixed(unformatted, &block, &doc, &inner, rustfmt());

        assert_eq!(
            applied(text, &findings),
            "# G\n\n```rust fragment(statements)\nlet x = 1;\n```\n"
        );
    }

    #[test]
    fn frame_format_a_dropped_leading_or_trailing_frame_line_is_frame_reformatted() {
        for output in ["    let x = 1;\n}\n", "fn main() {\n    let x = 1;\n"] {
            assert_eq!(
                judge_framed(
                    rustfmt(),
                    STATEMENTS,
                    FOUR,
                    "let x = 1;\n",
                    Ok(printed(output))
                ),
                Verdict::FrameReformatted(Kind::Statements),
                "output {output:?} keeps the indent but drops a frame line"
            );
        }
    }

    #[test]
    fn frame_format_a_blank_block_passes_when_the_output_is_the_frame_alone() {
        for (frame, indent, output) in [
            (STATEMENTS, FOUR, "fn main() {}\n"),
            (STATEMENTS, FOUR, "fn main() {\n}\n"),
            (ENTRIES, "\t", "({});\n"),
        ] {
            for input in ["", "\n", "\n\n"] {
                assert_eq!(
                    judge_framed(rustfmt(), frame, indent, input, Ok(printed(output))),
                    Verdict::Formatted,
                    "blank block {input:?} with output {output:?}"
                );
            }
        }
        assert_eq!(
            judge_framed(
                rustfmt(),
                STATEMENTS,
                FOUR,
                "let x = 1;\n",
                Ok(printed("fn main() {}\n"))
            ),
            Verdict::FrameReformatted(Kind::Statements),
            "a block with code is never the frame alone"
        );
    }

    #[test]
    fn frame_format_a_line_the_formatter_copies_verbatim_comes_back_exact() {
        let input = "let s = \"a\n    b\";\nlet j = r#\"{\n}\"#;\n";
        let sent = STATEMENTS.wrap_indented(input, FOUR);

        assert_eq!(
            sent,
            "fn main() {\n    let s = \"a\n        b\";\n    let j = r#\"{\n    }\"#;\n}\n"
        );
        assert_eq!(
            judge_framed(rustfmt(), STATEMENTS, FOUR, input, Ok(printed(&sent))),
            Verdict::Formatted,
            "a formatter that copies the literal's lines verbatim leaves the block as written"
        );
    }

    #[test]
    fn frame_format_a_blank_block_with_the_frame_lost_is_frame_reformatted() {
        for input in ["", "\n"] {
            assert_eq!(
                judge_framed(rustfmt(), STATEMENTS, FOUR, input, Ok(printed("garbage\n"))),
                Verdict::FrameReformatted(Kind::Statements),
                "blank block {input:?}: only the frame alone passes"
            );
        }
    }

    /// biome format's children frame, for a TypeScript block.
    fn children() -> Frame {
        let biome = formatter(Language::TypeScript).expect("TypeScript has a formatter");
        Frame::of(biome, Some(Kind::Children)).expect("biome format frames children")
    }

    #[test]
    fn frame_format_a_verbatim_line_in_a_children_block_comes_back_exact() {
        let input = "<p>{`line one\n\tline two`}</p>\n";
        let biome = formatter(Language::TypeScript).expect("TypeScript has a formatter");

        assert_eq!(
            children().wrap_indented(input, &"\t".repeat(super::depth(children()))),
            "const supdocFragment$ = <div>\n\t\t<p>{`line one\n\t\t\tline two`}</p>\n</div>;\n",
            "the block goes in as deep as biome lays it out"
        );
        assert_eq!(
            judge_framed(
                biome,
                children(),
                "\t",
                input,
                Ok(printed(
                    "const supdocFragment$ = (\n\t<div>\n\t\t<p>{`line one\n\t\t\tline two`}</p>\n\t</div>\n);\n"
                ))
            ),
            Verdict::Formatted,
            "the template's continuation keeps the author's tab"
        );
    }

    #[test]
    fn frame_format_a_children_block_printed_on_the_frames_line_is_read_from_it() {
        let biome = formatter(Language::TypeScript).expect("TypeScript has a formatter");
        let judged = |input: &str, output: &str| {
            judge_framed(biome, children(), "\t", input, Ok(printed(output)))
        };

        for input in ["", "\n", "\n\n"] {
            assert_eq!(
                judged(input, "const supdocFragment$ = <div></div>;\n"),
                Verdict::Formatted,
                "blank block {input:?}"
            );
        }
        assert_eq!(
            judged("{label}\n", "const supdocFragment$ = <div>{label}</div>;\n"),
            Verdict::Formatted
        );
        assert_eq!(
            judged(
                "Hello  world\n",
                "const supdocFragment$ = <div>Hello world</div>;\n"
            ),
            Verdict::Unformatted("Hello world\n".to_owned())
        );
        assert_eq!(
            judged(
                "{label}\n\n",
                "const supdocFragment$ = <div>{label}</div>;\n"
            ),
            Verdict::Unformatted("{label}\n".to_owned()),
            "biome's content and one LF are compared with the block exactly"
        );
        assert_eq!(
            judged("{label}\n", "const supdocFragment$ = <div></div>;\n"),
            Verdict::FrameReformatted(Kind::Children),
            "a block with content is never the frame alone"
        );
        for output in [
            "const supdocFragment$ = <div>Hello </div>;\n",
            "const supdocFragment$ = <div> </div>;\n",
        ] {
            assert_eq!(
                judged("Hello{\" \"}\n", output),
                Verdict::Uncompared(Kind::Children),
                "whitespace at the frame's edge can't be compared: {output:?}"
            );
        }
    }

    #[test]
    fn frame_format_a_parse_error_on_a_block_line_is_not_the_kind_at_that_line() {
        let stderr = "error: expected `;`, found keyword `let`\n --> <stdin>:2:14\n  |\n\
                      2 |     let x = 1\n  |              ^ help: add `;` here\n";

        let verdict = judge_framed(
            rustfmt(),
            STATEMENTS,
            FOUR,
            "let x = 1\nlet y = 2;\n",
            Ok(exited(false, "", stderr)),
        );

        assert_eq!(
            verdict,
            Verdict::NotKind {
                kind: Kind::Statements,
                line: Some(0),
            }
        );
    }

    /// The built-in languages [`try_blocks_in`](crate::code_blocks::block::try_blocks_in) reads
    /// no blocks from. Every other one is a doc language: Markdown, Rust, Python, JavaScript,
    /// TypeScript, TSX and Go.
    const NO_BLOCKS: [&str; 4] = ["bash", "toml", "json", "yaml"];

    #[test]
    fn the_fix_reparse_reads_every_doc_extension_with_its_own_cartridge() {
        let mut checked = 0;
        for cartridge in Languages::builtin()
            .expect("the built-in cartridges should load")
            .iter()
        {
            let id = cartridge.id();
            if NO_BLOCKS.contains(&id.as_str()) {
                continue;
            }
            for extension in cartridge.extensions() {
                let picked = cartridge_for(&format!("src/doc.{extension}"))
                    .expect("every built-in cartridge's queries should compile");
                assert_eq!(
                    picked.id(),
                    id,
                    "the fix re-parse should read a `.{extension}` file as blocks were read from it"
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 7,
            "expected every doc language's extensions, checked {checked}"
        );
    }
}
