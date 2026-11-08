//! Where links live in a Markdown file: its body.
//!
//! tree-sitter-md parses a document in two layers. The block grammar finds paragraphs, headings,
//! lists, quotes and fences; its own `injections.scm` then marks every `inline` node as
//! `markdown_inline`, the layer where links are written. [`markdown_regions`] returns those same
//! nodes, plus table cells, whose text the block grammar leaves unparsed. Fenced and indented
//! code, HTML blocks, front matter and link reference definitions are never regions.

use tree_sitter::Node;

use crate::read::cartridge::{MarkdownRegion, SourceFile};

/// Block-grammar node kinds whose text is inline Markdown.
///
/// `inline` is tree-sitter-md's own `markdown_inline` injection. `pipe_table_cell` has no `inline`
/// child, so it is listed too: a link in a table is part of the body like any other.
const INLINE_KINDS: [&str; 2] = ["inline", "pipe_table_cell"];

/// The regions of `file`'s Markdown body where links are read, in file order. Each is one node's
/// range.
///
/// Meant for a file parsed by tree-sitter-md's block grammar; any other tree has no such nodes and
/// yields no regions.
#[must_use]
pub(crate) fn markdown_regions(file: &SourceFile) -> Vec<MarkdownRegion> {
    let mut regions = Vec::new();
    let mut pending: Vec<Node<'_>> = vec![file.tree().root_node()];
    while let Some(node) = pending.pop() {
        if INLINE_KINDS.contains(&node.kind()) {
            regions.push(MarkdownRegion::from(node.range()));
            continue;
        }
        let mut cursor = node.walk();
        // Reversed so the stack pops children in file order.
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    regions
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cartridges::markdown::cartridge::cartridge;

    /// `text` parsed as `/repo/docs/guide.md`.
    fn markdown_file(text: &str) -> SourceFile {
        let path = crate::path::fixture("/repo/docs/guide.md");
        let markdown = cartridge();
        SourceFile::parse(path, text.to_owned(), markdown)
            .expect("tree-sitter should return a tree")
    }

    fn region_texts(file: &SourceFile) -> Vec<String> {
        markdown_regions(file)
            .iter()
            .map(|region| {
                region
                    .text_within(0..file.text().len(), file.text())
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn body_text_blocks_are_regions_in_file_order() {
        let file = markdown_file(
            "# Guide\n\nA paragraph.\n\n- an item\n\n> a quote\n\n| a | b |\n|---|---|\n| c | d |\n",
        );

        assert_eq!(
            region_texts(&file),
            [
                "Guide",
                "A paragraph.",
                "an item",
                "a quote",
                "a ",
                "b ",
                "c ",
                "d "
            ],
        );
    }

    #[test]
    fn code_html_and_reference_definitions_are_not_regions() {
        let file = markdown_file(
            "```rust\n[f](f.rs)\n```\n\n    [i](i.rs)\n\n<div>[h](h.rs)</div>\n\n[id]: d.rs\n\nText.\n",
        );

        assert_eq!(region_texts(&file), ["Text."]);
    }

    #[test]
    fn region_ranges_carry_the_file_offsets() {
        let file = markdown_file("# Title\n\nSecond paragraph.\n");

        let regions = markdown_regions(&file);
        let second = regions
            .get(1)
            .and_then(|region| region.ranges().first())
            .expect("the heading and the paragraph are two regions");

        assert_eq!(second.start_byte, 9);
        assert_eq!(second.start_point, tree_sitter::Point { row: 2, column: 0 });
    }
}
