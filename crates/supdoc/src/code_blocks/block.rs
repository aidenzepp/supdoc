//! A code block found in a source file, and finding every block in a file by its language.

use std::ops::Range;

use crate::finding::Location;
use crate::read::cartridge::SourceFile;

use super::markdown;

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

/// One line of a block's body, split where its container's prefix ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BodyLine {
    /// The container's part of the line: a list item's indent or a quote's `> `. Empty at the top
    /// level.
    pub(crate) prefix: Range<usize>,
    /// The block's own text on the line, without its line ending.
    pub(crate) content: Range<usize>,
}

/// Every code block in `file`, in file order, located by `locate`, which turns a byte range of
/// the file into a [`Location`]. Only Markdown files hold blocks so far.
#[must_use]
pub(crate) fn blocks_in(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Vec<CodeBlock> {
    match file.language().id().as_str() {
        "markdown" => markdown::blocks(file, locate),
        _ => Vec::new(),
    }
}
