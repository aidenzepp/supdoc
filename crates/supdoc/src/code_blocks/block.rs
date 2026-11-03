//! A code block found in a source file, and finding every block in a file by its language.

use std::ops::Range;

use crate::finding::Location;
use crate::read::cartridge::SourceFile;
use crate::read::error::ReadError;

use super::info::InfoString;
use super::{doc_comments, markdown};

/// One fenced code block, located in its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeBlock {
    /// From the opening fence through the closing one, or to the end of the block's container
    /// when the fence is never closed.
    pub(crate) fence: Location,
    /// The text after the opening fence, if there is any.
    pub(crate) info: Option<Info>,
    /// The lines between the fences, in order.
    pub(crate) body: Vec<BodyLine>,
}

/// A block's info string as written, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Info {
    /// The info string, as written.
    pub(crate) text: String,
    /// Where the info string is.
    pub(crate) location: Location,
}

impl Info {
    /// The info string split into language and attributes, read as rustdoc reads it when the
    /// block is in a Rust file's doc comments.
    #[must_use]
    pub(crate) fn parsed(&self) -> InfoString<'_> {
        InfoString::parse_in(&self.text, self.in_rust_doc())
    }

    /// Whether the info string is in a Rust file's doc comments.
    #[must_use]
    pub(crate) fn in_rust_doc(&self) -> bool {
        is_rust_file(self.location.file.as_str())
    }
}

/// One line of a block's body, split where its container's prefix ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BodyLine {
    /// The container's part of the line: a list item's indent or a quote's `> `. Empty at the top
    /// level.
    pub(crate) prefix: Range<usize>,
    /// The block's own text on the line, without its line ending.
    pub(crate) content: Range<usize>,
}

impl CodeBlock {
    /// The language of a block with no info string: `rust` in a Rust file's doc comments, where
    /// rustdoc reads an untagged fence as Rust, and none in Markdown.
    #[must_use]
    pub(crate) fn default_language(&self) -> Option<&'static str> {
        is_rust_file(self.fence.file.as_str()).then_some("rust")
    }
}

/// Whether `file` is a Rust source file, by its extension.
fn is_rust_file(file: &str) -> bool {
    std::path::Path::new(file)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
}

/// Every code block in `file`, in file order, located by `locate`, which turns a byte range of
/// the file into a [`Location`]. Markdown files hold blocks, and so do the `///` and `//!` doc
/// comments of Rust files.
#[cfg(test)]
#[must_use]
pub(crate) fn blocks_in(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Vec<CodeBlock> {
    try_blocks_in(file, locate).unwrap_or_default()
}

/// Like [`blocks_in`], but an error when `file`'s blocks can't be read reliably (a Markdown file
/// holding a NUL whose parse with each NUL replaced failed), so a caller can report or refuse
/// rather than treat the file as holding no blocks.
///
/// # Errors
///
/// The [`ReadError`] of the failed parse.
pub(crate) fn try_blocks_in(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    match file.language().id().as_str() {
        "markdown" => markdown::try_blocks(file, locate),
        "rust" => doc_comments::blocks(file, locate),
        _ => Ok(Vec::new()),
    }
}

/// The edit that makes `block`'s body read `body`: the byte range of `text`, the file the block
/// was found in, and its replacement. Each new line is re-prefixed with the container prefix the
/// body's first prefixed line has (a list item's indent or a quote's `> `), trimmed on a blank
/// line, and the body's line ending (LF or CRLF) is kept. `body` holds no final line ending.
///
/// `None` when the block's ranges don't lie inside `text`, or its body is empty and its fence is
/// never closed.
#[must_use]
pub(crate) fn replacement_for(
    block: &CodeBlock,
    text: &str,
    body: &str,
) -> Option<(Range<usize>, String)> {
    let Some((first, last)) = block.body.first().zip(block.body.last()) else {
        return empty_body_replacement(block, text, body);
    };
    let prefix = block
        .body
        .iter()
        .map(|line| text.get(line.prefix.clone()))
        .find(|prefix| prefix.is_none_or(|prefix| !prefix.is_empty() && !is_bare_marker(prefix)))
        .or_else(|| {
            block
                .body
                .iter()
                .map(|line| text.get(line.prefix.clone()))
                .next()
        })
        .flatten()
        .unwrap_or_default();
    let crlf = text.get(first.content.clone())?.ends_with('\r');
    let mut replacement = prefixed(body, prefix, if crlf { "\r\n" } else { "\n" });
    if text.get(last.content.clone())?.ends_with('\r') {
        replacement.push('\r');
    }
    Some((first.prefix.start..last.content.end, replacement))
}

/// The insertion that fills `block`'s empty body with `body`, just after its opening fence's
/// line, prefixed like the closing fence's line.
fn empty_body_replacement(
    block: &CodeBlock,
    text: &str,
    body: &str,
) -> Option<(Range<usize>, String)> {
    let fence = text.get(block.fence.range.clone())?;
    let start = block.fence.range.start + fence.find('\n')? + 1;
    let closing = text.get(start..block.fence.range.end)?;
    let indent = closing.len() - closing.trim_start_matches([' ', '\t']).len();
    let marker = closing
        .get(indent..)
        .and_then(|rest| {
            ["///", "//!"]
                .into_iter()
                .find(|marker| rest.starts_with(marker))
        })
        .map_or(0, str::len);
    let rest = closing.get(indent + marker..)?;
    let prefix_len = closing.len() - rest.trim_start_matches([' ', '\t', '>']).len();
    let prefix = closing.get(..prefix_len)?;
    let mut replacement = prefixed(body, prefix, "\n");
    replacement.push('\n');
    Some((start..start, replacement))
}

/// Whether `prefix` is a doc-comment marker with no space after it, as on a blank doc line.
fn is_bare_marker(prefix: &str) -> bool {
    prefix.ends_with("///") || prefix.ends_with("//!")
}

/// `body`'s lines, each led by `prefix` (trimmed on a blank line), joined by `newline`. A line's
/// own CR is dropped when `newline` brings one.
fn prefixed(body: &str, prefix: &str, newline: &str) -> String {
    body.split('\n')
        .map(|line| match newline {
            "\r\n" => line.strip_suffix('\r').unwrap_or(line),
            _ => line,
        })
        .map(|line| {
            if line.is_empty() {
                prefix.trim_end().to_owned()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join(newline)
}
