//! The doc-comment container core: code blocks in the comments of a language.
//!
//! A language's run builder turns each run of doc-comment lines into Markdown text and a
//! [`Run`], the per-line map back to the file. The core parses that text with the Markdown block
//! grammar and maps every offset it reports to the file, so a block's prefix is the line's
//! indentation and marker. A container never rewrites a line's bytes after its prefix.

mod go;
mod jsdoc;
mod python;
mod rust;

use std::ops::Range;

use crate::cartridges::markdown::cartridge::cartridge as markdown_cartridge;
use crate::finding::Location;
use crate::read::cartridge::SourceFile;
use crate::read::error::ReadError;

use super::block::{BodyLine, CodeBlock};
use super::markdown;

pub(super) use go::blocks as go_blocks;
pub(super) use jsdoc::blocks as jsdoc_blocks;
pub(super) use python::blocks as python_blocks;
pub(super) use rust::blocks;

/// One line of a doc comment's Markdown text, and where it came from.
struct Line {
    /// Where the line's text starts in the Markdown text.
    synthetic: usize,
    /// Where the line's text starts in the file, after the marker and its space.
    file: usize,
    /// Where the line starts in the file, before its indentation.
    begins: usize,
}

/// A doc-comment run read as Markdown, with the map back to the file.
struct Run {
    /// The lines, in order.
    lines: Vec<Line>,
}

impl Run {
    /// The line holding the Markdown offset `offset`.
    fn line_at(&self, offset: usize) -> Option<&Line> {
        let after = self.lines.partition_point(|line| line.synthetic <= offset);
        self.lines.get(after.checked_sub(1)?)
    }

    /// The file offset of the Markdown offset `offset`.
    fn to_file(&self, offset: usize) -> Option<usize> {
        let line = self.line_at(offset)?;
        Some(line.file + (offset - line.synthetic))
    }

    /// The file range of the Markdown range `range`. Its end is mapped from the last byte it
    /// covers, so an end on a line start doesn't land on the next line's text.
    fn range_to_file(&self, range: Range<usize>) -> Option<Range<usize>> {
        let start = self.to_file(range.start)?;
        let end = if range.end > range.start {
            self.to_file(range.end - 1)? + 1
        } else {
            start
        };
        Some(start..end)
    }
}

/// The blocks of every run in `runs`, each a [`Run`] and its Markdown text, in order.
///
/// # Errors
///
/// The [`ReadError`] of the first run whose Markdown, or its NUL-replaced re-parse, can't be
/// parsed: the file's blocks are then unknown, not none.
fn blocks_of_runs(
    file: &SourceFile,
    runs: Vec<(Run, String)>,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let mut blocks = Vec::new();
    for (run, markdown) in runs {
        let parsed = SourceFile::from_text(
            file.path().clone(),
            markdown,
            markdown_cartridge(),
            file.deadline(),
        );
        blocks.extend(blocks_of_run(&run, parsed, &locate)?);
    }
    Ok(blocks)
}

/// The blocks of `run`, read from `parsed`, the parse of its Markdown text.
///
/// # Errors
///
/// The [`ReadError`] `parsed` failed with, or that of its NUL-replaced re-parse.
fn blocks_of_run(
    run: &Run,
    parsed: Result<SourceFile, ReadError>,
    locate: &impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let mapped = |range| run.range_to_file(range).and_then(locate);
    Ok(markdown::try_blocks(&parsed?, mapped)?
        .into_iter()
        .filter_map(|block| to_file(run, block))
        .collect())
}

