//! Rust doc comments: the `///` and `//!` containers.
//!
//! Each run of adjacent doc-comment lines is the lines without their marker and the one space
//! after it. `//`, `/* */` and `/** */` comments are not containers.

use std::ops::Range;

use crate::finding::Location;
use crate::read::cartridge::{MarkdownRegion, SourceFile};
use crate::read::error::ReadError;

use super::super::block::CodeBlock;
use super::{Line, Run, blocks_of_runs};

/// The markers of a doc comment.
const DOC_MARKERS: [&str; 2] = ["///", "//!"];

/// Every code block in the doc comments of `file`, a Rust file, in file order.
///
/// # Errors
///
/// The [`ReadError`] of the first run whose Markdown, or its NUL-replaced re-parse, can't be
/// parsed: the file's blocks are then unknown, not none.
pub(in super::super) fn blocks(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let regions = file.language().markdown_regions(file)?;
    blocks_of_runs(file, doc_runs(file.text(), &regions), locate)
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
