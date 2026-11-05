//! The lint step: a linter's diagnostics on a block, each mapped from the checked text to its
//! line and column in the doc.

use std::borrow::Cow;
use std::time::Duration;

use crate::finding::{Finding, Location, Severity};

use super::block::CodeBlock;
use super::format;
use super::fragment::Kind;
use super::runner::{Failure, Ran};
use super::rust::LineMap;
use super::tools::ToolCartridge;

/// The code for a linter's diagnostic on a block.
const LINT: &str = "code-block-lint";

/// The code for a name a block uses but neither it nor the project's crate defines.
const MISSING_NAME: &str = "code-block-missing-name";

/// The rustc error codes that say a name isn't defined: an unresolved type, trait, struct,
/// function, import, path, crate or pattern name.
const MISSING_NAME_CODES: [&str; 9] = [
    "E0405", "E0412", "E0422", "E0425", "E0432", "E0433", "E0463", "E0531", "E0532",
];

/// The rules linters name a syntax error with: ruff's `syntax-error` and biome's `parse`.
const PARSE_RULES: [&str; 2] = ["syntax-error", "parse"];

/// The fixed lines a linter sees around a block that declares a kind its tool reads only inside
/// a frame (see [`super::fragment::Support::Framed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The kind the block declares.
    pub(crate) kind: Kind,
    /// The lines before the block. Empty for none.
    pub(crate) before: &'static str,
    /// The lines after the block. Empty for none.
    pub(crate) after: &'static str,
}

impl Frame {
    /// The frame `tool` puts around a block of `kind`. `None` for a block checked as a whole
    /// file, and for a kind the tool reads as written or doesn't list.
    #[must_use]
    pub(crate) fn of(tool: &ToolCartridge, kind: Option<Kind>) -> Option<Self> {
        let kind = kind?;
        let (_, support) = tool.fragments.iter().find(|(listed, _)| *listed == kind)?;
        let (before, after) = support.frame()?;
        Some(Self {
            kind,
            before,
            after,
        })
    }

    /// The text the linter checks for a block whose `input` is its body as written: `before`,
    /// the body and `after`, each starting on its own line.
    #[must_use]
    pub(crate) fn wrap(self, input: &str) -> String {
        let mut text =
            String::with_capacity(self.before.len() + input.len() + self.after.len() + 2);
        for part in [self.before, input, self.after] {
            if part.is_empty() {
                continue;
            }
            text.push_str(part);
            if !part.ends_with('\n') {
                text.push('\n');
            }
        }
        text
    }

