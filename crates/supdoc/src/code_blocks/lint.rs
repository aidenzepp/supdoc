//! The lint step: a linter's diagnostics on a block, each mapped from the checked text to its
//! line and column in the doc.

use std::borrow::Cow;
use std::time::Duration;

use crate::finding::{Finding, Location, Severity};

use super::block::CodeBlock;
use super::format;
use super::fragment::Frame;
use super::runner::{Failure, Ran};
use super::rust::LineMap;
use super::tools::ToolCartridge;

/// The code for a linter's diagnostic on a block.
pub(crate) const LINT: &str = "code-block-lint";

/// The code for a name a block uses but neither it nor the project's crate defines.
const MISSING_NAME: &str = "code-block-missing-name";

/// The rustc error codes that say a name isn't defined: an unresolved type, trait, struct,
/// function, import, path, crate or pattern name.
const MISSING_NAME_CODES: [&str; 9] = [
    "E0405", "E0412", "E0422", "E0425", "E0432", "E0433", "E0463", "E0531", "E0532",
];

/// The text a linter checks for a block whose body as written is `input`: `input` itself, or
/// `input` inside `frame`.
#[must_use]
pub(crate) fn checked(frame: Option<Frame>, input: &str) -> Cow<'_, str> {
    frame.map_or(Cow::Borrowed(input), |frame| Cow::Owned(frame.wrap(input)))
}

/// The name `diagnostic` says is missing: the first backticked span of rustc's message. `None`
/// when `diagnostic` isn't a missing-name error. A message with no backticks names itself.
#[must_use]
pub(crate) fn missing_name(diagnostic: &Diagnostic) -> Option<&str> {
    if !MISSING_NAME_CODES.contains(&diagnostic.rule.as_str()) {
        return None;
    }
    let message = diagnostic.message.as_str();
    Some(
        message
            .split_once('`')
            .and_then(|(_, rest)| rest.split_once('`'))
            .map_or(message, |(name, _)| name),
    )
}

/// One diagnostic a linter reported, in checked-text coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Diagnostic {
    /// Its 1-based line in the checked text.
    pub(crate) line: usize,
    /// Its 1-based column in that line, counted in characters.
    pub(crate) column: usize,
    /// Its 1-based last line in the checked text: `line` when the linter gives no end.
    pub(crate) end_line: usize,
    /// Whether its linter reports it as a syntax error: the checked text doesn't parse. Each
    /// reader says how it tells one.
    pub(crate) syntax: bool,
    /// The rule it breaks, as the linter names it.
    pub(crate) rule: String,
    /// What the linter says about it.
    pub(crate) message: String,
}

/// What the lint step made of one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The linter reported nothing.
    Clean,
    /// The linter's diagnostics, at least one.
    Diagnostics(Vec<Diagnostic>),
    /// The linter failed or passed its timeout: [`format::Verdict::ToolFailed`] or
    /// [`format::Verdict::TimedOut`].
    Failed(format::Verdict),
    /// A custom linter exited non-zero and no line it printed reads as a diagnostic: the first
    /// lines of its output, `None` when it printed nothing. Only a custom linter gives this.
    Unparsed(Option<String>),
}

impl Verdict {
    /// Whether the run behind this verdict may be cached: a clean run or diagnostics, never a
    /// tool failure or a timeout.
    #[must_use]
    pub(crate) const fn cacheable(&self) -> bool {
        matches!(self, Self::Clean | Self::Diagnostics(_))
    }
}

/// Judges a linter's `result`: exit 0 is clean, a non-zero exit whose stdout `diagnostics` reads
/// as the tool's report of at least one diagnostic gives those diagnostics, and any other
/// non-zero exit, output past the cap or a failed start is a tool failure; a timeout is a
/// timeout.
#[must_use]
pub(crate) fn judge(
    result: Result<Ran, Failure>,
    diagnostics: impl FnOnce(&[u8]) -> Option<Vec<Diagnostic>>,
) -> Verdict {
    match result {
        Ok(ran) if ran.success => Verdict::Clean,
        Ok(ran) => match diagnostics(&ran.stdout) {
            Some(found) if !found.is_empty() => Verdict::Diagnostics(found),
            _ => Verdict::Failed(format::Verdict::ToolFailed(format::exit_detail(
                &ran.stderr,
            ))),
        },
        Err(failure) => Verdict::Failed(format::failed(failure)),
    }
}

