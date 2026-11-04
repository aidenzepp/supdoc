//! Fenced code blocks in a Markdown file, read from tree-sitter-md's block tree.
//!
//! Backtick and tilde fences are both `fenced_code_block` nodes. Indented code is a different node
//! and never a block. Inside a list item or a quote, the grammar marks each line's container prefix
//! as a `block_continuation` node, which is where a body line's own text starts.

use std::collections::BTreeMap;
use std::ops::Range;

use tree_sitter::{Node, Tree};

use crate::finding::Location;
use crate::read::cartridge::SourceFile;
use crate::read::error::ReadError;

use super::block::{BodyLine, CodeBlock, Info};

/// Every fenced code block in `file`, a Markdown file, in file order.
///
/// A Markdown renderer reads a NUL as an ordinary character (U+FFFD), but tree-sitter-md doesn't,
/// so a NUL could hide a fence line from supdoc that a renderer sees. A file holding one is parsed again
/// with each NUL as `?`, which keeps every offset, and its blocks are read only from that tree.
///
/// # Errors
///
/// The [`ReadError`] of the parse with each NUL replaced when it fails: the file's own tree can't
/// see past a NUL, so it is never read instead.
pub(super) fn try_blocks(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let text = file.text();
    if !text.contains('\0') {
        return Ok(blocks_of_tree(text, file.tree(), &locate));
    }
    let cleaned = SourceFile::from_text(
        file.path().clone(),
        text.replace('\0', "?"),
        file.language(),
        file.deadline(),
    );
    blocks_from_parse(text, cleaned, &locate)
}

/// The step a timeout of the parse with each NUL replaced names, so its `read-timeout` says which
/// parse ran out of `parse-timeout`.
const REPARSE: &str = "the re-parse after NUL replacement";

/// The blocks of `text` read from `cleaned`, the parse of `text` with each NUL replaced.
///
/// # Errors
///
/// The [`ReadError`] `cleaned` failed with; a timeout names [`REPARSE`] as its step.
fn blocks_from_parse(
    text: &str,
    cleaned: Result<SourceFile, ReadError>,
    locate: &impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let cleaned = cleaned.map_err(|error| match error {
        ReadError::Timeout {
            path,
            limit,
            offset,
            size,
            ..
        } => ReadError::Timeout {
            path,
            limit,
            step: REPARSE,
            offset,
            size,
        },
        other => other,
    })?;
    Ok(blocks_of_tree(text, cleaned.tree(), locate))
}

/// Every fenced code block in `tree`, parsed from `text` or a copy with the same offsets, in file
/// order.
fn blocks_of_tree(
    text: &str,
    tree: &Tree,
    locate: &impl Fn(Range<usize>) -> Option<Location>,
) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let mut pending: Vec<Node<'_>> = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if node.kind() == "fenced_code_block" {
            blocks.extend(block(text, node, locate));
            continue;
        }
        let mut cursor = node.walk();
        // Reversed so the stack pops children in file order.
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    blocks
}

/// The block `node`, a `fenced_code_block`, or `None` if its ranges can't be located in `text`,
/// which a tree parsed from `text` never gives.
fn block(
    text: &str,
    node: Node<'_>,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Option<CodeBlock> {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
    let mut delimiters = children
        .iter()
        .filter(|child| child.kind() == "fenced_code_block_delimiter");
    let opening = delimiters.next()?;
    let closing = delimiters.next();
    let info = match children.iter().find(|child| child.kind() == "info_string") {
        Some(child) => {
            let range = child.byte_range();
            Some(Info {
                text: text.get(range.clone())?.to_owned(),
                location: locate(range)?,
            })
        }
        None => None,
    };

    let body_start = line_end(text, opening.end_byte()).map_or(text.len(), |end| end + 1);
    let body_end = match closing {
        Some(closing) => line_start(text, closing.start_byte()),
        None => node.end_byte(),
    }
    .max(body_start);
    let fence_end = closing.map_or(body_end, Node::end_byte);
    let prefixes = continuations(node);
    let body = body_lines(text, body_start..body_end, &prefixes);
    Some(CodeBlock {
        fence: locate(opening.start_byte()..fence_end)?,
        info,
        body,
    })
}

