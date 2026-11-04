//! The lint step: a linter's diagnostics on a block, each mapped from the checked text to its
//! line and column in the doc.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::config::code_blocks::Language;
use crate::finding::{Finding, Location, Severity};

use super::block::CodeBlock;
use super::format;
use super::runner::{Failure, Ran};
use super::rust::LineMap;
use super::tools::Tool;
use super::{python, rust, shell};

/// The code for a linter's diagnostic on a block.
const LINT: &str = "code-block-lint";

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

/// `tool`'s lint arguments for the `index`th block (1-based) of `doc`, the doc's absolute path.
/// `None` while `tool`'s language has no lint step yet.
#[must_use]
pub(crate) fn args(tool: &Tool, doc: &Path, index: usize) -> Option<Vec<OsString>> {
    match tool.language {
        Language::Python => python::lint_args(tool, doc, index),
        Language::Shell => shell::lint_args(tool, doc, index),
        Language::Rust => rust::lint_args(tool, doc, index),
    }
}

/// `tool`'s diagnostics in a run's `stdout`. `None` when it isn't the tool's report.
fn diagnostics(tool: &Tool, stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    match tool.language {
        Language::Python => python::diagnostics(stdout),
        Language::Shell | Language::Rust => None,
    }
}

/// Judges a linter's `result`: exit 0 is clean, a non-zero exit whose stdout is the tool's
/// report of at least one diagnostic gives those diagnostics, and any other non-zero exit, output
/// past the cap or a failed start is a tool failure; a timeout is a timeout.
#[must_use]
pub(crate) fn judge(tool: &Tool, result: Result<Ran, Failure>) -> Verdict {
    match result {
        Ok(ran) if ran.success => Verdict::Clean,
        Ok(ran) => match diagnostics(tool, &ran.stdout) {
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

/// The findings for `verdict`, from `tool` on `block` in `text`: one `code-block-lint` error per
/// diagnostic at its place in the doc (the fence when it can't be
/// placed), naming its rule and `no_lint(<reason>)`, never with a fix; or the tool failure or
/// timeout error at the fence, run under `timeout`.
#[must_use]
pub(crate) fn findings(
    verdict: &Verdict,
    tool: &Tool,
    block: &CodeBlock,
    text: &str,
    timeout: Option<Duration>,
) -> Vec<Finding> {
    match verdict {
        Verdict::Clean => Vec::new(),
        Verdict::Failed(failed) => format::finding(failed, tool, &block.fence, timeout)
            .into_iter()
            .collect(),
        Verdict::Diagnostics(found) => {
            let map = LineMap::of(block, text);
            found
                .iter()
                .map(|diagnostic| {
                    let at = map
                        .as_ref()
                        .and_then(|map| place(diagnostic, map, text))
                        .unwrap_or_else(|| block.fence.clone());
                    lint_finding(tool, diagnostic).with_location(at)
                })
                .collect()
        }
    }
}

/// The `code-block-lint` error for `diagnostic` from `tool`, without a location.
fn lint_finding(tool: &Tool, diagnostic: &Diagnostic) -> Finding {
    let Diagnostic { rule, message, .. } = diagnostic;
    Finding::new(
        LINT,
        Severity::Error,
        format!("`{}` reports `{rule}`: {message}", tool.name),
    )
    .with_help(format!(
        "fix the code `{rule}` flags, or mark the block `no_lint(<reason>)` to skip its lint step"
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Diagnostic, Verdict, findings, place};
    use crate::cartridges::markdown::cartridge::cartridge as markdown;
    use crate::cartridges::rust::cartridge as rust;
    use crate::code_blocks::block::{CodeBlock, blocks_in};
    use crate::code_blocks::rust::LineMap;
    use crate::code_blocks::tools::{TOOLS, Tool};
    use crate::finding::Location;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, SourceFile};

    fn ruff() -> &'static Tool {
        TOOLS
            .iter()
            .find(|tool| tool.name == "ruff")
            .expect("the tool table should list ruff")
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
            ruff(),
            &block,
            text,
            None,
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
    fn a_lint_finding_names_its_rule_and_no_lint() {
        let text = "```py\nx == None\n```\n";
        let block = markdown_block(text);

        let found = findings(
            &Verdict::Diagnostics(vec![diagnostic(1, 1)]),
            ruff(),
            &block,
            text,
            None,
        );

        let help = found
            .first()
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default();
        assert!(
            help.contains("`E711`") && help.contains("no_lint(<reason>)"),
            "{help}"
        );
    }
}