/// Where `diagnostic` is in the doc holding `block`, whose text is `text`: its body line through
/// `map`, then its column past the line's container prefix. A column past the line's end lands at
/// its end. `None` when the line isn't one of the block's.
#[must_use]
pub(crate) fn place(diagnostic: &Diagnostic, map: &LineMap, text: &str) -> Option<Location> {
    let line = diagnostic.line.checked_sub(1)?;
    let start = map.doc_line(line)?;
    let content = text.get(start.range.clone())?;
    let skip = diagnostic.column.saturating_sub(1);
    let (chars, bytes) = content
        .char_indices()
        .take(skip)
        .fold((0, 0), |(chars, _), (at, c)| (chars + 1, at + c.len_utf8()));
    let at = start.range.start + bytes;
    Some(Location {
        file: start.file.clone(),
        range: at..at,
        line: start.line,
        column: start.column + u32::try_from(chars).unwrap_or(u32::MAX),
    })
}

/// What the lint step reads a run with: the tool, the block, the doc's text and whether the
/// block was framed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Read<'run> {
    /// The linter.
    pub(crate) tool: &'run ToolCartridge,
    /// The block it checked.
    pub(crate) block: &'run CodeBlock,
    /// The text of the doc holding the block.
    pub(crate) text: &'run str,
    /// The frame the linter saw around the block, if any.
    pub(crate) frame: Option<Frame>,
}

/// The findings for `verdict`, read as `read` says: one `code-block-lint` error per diagnostic
/// at its place in the doc (the fence when it can't be placed), naming its rule and
/// `lint(off: <reason>)`, never with a fix; or the tool failure or timeout error at the fence, run
/// under `timeout`. A diagnostic that [`missing_name`] reads is a `code-block-missing-name` error
/// instead, whose message says whether the project's crate was passed to the tool (`externs`).
///
/// A framed block's diagnostics are shifted past the frame's leading lines. A framed run with
/// any syntax error, wherever it lands, gives one `code-block-parse` "couldn't parse the block
/// as `<kind>`" and nothing else: at the first syntax error's doc line, or at the block's info
/// string when that is a frame line. The tool's syntax messages are never shown, since they name
/// what the frame expected, and the run's other diagnostics come from the tool's recovery from
/// text that isn't the kind. In a run that parsed, a diagnostic that starts or ends on a frame
/// line is never dropped: it is reported at the info string with the tool's rule and supdoc's
/// own message, never the tool's. A framed block's tool failure shows no line
/// of the frame. No location, message or help holds a frame line.
#[must_use]
pub(crate) fn findings(
    verdict: &Verdict,
    read: &Read<'_>,
    timeout: Option<Duration>,
    externs: bool,
) -> Vec<Finding> {
    let Read {
        tool,
        block,
        text,
        frame,
    } = *read;
    match verdict {
        // Only a custom linter's output is unparsed, and its own findings read that.
        Verdict::Clean | Verdict::Unparsed(_) => Vec::new(),
        Verdict::Failed(failed) => {
            let failed = frame.map_or_else(
                || failed.clone(),
                |frame| format::hidden(frame, failed, block.body.len(), ""),
            );
            format::finding(&failed, tool, &block.fence, timeout)
                .into_iter()
                .collect()
        }
        Verdict::Diagnostics(found) => {
            let map = LineMap::of(block, text);
            let lines = block.body.len();
            let place_in_block = |diagnostic: &Diagnostic| {
                map.as_ref()
                    .and_then(|map| place(diagnostic, map, text))
                    .unwrap_or_else(|| block.fence.clone())
            };
            let not_its_kind = frame.and_then(|frame| {
                let first = found.iter().find(|diagnostic| diagnostic.syntax)?;
                let at = unframed(first, Some(frame), lines)
                    .map_or_else(|_| attribute(block), |inside| place_in_block(&inside));
                Some(not_its_kind(tool, frame).with_location(at))
            });
            if let Some(not_its_kind) = not_its_kind {
                return vec![not_its_kind];
            }
            found
                .iter()
                .map(|diagnostic| {
                    let diagnostic = match unframed(diagnostic, frame, lines) {
                        Ok(diagnostic) => diagnostic,
                        Err(frame) => {
                            return on_frame(tool, frame, diagnostic)
                                .with_location(attribute(block));
                        }
                    };
                    let diagnostic = &diagnostic;
                    let at = place_in_block(diagnostic);
                    missing_name(diagnostic)
                        .map_or_else(
                            || lint_finding(tool, diagnostic),
                            |name| missing_name_finding(name, externs),
                        )
                        .with_location(at)
                })
                .collect()
        }
    }
}