    /// How many lines `before` takes.
    fn lead(self) -> usize {
        self.before.lines().count()
    }
}

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
/// A framed block's diagnostics are shifted past the frame's leading lines. One on a frame line
/// is never dropped: it is reported at the block's info string, a syntax error as
/// `code-block-parse` "couldn't parse the block as `<kind>`", any other with the tool's rule and
/// message. No location, message or help supdoc writes holds frame text.
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
        Verdict::Clean => Vec::new(),
        Verdict::Failed(failed) => format::finding(failed, tool, &block.fence, timeout)
            .into_iter()
            .collect(),
        Verdict::Diagnostics(found) => {
            let map = LineMap::of(block, text);
            let lines = block.body.len();
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
                    let at = map
                        .as_ref()
                        .and_then(|map| place(diagnostic, map, text))
                        .unwrap_or_else(|| block.fence.clone());
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
/// holding `lines` lines.
///
/// # Errors
///
/// The frame, when `diagnostic` falls on one of its lines.
fn unframed(
    diagnostic: &Diagnostic,
    frame: Option<Frame>,
    lines: usize,
) -> Result<Diagnostic, Frame> {
    let Some(frame) = frame else {
        return Ok(diagnostic.clone());
    };
    let line = diagnostic
        .line
        .checked_sub(frame.lead())
        .filter(|line| (1..=lines).contains(line))
        .ok_or(frame)?;
    Ok(Diagnostic {
        line,
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

/// The finding, without a location, for `diagnostic` from `tool` on a line of `frame`: a syntax
/// error says the block isn't `frame`'s kind; any other keeps the tool's rule and message.
fn on_frame(tool: &ToolCartridge, frame: Frame, diagnostic: &Diagnostic) -> Finding {
    if !PARSE_RULES.contains(&diagnostic.rule.as_str()) {
        return lint_finding(tool, diagnostic);
    }
    let kind = frame.kind.name();
    Finding::new(
        format::PARSE,
        Severity::Error,
        format!("`{}` couldn't parse the block as `{kind}`", tool.name),
    )
    .with_help(format!(
        "fix the code so it reads as {} fragment, name the kind it is in `fragment(<kind>)`, \
         or mark the block `lint(off: <reason>)`",
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
    use std::path::Path;

    use super::{Diagnostic, Frame, Read, Verdict, findings, place};
    use crate::cartridges::markdown::cartridge::cartridge as markdown;
    use crate::cartridges::rust::cartridge as rust;
    use crate::code_blocks::block::{CodeBlock, blocks_in};
    use crate::code_blocks::fragment::Kind;
    use crate::code_blocks::rust::LineMap;
    use crate::code_blocks::tools::{CARTRIDGES, ToolCartridge};
    use crate::finding::{Finding, Location};
    use crate::path::AbsolutePath;
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
            rule: "E711".to_owned(),
            message: "comparison to None".to_owned(),
        }
    }

    /// The one block of `text`, read as `/repo/<source>` with `cartridge`.
    fn only_block(source: &str, text: &str, cartridge: &'static dyn Cartridge) -> CodeBlock {
        let path = AbsolutePath::new(&Path::new("/repo").join(source))
            .expect("the fixture path should be absolute");
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

    /// A frame no block line or tool message in these tests contains.
    const ENTRIES: Frame = Frame {
        kind: Kind::Entries,
        before: "({",
        after: "})",
    };

    /// A diagnostic breaking `rule` at `line`, column 1, with a message holding no frame text.
    fn diagnostic_named(rule: &str, line: usize) -> Diagnostic {
        Diagnostic {
            line,
            column: 1,
            rule: rule.to_owned(),
            message: "the tool's message".to_owned(),
        }
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

    #[test]
    fn a_frame_wraps_the_block_in_its_lines() {
        assert_eq!(ENTRIES.wrap("a: 1,\nb: 2,\n"), "({\na: 1,\nb: 2,\n})\n");
        let header = Frame {
            kind: Kind::Declarations,
            before: "package fragment",
            after: "",
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
        let text = "# D\n\n```ts fragment(entries)\na: 1,\nb: 2,\n```\n";
        let block = markdown_block(text);

        let found = findings(
            &Verdict::Diagnostics(vec![diagnostic_named("E711", 3)]),
            &read(&block, text, Some(ENTRIES)),
            None,
            false,
        );

        assert_eq!(placed(&found), [("code-block-lint", (5, 1))]);
    }

    #[test]
    fn a_syntax_error_on_a_frame_line_is_a_parse_error_at_the_attribute() {
        let text = "# D\n\n```ts fragment(entries)\na: 1,\n```\n";
        let block = markdown_block(text);

        let found = findings(
            &Verdict::Diagnostics(vec![diagnostic_named("syntax-error", 3)]),
            &read(&block, text, Some(ENTRIES)),
            None,
            false,
        );

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
    fn any_other_diagnostic_on_a_frame_line_keeps_the_tools_message_at_the_attribute() {
        let text = "# D\n\n```ts fragment(entries)\na: 1,\n```\n";
        let block = markdown_block(text);

        let found = findings(
            &Verdict::Diagnostics(vec![diagnostic_named("E711", 1)]),
            &read(&block, text, Some(ENTRIES)),
            None,
            false,
        );

        assert_eq!(placed(&found), [("code-block-lint", (3, 4))]);
        assert_eq!(
            found.first().map(|finding| finding.message.as_str()),
            Some("`ruff` reports `E711`: the tool's message")
        );
    }

    #[test]
    fn no_finding_holds_a_frame_line_whichever_line_a_diagnostic_is_on() {
        let text = "# D\n\n```ts fragment(entries)\na: 1,\nb: 2,\n```\n";
        let block = markdown_block(text);
        let checked = ENTRIES.wrap("a: 1,\nb: 2,\n");
        let every_line: Vec<Diagnostic> = (1..=checked.lines().count())
            .flat_map(|line| ["syntax-error", "E711"].map(|rule| diagnostic_named(rule, line)))
            .collect();

        let found = findings(
            &Verdict::Diagnostics(every_line.clone()),
            &read(&block, text, Some(ENTRIES)),
            None,
            false,
        );

        assert_eq!(found.len(), every_line.len(), "none dropped: {found:?}");
        for finding in &found {
            let at = finding
                .location
                .as_ref()
                .expect("every lint finding should be placed");
            let shown = [
                finding.message.as_str(),
                finding.help.as_deref().unwrap_or_default(),
                text.get(at.range.clone()).unwrap_or_default(),
            ];
            for frame_line in [ENTRIES.before, ENTRIES.after] {
                assert!(
                    shown.iter().all(|part| !part.contains(frame_line)),
                    "{frame_line:?} in {finding:?}"
                );
            }
            assert!(finding.fix.is_none(), "a lint finding never carries a fix");
        }
    }
}
