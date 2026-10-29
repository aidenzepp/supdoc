//! Finding links in a file's Markdown regions: [`links`] (03 R9, R14, Links from any source file).
//!
//! Link syntax is the same for every language, so this is a function, not a cartridge method.
//! Cartridges only decide *where* links can be ([`Cartridge::markdown_regions`]); this module
//! decides *how* they are written.
//!
//! Each region is parsed on its own by tree-sitter-md's inline grammar, restricted to the region
//! with `Parser::set_included_ranges`. The inline parser reads the original text in place, so
//! every node already carries the file's own offsets, and a link in a `.rs` comment reports its
//! real line with no offset arithmetic.
//!
//! What counts as a link:
//!
//! - **Inline links**, `[text](destination)`.
//! - **Reference links**, `[text][label]`, `[label][]` and `[label]`, when a definition
//!   `[label]: destination` exists for the label. Labels match case-insensitively with runs of
//!   whitespace collapsed, as the Markdown spec says. The first definition of a label wins.
//! - Never images, autolinks, or anything inside a code span or code fence: the grammar produces
//!   no link nodes there.
//!
//! Where a definition applies:
//!
//! - In a Markdown file, everywhere in the document. Definitions come from the block grammar's
//!   `link_reference_definition` nodes.
//! - In a code file, only inside the comment that holds it (*comments aren't one document*). A
//!   definition there is a `[label]: destination` that starts its line, after nothing but comment
//!   markers and whitespace (`/// [id]: x.rs`, ` * [id]: x.rs`, `# [id]: x.rs`).
//!
//! [`Cartridge::markdown_regions`]: crate::read::cartridge::Cartridge::markdown_regions

use std::collections::HashMap;

use tree_sitter::{Node, Parser, Tree};

use crate::link::RawLink;
use crate::read::cartridge::SourceFile;

/// Every link in `file`'s Markdown regions, in file order (03 R9, R14).
///
/// Destinations are returned as written, after reference resolution and without the `<…>` that
/// may wrap them. Reading them is `parse_destination`'s job.
#[must_use]
pub fn links(file: &SourceFile) -> Vec<RawLink> {
    let Some(mut parser) = inline_parser() else {
        // The inline grammar is compiled in; a test proves it loads, so this is unreachable.
        return Vec::new();
    };
    let text = file.text();
    let document = document_definitions(file);

    let mut links = Vec::new();
    for region in file.language().markdown_regions(file) {
        let Some(tree) = parse_region(&mut parser, text, region) else {
            continue;
        };
        let found = Found::collect(tree.root_node());
        if let Some(definitions) = &document {
            links.extend(found.resolve(definitions, text));
        } else {
            let local = comment_definitions(&found.references, text, region.start_byte);
            links.extend(found.resolve(&local, text));
        }
    }
    links.sort_by_key(|link| (link.range.start, link.range.end));
    links
}

/// A parser for tree-sitter-md's inline grammar, or `None` if the grammar can't be loaded.
fn inline_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .ok()?;
    Some(parser)
}

/// Parses only `region` of `text` with the inline grammar. The tree keeps `text`'s offsets.
///
/// `None` if tree-sitter rejects the range or returns no tree; supdoc sets no timeout or
/// cancellation, so neither happens for a region a cartridge produced from the file's own tree.
fn parse_region(parser: &mut Parser, text: &str, region: tree_sitter::Range) -> Option<Tree> {
    parser.set_included_ranges(&[region]).ok()?;
    parser.parse(text, None)
}

/// What one region's inline tree holds, before references are resolved.
#[derive(Debug, Default)]
struct Found<'tree> {
    /// `[text](destination)` nodes.
    inline: Vec<Node<'tree>>,
    /// `[text][label]`, `[label][]` and `[label]` nodes.
    references: Vec<Node<'tree>>,
}