/// `diagnostic` in the block's own coordinates: shifted past `frame`'s leading lines, the block
/// holding `lines` lines. A diagnostic at line 0, which names no line, stays there.
///
/// # Errors
///
/// The frame, when `diagnostic` starts or ends on one of its lines.
fn unframed(
    diagnostic: &Diagnostic,
    frame: Option<Frame>,
    lines: usize,
) -> Result<Diagnostic, Frame> {
    let Some(frame) = frame.filter(|_| diagnostic.line != 0) else {
        return Ok(diagnostic.clone());
    };
    let shift = |line: usize| {
        line.checked_sub(frame.lead())
            .filter(|line| (1..=lines).contains(line))
            .ok_or(frame)
    };
    let line = shift(diagnostic.line)?;
    let end_line = shift(diagnostic.end_line.max(diagnostic.line))?;
    Ok(Diagnostic {
        line,
        end_line,
        ..diagnostic.clone()
    })
}

/// Where a finding about the block as a whole goes: its info string, which holds its
/// `fragment(...)`, or its fence when it has none.
fn attribute(block: &CodeBlock) -> Location {
    block
        .info
        .as_ref()
        .map_or_else(|| block.fence.clone(), |info| info.location.clone())
}

/// The finding, without a location, for `diagnostic`, a rule's (never a syntax error, see
/// [`not_its_kind`]), from `tool` on a line of `frame`: the tool's rule in supdoc's own words,
/// never the tool's message, which may quote the frame.
fn on_frame(tool: &ToolCartridge, frame: Frame, diagnostic: &Diagnostic) -> Finding {
    let rule = &diagnostic.rule;
    Finding::new(
        LINT,
        Severity::Error,
        format!(
            "`{}` reports `{rule}` where the block meets the frame supdoc adds around {} fragment",
            tool.name,
            frame.kind.with_article()
        ),
    )
    .with_help(format!(
        "fix the code `{rule}` flags at the block's start or end, check that the block is {} \
         fragment, or mark the block `lint(off: <reason>)`",
        frame.kind.with_article()
    ))
}

/// The `code-block-parse` error, without a location, for a framed block the tool couldn't
/// parse: it isn't `frame`'s kind. It never carries the tool's message, which names what the
/// frame expected (a `)` the author never wrote, a function that isn't `async`).
fn not_its_kind(tool: &ToolCartridge, frame: Frame) -> Finding {
    Finding::new(
        format::PARSE,
        Severity::Error,
        format!(
            "`{}` couldn't parse the block as `{}`",
            tool.name,
            frame.kind.name()
        ),
    )
    .with_help(format!(
        "fix the code so it reads as {} fragment, write the lines around the code in the block \
         and drop `fragment(...)`, or mark the block `lint(off: <reason>)`",
        frame.kind.with_article()
    ))
}

/// The `code-block-missing-name` error for `name`, without a location.
fn missing_name_finding(name: &str, externs: bool) -> Finding {
    let whence = if externs {
        "this block or the project's crate"
    } else {
        "this block"
    };
    Finding::new(
        MISSING_NAME,
        Severity::Error,
        format!("`{name}` isn't defined in {whence}"),
    )
    .with_help(format!(
        "define or import `{name}` in the block, show the real code with `from(<path>#<symbol>)`, \
         or mark the block `lint(off: <reason>)`"
    ))
}

/// The `code-block-lint` error for `diagnostic` from `tool`, without a location.
fn lint_finding(tool: &ToolCartridge, diagnostic: &Diagnostic) -> Finding {
    let Diagnostic { rule, message, .. } = diagnostic;
    Finding::new(
        LINT,
        Severity::Error,
        format!("`{}` reports `{rule}`: {message}", tool.name),
    )
    .with_help(format!(
        "fix the code `{rule}` flags, or mark the block `lint(off: <reason>)` to skip its lint step"
    ))
}

#[cfg(test)]
mod tests {

