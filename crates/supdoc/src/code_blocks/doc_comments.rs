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
use crate::read::error::ReadError;

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
///
/// # Errors
///
/// The [`ReadError`] of the first run whose Markdown, or its NUL-replaced re-parse, can't be
/// parsed: the file's blocks are then unknown, not none.
pub(super) fn blocks(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let regions = file.language().markdown_regions(file)?;
    let mut blocks = Vec::new();
    for (run, markdown) in doc_runs(file.text(), &regions) {
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

/// What one comment range is to a doc-comment run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comment {
    /// A `///` or `//!` line.
    Doc {
        /// Its marker.
        marker: &'static str,
        /// Where the marker starts.
        start: usize,
    },
    /// A `//` or `////` line. Rustdoc skips it, so a run continues across it.
    Plain {
        /// Where its marker starts.
        start: usize,
    },
    /// Anything else, such as a block comment: it ends a run.
    Other,
}

/// What `range`, a comment body from a region, is to a doc-comment run.
fn classify(text: &str, range: &tree_sitter::Range) -> Comment {
    let at = range.start_byte;
    let before = |len: usize| text.get(at.checked_sub(len)?..at);
    let doc = DOC_MARKERS
        .into_iter()
        .find(|marker| before(3) == Some(*marker));
    let body = text.get(at..range.end_byte).unwrap_or_default();
    match doc {
        // `////` opens an ordinary comment.
        Some("///") if body.starts_with('/') => Comment::Plain { start: at - 3 },
        Some(marker) => Comment::Doc {
            marker,
            start: at - 3,
        },
        None if before(2) == Some("//") => Comment::Plain { start: at - 2 },
        None => Comment::Other,
    }
}

/// A doc-comment run being read: its marker, where its last line ends, its lines and Markdown.
struct Open {
    /// `///` or `//!`: a run never mixes them.
    marker: &'static str,
    /// Where the run's last line, doc or skipped, ends in the file.
    end: usize,
    /// The doc lines so far.
    lines: Vec<Line>,
    /// Their Markdown text.
    markdown: String,
}

/// Every doc-comment run in `regions`, as a [`Run`] and its Markdown text, in file order.
///
/// A run is adjacent `///` lines, or adjacent `//!` lines. A `//` or `////` line inside it is
/// skipped, as rustdoc skips it, and the run continues on the next line; a blank line, code, a
/// block comment or the other doc marker ends it.
fn doc_runs(text: &str, regions: &[MarkdownRegion]) -> Vec<(Run, String)> {
    let mut runs = Vec::new();
    let mut open: Option<Open> = None;
    let mut close = |open: &mut Option<Open>| {
        if let Some(done) = open.take() {
            runs.push((Run { lines: done.lines }, done.markdown));
        }
    };
    for range in regions.iter().flat_map(MarkdownRegion::ranges) {
        let comment = classify(text, range);
        let continues = |start: usize| {
            open.as_ref().is_some_and(|open| {
                text.get(open.end..start)
                    .is_some_and(|gap| gap.chars().all(|c| c.is_whitespace() && c != '\n'))
            })
        };
        match comment {
            Comment::Doc { marker, start } => {
                let joins = continues(start) && open.as_ref().is_some_and(|o| o.marker == marker);
                if !joins {
                    close(&mut open);
                }
                let Some((file, content)) = doc_line(text, range) else {
                    close(&mut open);
                    continue;
                };
                let run = open.get_or_insert_with(|| Open {
                    marker,
                    end: range.end_byte,
                    lines: Vec::new(),
                    markdown: String::new(),
                });
                run.lines.push(Line {
                    synthetic: run.markdown.len(),
                    file,
                    begins: text
                        .get(..start)
                        .and_then(|head| head.rfind('\n'))
                        .map_or(0, |at| at + 1),
                });
                run.markdown.push_str(content);
                run.end = range.end_byte;
            }
            Comment::Plain { start } if continues(start) => {
                if let Some(run) = open.as_mut() {
                    run.end = range.end_byte;
                }
            }
            Comment::Plain { .. } | Comment::Other => close(&mut open),
        }
    }
    close(&mut open);
    runs
}

/// Where the text of the doc line `range` begins in the file, after its marker and the one space
/// after it, and that text.
fn doc_line<'text>(text: &'text str, range: &tree_sitter::Range) -> Option<(usize, &'text str)> {
    let body = text.get(range.start_byte..range.end_byte)?;
    let space = usize::from(body.starts_with(' '));
    Some((range.start_byte + space, body.get(space..)?))
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
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");
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