impl<'tree> Found<'tree> {
    /// Collects the link nodes under `root`, without descending into links, images or code
    /// spans, whose contents are never links of their own.
    fn collect(root: Node<'tree>) -> Self {
        let mut found = Self::default();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            match node.kind() {
                "inline_link" => found.inline.push(node),
                "full_reference_link" | "collapsed_reference_link" | "shortcut_link" => {
                    found.references.push(node);
                }
                "image" | "code_span" => {}
                _ => {
                    let mut cursor = node.walk();
                    pending.extend(node.children(&mut cursor));
                }
            }
        }
        found
    }

    /// The region's links: every inline link, and every reference whose label `definitions`
    /// knows. A reference that is itself a definition (`[id]: x.rs` in a comment) is not a link.
    fn resolve(&self, definitions: &Definitions, text: &str) -> Vec<RawLink> {
        let inline = self.inline.iter().map(|&node| RawLink {
            text: child_text(node, "link_text", text).to_owned(),
            destination: unwrap_angle_brackets(child_text(node, "link_destination", text))
                .to_owned(),
            range: node.byte_range(),
        });
        let references = self.references.iter().filter_map(|&node| {
            if definitions.is_definition_site(node.start_byte()) {
                return None;
            }
            let link_text = child_text(node, "link_text", text);
            // `[text][label]` names its label; `[label][]` and `[label]` use their text.
            let label = child(node, "link_label").map_or(link_text, |label| node_text(label, text));
            let destination = definitions.get(label)?;
            Some(RawLink {
                text: link_text.to_owned(),
                destination: destination.to_owned(),
                range: node.byte_range(),
            })
        });
        inline.chain(references).collect()
    }
}

/// Reference definitions by normalized label.
#[derive(Debug, Default)]
struct Definitions {
    /// Normalized label → destination, without `<…>`. The first definition of a label wins.
    destinations: HashMap<String, String>,
    /// Where each definition's `[label]` starts, so the definition isn't read as a link itself.
    sites: Vec<usize>,
}

impl Definitions {
    /// Records `[label]: destination` written at byte `site`, unless the label is already defined.
    fn add(&mut self, label: &str, destination: &str, site: usize) {
        self.sites.push(site);
        let _first = self
            .destinations
            .entry(normalize_label(label))
            .or_insert_with(|| unwrap_angle_brackets(destination).to_owned());
    }

    /// The destination defined for `label`, matched the way the Markdown spec matches labels.
    fn get(&self, label: &str) -> Option<&str> {
        self.destinations
            .get(&normalize_label(label))
            .map(String::as_str)
    }

    /// Whether a definition's `[label]` starts at byte `start`.
    fn is_definition_site(&self, start: usize) -> bool {
        self.sites.contains(&start)
    }
}