    use super::{Diagnostic, Frame, Read, Verdict, findings, place};
    use crate::cartridges::markdown::cartridge::cartridge as markdown;
    use crate::cartridges::rust::cartridge as rust;
    use crate::code_blocks::block::{CodeBlock, blocks_in};
    use crate::code_blocks::fragment::Kind;
    use crate::code_blocks::rust::LineMap;
    use crate::code_blocks::tools::{CARTRIDGES, ToolCartridge};
    use crate::finding::{Finding, Location};
    use crate::read::cartridge::{Cartridge, SourceFile};

    fn ruff() -> &'static ToolCartridge {
        CARTRIDGES
            .iter()
            .find(|tool| tool.name == "ruff")
            .expect("the cartridges should include ruff")
    }

    /// How the lint step reads ruff's run on `block` in `text`, framed by `frame`.
    fn read<'run>(block: &'run CodeBlock, text: &'run str, frame: Option<Frame>) -> Read<'run> {
        Read {
            tool: ruff(),
            block,
            text,
            frame,
        }
    }

    fn diagnostic(line: usize, column: usize) -> Diagnostic {
        Diagnostic {
            line,
            column,
            end_line: line,
            syntax: false,
            rule: "E711".to_owned(),
            message: "comparison to None".to_owned(),
        }
    }

    /// The one block of `text`, read as `/repo/<source>` with `cartridge`.
    fn only_block(source: &str, text: &str, cartridge: &'static dyn Cartridge) -> CodeBlock {
        let path = crate::path::fixture(&format!("/repo/{source}"));
        let file = SourceFile::parse(path, text.to_owned(), cartridge)
            .expect("tree-sitter should return a tree");
        let mut blocks = blocks_in(&file, |range| {
            let source = source.parse().expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        });
        assert_eq!(blocks.len(), 1, "blocks: {blocks:?}");
        blocks.remove(0)
    }

    /// The one block of `text`, a Markdown doc.
    fn markdown_block(text: &str) -> CodeBlock {
        only_block("docs/a.md", text, markdown())
    }

    #[test]
    fn a_diagnostic_in_a_list_item_lands_past_the_items_indent() {
        let text = "# D\n\n- item\n\n  ```py\n  x = 1\n  y == None\n  ```\n";
        let block = markdown_block(text);
        let map = LineMap::of(&block, text).expect("the block's lines should be in the doc");

        let at = place(&diagnostic(2, 3), &map, text).expect("line 2 is the block's");

        assert_eq!((at.line, at.column), (7, 5));
        assert_eq!(text.get(at.range.start..at.range.start + 4), Some("== N"));
    }

    #[test]
    fn a_diagnostic_in_a_doc_comment_lands_past_the_comment_prefix() {
        let text = "/// Adds.\n///\n/// ```py\n/// x == None\n/// ```\nfn f() {}\n";
        let block = only_block(
            "src/lib.rs",
            text,
            rust().expect("the built-in Rust queries should compile"),
        );
        let map = LineMap::of(&block, text).expect("the block's lines should be in the doc");

        let at = place(&diagnostic(1, 3), &map, text).expect("line 1 is the block's");

        assert_eq!((at.line, at.column), (4, 7));
    }

    #[test]
    fn a_diagnostic_on_a_hash_line_lands_at_its_written_column() {
        let text = "```rust\n# let x = 1;\nlet y = x;\n```\n";
        let block = markdown_block(text);

        let found = findings(
            &Verdict::Diagnostics(vec![diagnostic(1, 5)]),
            &read(&block, text, None),
            None,
            false,
        );

        let [finding] = found.as_slice() else {
            panic!("a `# ` line is ordinary code, so its diagnostic counts: {found:?}");
        };
        let at = finding
            .location
            .as_ref()
            .expect("the finding should be placed");
        assert_eq!((at.line, at.column), (2, 5));
        assert!(finding.fix.is_none(), "a lint finding never carries a fix");
    }

    #[test]
    fn a_lint_finding_names_its_rule_and_lint_off() {
        let text = "```py\nx == None\n```\n";
        let block = markdown_block(text);

        let found = findings(
            &Verdict::Diagnostics(vec![diagnostic(1, 1)]),
            &read(&block, text, None),
            None,
            false,
        );

        let help = found
            .first()
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default();
        assert!(
            help.contains("`E711`") && help.contains("lint(off: <reason>)"),
            "{help}"
        );
    }

    #[test]
    fn a_missing_name_is_one_finding_with_the_externs_wording_and_help() {
        let text = "```rust\nlet a = 1;\nnope();\n```\n";
        let block = markdown_block(text);
        let missing = Diagnostic {
            line: 2,
            column: 1,
            end_line: 2,
            syntax: false,
            rule: "E0425".to_owned(),
            message: "cannot find function `nope` in this scope".to_owned(),
        };
        let verdict = Verdict::Diagnostics(vec![missing]);

        let without = findings(&verdict, &read(&block, text, None), None, false);
        let with = findings(&verdict, &read(&block, text, None), None, true);

        let [finding] = without.as_slice() else {
            panic!("one missing name is one finding: {without:?}");
        };
        assert_eq!(finding.code, "code-block-missing-name");
        assert_eq!(finding.message, "`nope` isn't defined in this block");
        let at = finding
            .location
            .as_ref()
            .expect("the finding should be placed");
        assert_eq!(at.line, 3);
        let help = finding.help.as_deref().unwrap_or_default();
        assert!(
            help.contains("from(<path>#<symbol>)") && help.contains("lint(off: <reason>)"),
            "{help}"
        );
        assert_eq!(
            with.first().map(|finding| finding.message.as_str()),
            Some("`nope` isn't defined in this block or the project's crate")
        );
    }

    #[test]
    fn a_non_missing_name_error_stays_a_lint_finding() {
        let text = "```rust\nx\n```\n";
        let block = markdown_block(text);
        let other = Diagnostic {
            line: 1,
            column: 1,
            end_line: 1,
            syntax: false,
            rule: "E0308".to_owned(),
            message: "mismatched types".to_owned(),
        };

        let found = findings(
            &Verdict::Diagnostics(vec![other]),
            &read(&block, text, None),
            None,
            true,
        );

        assert_eq!(found.first().map(|f| f.code), Some("code-block-lint"));
    }

    /// The frame ruff never uses, around map entries.
    const ENTRIES: Frame = Frame {
        kind: Kind::Entries,
        before: "({",
        after: "})",
        out_before: "({",
        out_after: "});",
    };

    /// The frame clippy is planned to use around statements (design §2.6).
    const STATEMENTS: Frame = Frame {
        kind: Kind::Statements,
        before: "fn main() {",
        after: "}",
        out_before: "fn main() {",
        out_after: "}",
    };

    /// A diagnostic from line `line` to `end_line`, column 1, breaking `rule`, a syntax error
    /// when `syntax`, saying `message`.
    fn spanning(
        line: usize,
        end_line: usize,
        rule: &str,
        syntax: bool,
        message: &str,
    ) -> Diagnostic {
        Diagnostic {
            line,
            column: 1,
            end_line,
            syntax,
            rule: rule.to_owned(),
            message: message.to_owned(),
        }
    }

    /// A one-line diagnostic at `line` breaking `rule`, with a message holding no frame text.
    fn on_line(line: usize, rule: &str, syntax: bool) -> Diagnostic {
        spanning(line, line, rule, syntax, "the tool's message")
    }

    /// The code and `(line, column)` of each of `found`.
    fn placed(found: &[Finding]) -> Vec<(&'static str, (u32, u32))> {
        found
            .iter()
            .map(|finding| {
                let at = finding
                    .location
                    .as_ref()
                    .expect("every lint finding should be placed");
                (finding.code, (at.line, at.column))
            })
            .collect()
    }

    /// The findings for `diagnostics` from ruff on `text`'s one block, framed by `frame`.
    fn framed_findings(text: &str, frame: Frame, diagnostics: Vec<Diagnostic>) -> Vec<Finding> {
        let block = markdown_block(text);
        findings(
            &Verdict::Diagnostics(diagnostics),
            &read(&block, text, Some(frame)),
            None,
            false,
        )
    }

    /// Asserts that no message, help or location text of `found` in `text` holds a line of
    /// `frame`.
    fn assert_no_frame_line(found: &[Finding], text: &str, frame: Frame) {
        for finding in found {
            let at = finding
                .location
                .as_ref()
                .expect("every lint finding should be placed");
            let shown = [
                finding.message.as_str(),
                finding.help.as_deref().unwrap_or_default(),
                text.get(at.range.clone()).unwrap_or_default(),
            ];
            for frame_line in frame.before.lines().chain(frame.after.lines()) {
                assert!(
                    shown.iter().all(|part| !part.contains(frame_line)),
                    "{frame_line:?} in {finding:?}"
                );
            }
            assert!(finding.fix.is_none(), "a lint finding never carries a fix");
        }
    }

    /// A doc whose one `ts` block, at doc lines 4 and 5, declares `fragment(entries)`.
    const TWO_ENTRIES: &str = "# D\n\n```ts fragment(entries)\na: 1,\nb: 2,\n```\n";

    #[test]
    fn a_frame_wraps_the_block_in_its_lines() {
        assert_eq!(ENTRIES.wrap("a: 1,\nb: 2,\n"), "({\na: 1,\nb: 2,\n})\n");
        let header = Frame {
            kind: Kind::Declarations,
            before: "package fragment",
            after: "",
            out_before: "package fragment",
            out_after: "",
        };
        assert_eq!(header.wrap("var x = 1\n"), "package fragment\nvar x = 1\n");
    }

    #[test]
    fn a_tool_reading_the_kind_as_written_has_no_frame() {
        assert_eq!(Frame::of(ruff(), Some(Kind::Statements)), None);
        assert_eq!(Frame::of(ruff(), Some(Kind::Entries)), None);
        assert_eq!(Frame::of(ruff(), None), None);
    }

    #[test]
    fn a_framed_diagnostic_lands_at_its_doc_line_past_the_frame() {
        let found = framed_findings(TWO_ENTRIES, ENTRIES, vec![on_line(3, "E711", false)]);

        assert_eq!(placed(&found), [("code-block-lint", (5, 1))]);
    }

    #[test]
    fn a_diagnostic_naming_no_line_stays_at_the_fence() {
        let found = framed_findings(TWO_ENTRIES, ENTRIES, vec![on_line(0, "E0308", false)]);

        assert_eq!(placed(&found), [("code-block-lint", (3, 1))]);
    }

    #[test]
    fn a_syntax_error_on_a_frame_line_is_a_parse_error_at_the_attribute() {
        let found = framed_findings(TWO_ENTRIES, ENTRIES, vec![on_line(4, "parse", true)]);

        let [finding] = found.as_slice() else {
            panic!("a diagnostic on a frame line is never dropped: {found:?}");
        };
        assert_eq!(
            finding.message,
            "`ruff` couldn't parse the block as `entries`"
        );
        assert_eq!(placed(&found), [("code-block-parse", (3, 4))]);
    }

    #[test]
    fn a_frame_line_diagnostic_never_shows_the_tools_message_quoting_the_frame() {
        let text = "# D\n\n```rust fragment(statements)\nlet x = 1;\n}\n```\n";
        let brace = spanning(4, 4, "error", false, "unexpected closing delimiter: `}`");

        let found = framed_findings(text, STATEMENTS, vec![brace]);

        let [finding] = found.as_slice() else {
            panic!("a diagnostic on a frame line is never dropped: {found:?}");
        };
        assert_eq!(placed(&found), [("code-block-lint", (3, 4))]);
        assert!(finding.message.contains("`error`"), "{finding:?}");
        assert!(!finding.message.contains("delimiter"), "{finding:?}");
        assert_no_frame_line(&found, text, STATEMENTS);
    }

    #[test]
    fn any_other_diagnostic_on_a_frame_line_names_its_rule_at_the_attribute() {
        let found = framed_findings(TWO_ENTRIES, ENTRIES, vec![on_line(1, "E711", false)]);

        assert_eq!(placed(&found), [("code-block-lint", (3, 4))]);
        let message = found.first().map(|finding| finding.message.as_str());
        assert_eq!(
            message,
            Some(
                "`ruff` reports `E711` where the block meets the frame supdoc adds around an \
                 entries fragment"
            )
        );
    }

    #[test]
    fn a_diagnostic_that_runs_from_the_block_into_the_frame_goes_to_the_attribute() {
        let found = framed_findings(
            TWO_ENTRIES,
            ENTRIES,
            vec![spanning(3, 4, "E711", false, "the tool's message")],
        );

        assert_eq!(placed(&found), [("code-block-lint", (3, 4))]);
    }

    #[test]
    fn no_finding_holds_a_frame_line_whichever_line_a_diagnostic_is_on() {
        let checked = ENTRIES.wrap("a: 1,\nb: 2,\n");
        let every_line: Vec<Diagnostic> = (1..=checked.lines().count())
            .map(|line| on_line(line, "E711", false))
            .collect();

        let found = framed_findings(TWO_ENTRIES, ENTRIES, every_line.clone());

        assert_eq!(found.len(), every_line.len(), "none dropped: {found:?}");
        assert_no_frame_line(&found, TWO_ENTRIES, ENTRIES);
    }

    #[test]
    fn a_framed_run_that_didnt_parse_is_one_parse_error_at_the_first_ones_doc_line() {
        let found = framed_findings(
            TWO_ENTRIES,
            ENTRIES,
            vec![
                spanning(3, 3, "parse", true, "expected `)` but instead found `;`"),
                on_line(4, "parse", true),
                on_line(3, "E711", false),
                on_line(1, "E711", false),
            ],
        );

        assert_eq!(
            placed(&found),
            [("code-block-parse", (5, 1))],
            "a run that didn't parse as the kind gives nothing but the parse error"
        );
        let message = found.first().map(|finding| finding.message.as_str());
        assert_eq!(
            message,
            Some("`ruff` couldn't parse the block as `entries`"),
            "the tool's syntax message, which names the frame's `)`, is never shown"
        );
    }

    #[test]
    fn a_framed_tool_failure_shows_no_line_of_the_frame() {
        let block = markdown_block(TWO_ENTRIES);
        let stderr = "error: unexpected token\n  ({\na: 1,\n})\nend";
        let failed = Verdict::Failed(super::format::Verdict::ToolFailed(stderr.to_owned()));

        let found = findings(
            &failed,
            &read(&block, TWO_ENTRIES, Some(ENTRIES)),
            None,
            false,
        );
        let plain = findings(&failed, &read(&block, TWO_ENTRIES, None), None, false);

        let message = found.first().map(|finding| finding.message.as_str());
        let message = message.unwrap_or_default();
        assert!(
            message.contains(
                "error: unexpected token\n<the frame supdoc adds>\na: 1,\n<the frame supdoc adds>\nend"
            ),
            "{message}"
        );
        assert_no_frame_line(&found, TWO_ENTRIES, ENTRIES);
        let unframed = plain.first().map(|finding| finding.message.as_str());
        assert!(unframed.unwrap_or_default().contains("  ({"), "{plain:?}");
    }

    #[test]
    fn a_framed_tool_failure_hides_a_frame_line_named_by_its_gutter_or_location() {
        let block = markdown_block(TWO_ENTRIES);
        let stderr = "error: unexpected token\n --> <stdin>:1:1\n  |\n1 | ({\n2 | a: 1,\n  \
                      > 4 \u{2502} })\nend";
        let failed = Verdict::Failed(super::format::Verdict::ToolFailed(stderr.to_owned()));

        let found = findings(
            &failed,
            &read(&block, TWO_ENTRIES, Some(ENTRIES)),
            None,
            false,
        );
        let plain = findings(&failed, &read(&block, TWO_ENTRIES, None), None, false);

        let message = found.first().map(|finding| finding.message.as_str());
        let message = message.unwrap_or_default();
        assert!(
            message.contains(
                "error: unexpected token\n<the frame supdoc adds>\n  |\n<the frame supdoc adds>\n\
                 1 | a: 1,\n<the frame supdoc adds>\nend"
            ),
            "{message}"
        );
        assert_no_frame_line(&found, TWO_ENTRIES, ENTRIES);
        let unframed = plain.first().map(|finding| finding.message.as_str());
        assert!(unframed.unwrap_or_default().contains("1 | ({"), "{plain:?}");
    }

    #[test]
    fn a_framed_tool_failure_keeps_the_authors_own_closing_line() {
        let text = "# D\n\n```rust fragment(statements)\nif x {\n}\n```\n";
        let block = markdown_block(text);
        let stderr = "error: x\n --> <stdin>:3:1\n3 | }\n4 | }";
        let failed = Verdict::Failed(super::format::Verdict::ToolFailed(stderr.to_owned()));

        let found = findings(&failed, &read(&block, text, Some(STATEMENTS)), None, false);

        let message = found.first().map(|finding| finding.message.as_str());
        assert!(
            message
                .unwrap_or_default()
                .contains("error: x\n --> <stdin>:2:1\n2 | }\n<the frame supdoc adds>"),
            "the block's own `}}` on line 3 stays, the frame's on line 4 goes: {found:?}"
        );
    }
}
