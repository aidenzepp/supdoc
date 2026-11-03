//! Code blocks in Rust doc comments: the `///` and `//!` containers.
//!
//! Each run of adjacent doc-comment lines is one Markdown document: the lines without their
//! marker and the one space after it. That text is parsed with the Markdown block grammar, and
//! every offset it reports is mapped back to the file, so a block's prefix is the line's
//! indentation, its marker and its space. `//`, `/* */` and `/** */` comments are not containers.

use std::ops::Range;

use crate::cartridges::markdown::cartridge::cartridge as markdown_cartridge;
use crate::finding::Location;
use crate::read::cartridge::{MarkdownRegion, SourceFile};

use super::block::{BodyLine, CodeBlock};
use super::markdown;

/// The markers of a doc comment.
const DOC_MARKERS: [&str; 2] = ["///", "//!"];

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

/// Every code block in the doc comments of `file`, a Rust file, in file order.
pub(super) fn blocks(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Vec<CodeBlock> {
    let Ok(regions) = file.language().markdown_regions(file) else {
        return Vec::new();
    };
    regions
        .iter()
        .flat_map(|region| region_blocks(file, region, &locate))
        .collect()
}

/// The blocks of `region` when it is a run of `///` or `//!` lines.
fn region_blocks(
    file: &SourceFile,
    region: &MarkdownRegion,
    locate: &impl Fn(Range<usize>) -> Option<Location>,
) -> Vec<CodeBlock> {
    let text = file.text();
    let Some((run, markdown)) = read_run(text, region) else {
        return Vec::new();
    };
    let Ok(parsed) = SourceFile::from_text(
        file.path().clone(),
        markdown,
        markdown_cartridge(),
        file.deadline(),
    ) else {
        return Vec::new();
    };
    let mapped = |range| run.range_to_file(range).and_then(locate);
    markdown::blocks(&parsed, mapped)
        .into_iter()
        .filter_map(|block| to_file(&run, block))
        .collect()
}

/// `region` as a [`Run`] and its Markdown text, or `None` when it isn't a doc-comment run.
fn read_run(text: &str, region: &MarkdownRegion) -> Option<(Run, String)> {
    let mut lines = Vec::new();
    let mut markdown = String::new();
    for range in region.ranges() {
        let marker = text.get(range.start_byte.checked_sub(3)?..range.start_byte)?;
        let body = text.get(range.start_byte..range.end_byte)?;
        // `////` opens an ordinary comment.
        if !DOC_MARKERS.contains(&marker) || (marker == "///" && body.starts_with('/')) {
            return None;
        }
        let space = usize::from(body.starts_with(' '));
        let begins = text
            .get(..range.start_byte - 3)?
            .rfind('\n')
            .map_or(0, |at| at + 1);
        let content = body.get(space..)?;
        lines.push(Line {
            synthetic: markdown.len(),
            file: range.start_byte + space,
            begins,
        });
        markdown.push_str(content);
    }
    Some((Run { lines }, markdown))
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
    use std::path::Path;

    use super::*;
    use crate::cartridges::rust::cartridge;
    use crate::code_blocks::block::{blocks_in, replacement_for};
    use crate::path::AbsolutePath;

    /// The blocks of `text` parsed as `/repo/src/lib.rs`.
    fn blocks_of(text: &str) -> Vec<CodeBlock> {
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");
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
}