/// Where each `block_continuation` under `node` starts, mapped to where it ends.
fn continuations(node: Node<'_>) -> BTreeMap<usize, usize> {
    let mut found = BTreeMap::new();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if node.kind() == "block_continuation" {
            let _previous = found.insert(node.start_byte(), node.end_byte());
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    found
}

/// The lines of `text[body]`, each split after the container prefix that starts it, if any.
fn body_lines(text: &str, body: Range<usize>, prefixes: &BTreeMap<usize, usize>) -> Vec<BodyLine> {
    let mut lines = Vec::new();
    let mut start = body.start;
    while start < body.end {
        let end = line_end(text, start).unwrap_or(text.len()).min(body.end);
        let prefix_end = prefixes
            .get(&start)
            .map_or(start, |&prefix_end| prefix_end.min(end));
        lines.push(BodyLine {
            prefix: start..prefix_end,
            content: prefix_end..end,
        });
        start = end + 1;
    }
    lines
}

/// The offset of the line ending at or after `offset`, if the line has one.
fn line_end(text: &str, offset: usize) -> Option<usize> {
    let rest = text.get(offset..)?;
    rest.find('\n').map(|found| offset + found)
}

/// The offset where the line holding `offset` starts.
fn line_start(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |found| found + 1)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::code_blocks::block::blocks_in;
    use crate::path::AbsolutePath;
    use crate::read::error::ReadRole;

    /// The blocks of `text` parsed as `/repo/docs/guide.md`.
    fn blocks_of(text: &str) -> Vec<CodeBlock> {
        let path = AbsolutePath::new(Path::new("/repo/docs/guide.md"))
            .expect("the fixture path should be absolute");
        let file = SourceFile::parse(path, text.to_owned(), cartridge())
            .expect("tree-sitter should return a tree");
        blocks_in(&file, |range| {
            let source = "docs/guide.md"
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        })
    }

    /// The text of `range` in `text`.
    fn slice(text: &str, range: &Range<usize>) -> String {
        text.get(range.clone())
            .expect("a block range should lie inside the text")
            .to_owned()
    }

    #[test]
    fn a_backtick_fence_has_its_info_string_and_body_lines() {
        let text = "# Guide\n\n```rust no_run\nfn main() {}\nlet x = 1;\n```\n";
        let blocks = blocks_of(text);

        let [block] = blocks.as_slice() else {
            panic!("expected one block, got {blocks:?}");
        };
        let info = block
            .info
            .as_ref()
            .expect("the block should have an info string");
        assert_eq!(info.text, "rust no_run");
        assert_eq!(info.location.line, 3);
        assert_eq!(block.fence.line, 3);
        let contents: Vec<String> = block
            .body
            .iter()
            .map(|line| slice(text, &line.content))
            .collect();
        assert_eq!(contents, ["fn main() {}", "let x = 1;"]);
        assert!(block.body.iter().all(|line| line.prefix.is_empty()));
        assert_eq!(
            slice(text, &block.fence.range),
            "```rust no_run\nfn main() {}\nlet x = 1;\n```"
        );
    }

    #[test]
    fn a_tilde_fence_is_a_block() {
        let text = "~~~sh\necho hi\n~~~\n";
        let blocks = blocks_of(text);

        let [block] = blocks.as_slice() else {
            panic!("expected one block, got {blocks:?}");
        };
        assert_eq!(
            block.info.as_ref().map(|info| info.text.as_str()),
            Some("sh")
        );
        assert_eq!(block.body.len(), 1);
    }

    #[test]
    fn indented_code_is_not_a_block() {
        let text = "A paragraph.\n\n    let x = 1;\n    let y = 2;\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn a_fence_in_a_list_item_keeps_the_item_indent_as_its_prefix() {
        let text = "- Install:\n\n  ```sh\n  cargo install supdoc\n  ```\n";
        let blocks = blocks_of(text);

        let [block] = blocks.as_slice() else {
            panic!("expected one block, got {blocks:?}");
        };
        let [line] = block.body.as_slice() else {
            panic!("expected one body line, got {:?}", block.body);
        };
        assert_eq!(slice(text, &line.prefix), "  ");
        assert_eq!(slice(text, &line.content), "cargo install supdoc");
    }

    #[test]
    fn a_fence_in_a_quote_keeps_the_quote_marker_as_its_prefix() {
        let text = "> ```sh\n> echo hi\n> ```\n";
        let blocks = blocks_of(text);

        let [block] = blocks.as_slice() else {
            panic!("expected one block, got {blocks:?}");
        };
        let [line] = block.body.as_slice() else {
            panic!("expected one body line, got {:?}", block.body);
        };
        assert_eq!(slice(text, &line.prefix), "> ");
        assert_eq!(slice(text, &line.content), "echo hi");
    }

    #[test]
    fn a_fence_without_an_info_string_has_none() {
        let blocks = blocks_of("```\nplain\n```\n");

        let [block] = blocks.as_slice() else {
            panic!("expected one block, got {blocks:?}");
        };
        assert_eq!(block.info, None);
    }

    #[test]
    fn an_unclosed_fence_runs_to_the_end() {
        let text = "```sh\necho one\necho two\n";
        let blocks = blocks_of(text);

        let [block] = blocks.as_slice() else {
            panic!("expected one block, got {blocks:?}");
        };
        let contents: Vec<String> = block
            .body
            .iter()
            .map(|line| slice(text, &line.content))
            .collect();
        assert_eq!(contents, ["echo one", "echo two"]);
    }

    #[test]
    fn a_nul_inside_an_earlier_block_hides_none_of_the_later_blocks() {
        let text = "```sh\necho \0\n```\n\n```sh\necho two\n```\n\n- item\n\n  ```py\n  x\n  ```\n";
        let blocks = blocks_of(text);

        let infos: Vec<Option<&str>> = blocks
            .iter()
            .map(|block| block.info.as_ref().map(|info| info.text.as_str()))
            .collect();
        assert_eq!(infos, [Some("sh"), Some("sh"), Some("py")], "{blocks:?}");
        let first = blocks
            .first()
            .and_then(|block| block.body.first())
            .expect("the first block should have a body line");
        assert_eq!(slice(text, &first.content), "echo \0");
    }

    #[test]
    fn a_failed_replaced_parse_is_an_error_never_zero_blocks() {
        let text = "```sh\necho \0\n```\n";
        let path = AbsolutePath::new(Path::new("/repo/docs/guide.md"))
            .expect("the fixture path should be absolute");
        let failed = Err(ReadError::Timeout {
            path,
            limit: "10s".parse().expect("10s is a valid limit"),
            step: "the parse",
            offset: 0,
            size: text.len(),
        });

        let result = blocks_from_parse(text, failed, &|_| None);

        assert!(
            matches!(result, Err(ReadError::Timeout { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_timed_out_replaced_parse_names_the_re_parse_and_parse_timeout() {
        let text = "```sh\necho \0\n```\n";
        let path = AbsolutePath::new(Path::new("/repo/docs/guide.md"))
            .expect("the fixture path should be absolute");
        let failed = Err(ReadError::Timeout {
            path,
            limit: "10s".parse().expect("10s is a valid limit"),
            step: "the parse",
            offset: 3,
            size: text.len(),
        });

        let error = blocks_from_parse(text, failed, &|_| None)
            .expect_err("a timed-out re-parse should be an error");
        let help = error.help(ReadRole::ScannedSource, "docs/guide.md");

        assert!(
            help.starts_with("The re-parse after NUL replacement stopped at byte 3 of"),
            "the help should name the re-parse: {help}"
        );
        assert!(
            help.contains("`parse-timeout`"),
            "the help should suggest raising `parse-timeout`: {help}"
        );
    }
}