/// The document-wide definitions of a Markdown file, or `None` for a file whose grammar has no
/// `link_reference_definition` nodes (every code file), where definitions are per comment.
fn document_definitions(file: &SourceFile) -> Option<Definitions> {
    let kind = file
        .language()
        .grammar()
        .id_for_node_kind("link_reference_definition", true);
    if kind == 0 {
        return None;
    }
    let text = file.text();
    let mut definitions = Definitions::default();
    let mut pending = vec![file.tree().root_node()];
    while let Some(node) = pending.pop() {
        if node.kind_id() == kind {
            let label = child(node, "link_label").map_or("", |label| node_text(label, text));
            let destination = child_text(node, "link_destination", text);
            definitions.add(label, destination, node.start_byte());
            continue;
        }
        let mut cursor = node.walk();
        // Reversed so definitions are met in file order and the first one wins.
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    Some(definitions)
}

/// The definitions written inside one comment region that starts at byte `region_start`.
///
/// The inline grammar reads `[id]: x.rs` as a shortcut link followed by text, so a definition is
/// a `[label]` shortcut that starts its line (after only comment markers and whitespace), is
/// followed directly by `:`, and then by a destination.
fn comment_definitions(references: &[Node<'_>], text: &str, region_start: usize) -> Definitions {
    let mut ordered: Vec<Node<'_>> = references
        .iter()
        .copied()
        .filter(|node| node.kind() == "shortcut_link")
        .collect();
    ordered.sort_by_key(Node::start_byte);

    let mut definitions = Definitions::default();
    for node in ordered {
        let start = node.start_byte();
        let line_start = text
            .get(..start)
            .and_then(|before| before.rfind('\n'))
            .map_or(0, |newline| newline + 1)
            .max(region_start);
        let starts_line = text
            .get(line_start..start)
            .is_some_and(|prefix| prefix.chars().all(is_comment_decoration));
        let destination = text
            .get(node.end_byte()..)
            .and_then(|after| after.strip_prefix(':'))
            .and_then(definition_destination);
        if let (true, Some(destination)) = (starts_line, destination) {
            definitions.add(child_text(node, "link_text", text), destination, start);
        }
    }
    definitions
}

/// Whether `c` can sit before a definition on its line in a comment: whitespace or a comment
/// marker such as `/`, `*`, `#` or `"`. Text and Markdown link syntax can't.
fn is_comment_decoration(c: char) -> bool {
    !(c.is_alphanumeric() || matches!(c, '[' | ']' | '(' | ')' | '`' | '<' | '>'))
}

/// The destination after a definition's `:`: `<…>` or the next run of non-whitespace.
fn definition_destination(after_colon: &str) -> Option<&str> {
    let rest = after_colon.trim_start();
    let end = if rest.starts_with('<') {
        rest.find('>')? + 1
    } else {
        rest.find(char::is_whitespace).unwrap_or(rest.len())
    };
    rest.get(..end)
        .filter(|destination| !destination.is_empty())
}

/// `destination` without the `<…>` the Markdown spec allows around it.
fn unwrap_angle_brackets(destination: &str) -> &str {
    destination
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .unwrap_or(destination)
}

/// A label as the Markdown spec compares it: brackets removed, case folded, whitespace runs collapsed.
fn normalize_label(label: &str) -> String {
    let inner = label
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(label);
    inner
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// `node`'s first child of `kind`.
fn child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// The text of `node`'s first child of `kind`, or `""` when it has none (`[](x.rs)` has no text).
fn child_text<'text>(node: Node<'_>, kind: &str, text: &'text str) -> &'text str {
    child(node, kind).map_or("", |child| node_text(child, text))
}

/// The text `node` covers.
fn node_text<'text>(node: Node<'_>, text: &'text str) -> &'text str {
    text.get(node.byte_range()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cartridges::markdown::regions::tests::markdown_file;
    use crate::path::AbsolutePath;

    fn rust_file(text: &str) -> SourceFile {
        let rust =
            crate::cartridges::rust::cartridge().expect("the built-in Rust queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/server.rs"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), rust).expect("tree-sitter should return a tree")
    }

    /// Each link as `(text, destination)`, in the order `links` returned them.
    fn pairs(file: &SourceFile) -> Vec<(String, String)> {
        links(file)
            .into_iter()
            .map(|link| (link.text, link.destination))
            .collect()
    }

    fn pair(text: &str, destination: &str) -> (String, String) {
        (text.to_owned(), destination.to_owned())
    }

    fn destinations(file: &SourceFile) -> Vec<String> {
        links(file)
            .into_iter()
            .map(|link| link.destination)
            .collect()
    }

    fn builtin_file(path: &str, text: &str) -> SourceFile {
        let path = AbsolutePath::new(Path::new(path)).expect("the fixture path should be absolute");
        let language = crate::read::cartridge::Languages::builtin()
            .for_path(path.as_path())
            .expect("the built-in registry should cover the fixture extension");
        SourceFile::parse(path, text.to_owned(), language)
            .expect("tree-sitter should return a tree")
    }

    /// Each link as `(text, destination, source slice)`, in the order `links` returned them.
    fn spans(file: &SourceFile) -> Vec<(String, String, String)> {
        links(file)
            .into_iter()
            .map(|link| {
                let slice = file.text().get(link.range.clone()).unwrap_or_default();
                (link.text, link.destination, slice.to_owned())
            })
            .collect()
    }

    fn span(text: &str, destination: &str, slice: &str) -> (String, String, String) {
        (text.to_owned(), destination.to_owned(), slice.to_owned())
    }

    #[test]
    fn inline_grammar_loads() {
        assert!(
            inline_parser().is_some(),
            "tree-sitter-md's inline grammar should load into this tree-sitter version",
        );
    }

    #[test]
    fn markdown_inline_link_is_extracted_with_text_destination_and_range() {
        let text = "# Guide\n\nThe [`add`](../src/lib.rs#add) function.\n";
        let file = markdown_file(text);

        let found = links(&file);

        assert_eq!(
            found,
            [RawLink {
                text: "`add`".to_owned(),
                destination: "../src/lib.rs#add".to_owned(),
                range: 13..39,
            }],
        );
        assert_eq!(
            text.get(13..39),
            Some("[`add`](../src/lib.rs#add)"),
            "the range covers the whole link",
        );
    }

    #[test]
    fn markdown_links_are_read_from_every_body_block_in_file_order() {
        let file = markdown_file(
            "# See [h](h.rs)\n\n- [l](l.rs)\n\n> [q](q.rs)\n\n| [t](t.rs) |\n|---|\n| [c](c.rs) |\n\n[p](p.rs) and [title](t.md \"Title\")\n",
        );

        assert_eq!(
            destinations(&file),
            ["h.rs", "l.rs", "q.rs", "t.rs", "c.rs", "p.rs", "t.md"],
        );
    }

    #[test]
    fn links_in_code_spans_and_fences_are_ignored() {
        let file = markdown_file(
            "Use `[y](c.rs#d)` to link.\n\n```md\n[f](f.rs)\n```\n\n    [i](i.rs)\n\n[real](r.rs)\n",
        );

        assert_eq!(destinations(&file), ["r.rs"]);
    }

    #[test]
    fn images_and_autolinks_are_not_links() {
        let file = markdown_file(
            "![logo](logo.rs) <https://example.com> <src/lib.rs> https://example.com [real](r.rs)\n",
        );

        assert_eq!(destinations(&file), ["r.rs"]);
    }

    #[test]
    fn reference_style_link_resolves_to_definition() {
        let file = markdown_file(
            "See [the add fn][add], [Config][], [Mode] and [unknown].\n\n\
             [ADD]: ../src/lib.rs#add\n\
             [config]: <../src/config.rs#Config>\n\
             [mode]: ../src/mode.rs#Mode \"Modes\"\n\
             [add]: ../src/other.rs#add\n",
        );

        assert_eq!(
            pairs(&file),
            [
                pair("the add fn", "../src/lib.rs#add"),
                pair("Config", "../src/config.rs#Config"),
                pair("Mode", "../src/mode.rs#Mode"),
            ],
            "labels match case-insensitively, the first definition wins, `<…>` is unwrapped, \
             and `[unknown]` has no definition so it is plain text",
        );
    }

    #[test]
    fn reference_labels_match_with_whitespace_collapsed() {
        let file = markdown_file("[the  Add\nfn][]\n\n[the add fn]: lib.rs#add\n");

        assert_eq!(destinations(&file), ["lib.rs#add"]);
    }

    #[test]
    fn markdown_definition_applies_across_the_whole_document() {
        let file = markdown_file(
            "[early]: lib.rs#a\n\n# One\n\n[x][early]\n\n# Two\n\n[y][late]\n\n[late]: lib.rs#b\n",
        );

        assert_eq!(destinations(&file), ["lib.rs#a", "lib.rs#b"]);
    }

    #[test]
    fn angle_bracket_destination_is_unwrapped() {
        let file = markdown_file("[spaced](<my file.rs#f>)\n");

        assert_eq!(destinations(&file), ["my file.rs#f"]);
    }

    #[test]
    fn rust_line_and_doc_comment_links_are_extracted() {
        let file = rust_file(
            "//! Crate [overview](../docs/overview.md).\n\
             /// Mirrors [the session rules](../docs/auth.md#sessions).\n\
             fn session() {\n\
                 // Keep in sync with [Config.port](../src/config.rs#Config.port)\n\
             }\n\
             /* Block [b](../docs/b.md) */\n",
        );

        assert_eq!(
            pairs(&file),
            [
                pair("overview", "../docs/overview.md"),
                pair("the session rules", "../docs/auth.md#sessions"),
                pair("Config.port", "../src/config.rs#Config.port"),
                pair("b", "../docs/b.md"),
            ],
        );
    }

    #[test]
    fn comment_link_range_points_at_its_line_in_the_code_file() {
        let text = "fn a() {}\n\nfn b() {\n    // see [b](../docs/b.md)\n}\n";
        let file = rust_file(text);

        let found = links(&file);
        let link = found.first().expect("the comment holds one link");

        assert_eq!(text.get(link.range.clone()), Some("[b](../docs/b.md)"));
        let row = text
            .get(..link.range.start)
            .map(|before| before.matches('\n').count());
        assert_eq!(row, Some(3), "the link sits on the fourth line (row 3)");
    }

    #[test]
    fn syntax_shown_in_a_comment_code_span_is_not_a_link() {
        let file = rust_file("/// Links look like `[text](path#symbol)`.\nfn scanner() {}\n");

        assert_eq!(links(&file), []);
    }

    #[test]
    fn string_literal_that_looks_like_a_link_is_not_a_link() {
        let file = rust_file("fn f() { let s = \"[a](b.rs#c)\"; }\n");

        assert_eq!(links(&file), []);
    }

    #[test]
    fn reference_definition_in_the_same_comment_applies() {
        let file = rust_file(
            "/* See [the rules][rules].\n * [rules]: ../docs/auth.md#rules\n */\nfn f() {}\n",
        );

        assert_eq!(pairs(&file), [pair("the rules", "../docs/auth.md#rules")]);
    }

    #[test]
    fn reference_definition_in_another_comment_does_not_apply() {
        let file = rust_file(
            "/// See [the rules][rules].\n/// [rules]: ../docs/auth.md#rules\nfn f() {}\n",
        );

        assert_eq!(
            links(&file),
            [],
            "each `///` line is its own comment, so the definition is in another region",
        );
    }

    #[test]
    fn comment_definition_is_not_itself_a_link() {
        let file = rust_file("/* [rules]: ../docs/auth.md [rules] */\nfn f() {}\n");

        assert_eq!(
            pairs(&file),
            [pair("rules", "../docs/auth.md")],
            "only the use after the definition is a link",
        );
    }

    #[test]
    fn bracket_mid_sentence_is_not_a_comment_definition() {
        let file = rust_file("/* Note [x]: y.rs is text. [x] */\nfn f() {}\n");

        assert_eq!(links(&file), []);
    }

    #[test]
    fn rustdoc_shortcut_without_definition_is_not_a_link() {
        let file = rust_file("/// Returns a [`Vec`] of [`Config`]s.\nfn f() {}\n");

        assert_eq!(links(&file), []);
    }

    #[test]
    fn ts_block_comment_link_is_extracted() {
        let file = builtin_file(
            "/repo/src/a.ts",
            "// [a](a.md)\n/** [b](b.md) */\n/* [c](c.md) */\nfunction f() {}\n",
        );

        assert_eq!(
            spans(&file),
            [
                span("a", "a.md", "[a](a.md)"),
                span("b", "b.md", "[b](b.md)"),
                span("c", "c.md", "[c](c.md)"),
            ],
        );
    }

    #[test]
    fn python_docstring_and_hash_comment_links_are_extracted() {
        let file = builtin_file(
            "/repo/src/a.py",
            "\"\"\"Module [guide](guide.md).\"\"\"\n# [host](host.md)\ndef f():\n    \"\"\"Adds [two](two.md).\"\"\"\n",
        );

        assert_eq!(
            spans(&file),
            [
                span("guide", "guide.md", "[guide](guide.md)"),
                span("host", "host.md", "[host](host.md)"),
                span("two", "two.md", "[two](two.md)"),
            ],
        );
    }
}