/// `block`, found in the Markdown text, with its body lines placed in the file. Its fence and
/// info were located in the file already.
fn to_file(run: &Run, block: CodeBlock) -> Option<CodeBlock> {
    let body = block
        .body
        .into_iter()
        .map(|line| {
            let found = run.line_at(line.prefix.start)?;
            let at = |offset: usize| found.file + (offset - found.synthetic);
            Some(BodyLine {
                prefix: found.begins..at(line.content.start),
                content: at(line.content.start)..at(line.content.end),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(CodeBlock { body, ..block })
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cartridges::rust::cartridge;
    use crate::code_blocks::block::{blocks_in, replacement_for};

    /// The blocks of `text` parsed as `/repo/src/lib.rs`.
    fn blocks_of(text: &str) -> Vec<CodeBlock> {
        let path = crate::path::fixture("/repo/src/lib.rs");
        let rust = cartridge().expect("the built-in Rust queries should compile");
        let file = SourceFile::parse(path, text.to_owned(), rust)
            .expect("tree-sitter should return a tree");
        blocks_in(&file, |range| {
            let source = "src/lib.rs"
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        })
    }

    /// The text of `range` in `text`.
    fn slice<'text>(text: &'text str, range: &Range<usize>) -> &'text str {
        text.get(range.clone())
            .expect("a block range should lie inside the text")
    }

    /// The one block of `text`.
    fn only(text: &str) -> CodeBlock {
        let mut blocks = blocks_of(text);
        assert_eq!(blocks.len(), 1, "blocks: {blocks:?}");
        blocks.remove(0)
    }

    /// `line`'s prefix and content in `text`, for each body line of `block`.
    fn lines<'text>(text: &'text str, block: &CodeBlock) -> Vec<(&'text str, &'text str)> {
        block
            .body
            .iter()
            .map(|line| (slice(text, &line.prefix), slice(text, &line.content)))
            .collect()
    }

    const OUTER: &str =
        "/// Adds.\n///\n/// ```rust from(/src/lib.rs#add)\n/// old\n/// ```\npub fn add() {}\n";

    #[test]
    fn offsets_land_on_the_right_file_lines() {
        let block = only(OUTER);

        assert_eq!(block.fence.line, 3, "the fence opens on line 3");
        assert_eq!(
            slice(OUTER, &block.fence.range),
            "```rust from(/src/lib.rs#add)\n/// old\n/// ```"
        );
        let info = block.info.as_ref().expect("the fence has an info string");
        assert_eq!(info.text, "rust from(/src/lib.rs#add)");
        assert_eq!(lines(OUTER, &block), [("/// ", "old")]);
    }

    #[test]
    fn an_inner_doc_run_is_read() {
        let text = "//! ```sh\n//! echo hi\n//! ```\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [("//! ", "echo hi")]);
    }

    #[test]
    fn plain_and_block_comments_are_not_containers() {
        let text =
            "// ```rust\n// x\n// ```\n/* ```rust\nx\n``` */\n/** ```rust\nx\n``` */\nfn f() {}\n";

        assert_eq!(blocks_of(text), Vec::new());
    }

    #[test]
    fn an_untagged_fence_is_rust() {
        let block = only("/// ```\n/// x\n/// ```\nfn f() {}\n");

        assert_eq!(block.info, None);
        assert_eq!(block.default_language(), Some("rust"));
    }

    #[test]
    fn a_replacement_keeps_a_methods_indentation() {
        let text = "impl T {\n    /// Doc.\n    ///\n    /// ```rust\n    /// old\n    /// ```\n    fn f() {}\n}\n";
        let block = only(text);

        let (range, replacement) =
            replacement_for(&block, text, "a\n\nb").expect("the block has a body to replace");

        assert_eq!(replacement, "    /// a\n    ///\n    /// b");
        assert_eq!(slice(text, &range), "    /// old");
    }

    #[test]
    fn a_blank_doc_line_inside_a_block_stays_in_the_block() {
        let text = "/// ```rust\n/// a\n///\n/// b\n/// ```\n";

        let block = only(text);

        assert_eq!(
            lines(text, &block),
            [("/// ", "a"), ("///", ""), ("/// ", "b")]
        );
    }

    #[test]
    fn a_block_spanning_a_plain_comment_line_is_found_on_its_file_lines() {
        let text = "/// ```rust\n/// a\n// not doc\n/// b\n/// ```\nfn f() {}\n";

        let block = only(text);

        assert_eq!(block.fence.line, 1, "the fence opens on line 1");
        assert_eq!(
            slice(text, &block.fence.range),
            "```rust\n/// a\n// not doc\n/// b\n/// ```"
        );
        assert_eq!(lines(text, &block), [("/// ", "a"), ("/// ", "b")]);
    }

    #[test]
    fn a_block_spanning_a_plain_comment_line_gets_no_replacement() {
        let text = "/// ```rust\n/// a\n// keep me\n/// b\n/// ```\nfn f() {}\n";
        let block = only(text);

        assert_eq!(
            replacement_for(&block, text, "x\ny"),
            None,
            "replacing the body would delete the `// keep me` line"
        );
    }

    #[test]
    fn a_block_after_a_quadruple_slash_line_is_found_on_its_file_lines() {
        let text = "/// Doc.\n//// not doc\n/// ```rust\n/// x\n/// ```\nfn f() {}\n";

        let block = only(text);

        assert_eq!(block.fence.line, 3, "the fence opens on line 3");
        assert_eq!(lines(text, &block), [("/// ", "x")]);
    }

    #[test]
    fn a_failed_markdown_parse_of_a_run_is_an_error_never_zero_blocks() {
        let path = crate::path::fixture("/repo/src/lib.rs");
        let failed = Err(ReadError::Timeout {
            path,
            limit: "10s".parse().expect("10s is a valid limit"),
            step: "the parse",
            offset: 0,
            size: 0,
        });
        let run = Run { lines: Vec::new() };

        let result = blocks_of_run(&run, failed, &|_| None);

        assert!(
            matches!(result, Err(ReadError::Timeout { .. })),
            "{result:?}"
        );
    }
}
