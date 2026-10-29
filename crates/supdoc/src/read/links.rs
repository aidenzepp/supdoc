//! Finding links in a file's Markdown regions: [`links`] (03 R9, R14, Links from any source file).
//!
//! Link syntax is the same for every language, so this is a function, not a cartridge method.
//! Cartridges only decide *where* links can be ([`Cartridge::markdown_regions`]); this module
//! decides *how* they are written.
//!
//! Each region is parsed on its own by tree-sitter-md's inline grammar, restricted to the region's
//! ranges with `Parser::set_included_ranges`. The inline parser reads the original text in place,
//! so every node already carries the file's own offsets, and a link in a `.rs` comment reports its
//! real line with no offset arithmetic. A run of line comments is one region of several ranges, its
//! markers left out, so a link may wrap from one comment line to the next. Its text is read
//! through the region, without the markers between its lines.
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
//! - In a code file, only inside the region that holds it (*comments aren't one document*): one
//!   block comment or docstring, or one run of adjacent line comments of the same kind. A
//!   definition there is a `[label]: destination` that starts its line, after nothing but comment
//!   markers and whitespace (`/// [id]: x.rs`, ` * [id]: x.rs`, `# [id]: x.rs`), with its
//!   destination on the same line.
//!
//! [`Cartridge::markdown_regions`]: crate::read::cartridge::Cartridge::markdown_regions

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ops::Range;

use tree_sitter::{Node, Parser, Tree};

use crate::link::RawLink;
use crate::read::cartridge::{MarkdownRegion, SourceFile};

/// Every link in `file`'s Markdown regions, in file order (03 R9, R14).
///
/// Destinations are returned as written, after reference resolution and without the `<…>` that
/// may wrap them, with the file bytes they are written at. Reading them is `parse_destination`'s
/// job.
#[must_use]
pub fn links(file: &SourceFile) -> Vec<RawLink> {
    let Some(mut parser) = inline_parser() else {
        // The inline grammar is compiled in; a test proves it loads, so this is unreachable.
        return Vec::new();
    };
    let text = file.text();
    let mut document = document_definitions(file);

    let mut parsed = Vec::new();
    for region in file.language().markdown_regions(file) {
        if let Some(tree) = parse_region(&mut parser, text, &region) {
            parsed.push((tree, region));
        }
    }

    // The block grammar reads a one-character definition, `[x]: a.md`, as a task-list marker and
    // drops it, so a Markdown file's definitions are also read from its regions, as comments'
    // are, before any link resolves.
    let found: Vec<Found<'_>> = parsed
        .iter()
        .map(|(tree, _)| Found::collect(tree.root_node()))
        .collect();
    let local: Vec<Definitions> = found
        .iter()
        .zip(&parsed)
        .map(|(found, (_, region))| {
            comment_definitions(
                &found.references,
                RegionText { region, text },
                document.is_some(),
            )
        })
        .collect();
    if let Some(document) = &mut document {
        for definitions in &local {
            document.absorb(definitions);
        }
    }

    let mut links = Vec::new();
    for ((found, (_, region)), local) in found.iter().zip(&parsed).zip(&local) {
        let reader = RegionText { region, text };
        links.extend(found.resolve(document.as_ref().unwrap_or(local), reader));
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

/// Parses only `region`'s ranges of `text`, together, with the inline grammar. The tree keeps
/// `text`'s offsets.
///
/// `None` if tree-sitter rejects the ranges or returns no tree. [`MarkdownRegion`] holds only
/// ranges tree-sitter accepts, and supdoc sets no timeout or cancellation, so neither happens.
fn parse_region(parser: &mut Parser, text: &str, region: &MarkdownRegion) -> Option<Tree> {
    parser.set_included_ranges(region.ranges()).ok()?;
    parser.parse(text, None)
}

/// A file's text read through one region, so a node that spans a run of line comments reads
/// without the markers between its lines.
#[derive(Debug, Clone, Copy)]
struct RegionText<'a> {
    /// The region the inline tree was parsed from.
    region: &'a MarkdownRegion,
    /// The whole file.
    text: &'a str,
}

impl<'a> RegionText<'a> {
    /// The text `node` covers inside the region.
    fn node(self, node: Node<'_>) -> Cow<'a, str> {
        self.region.text_within(node.byte_range(), self.text)
    }

    /// The text of `node`'s first child of `kind`, or `""` when it has none (`[](x.rs)` has no
    /// text).
    fn child(self, node: Node<'_>, kind: &str) -> Cow<'a, str> {
        child(node, kind).map_or(Cow::Borrowed(""), |child| self.node(child))
    }

    /// The region's range that holds byte `at`.
    fn range_at(self, at: usize) -> Option<&'a tree_sitter::Range> {
        self.region
            .ranges()
            .iter()
            .find(|range| range.start_byte <= at && at < range.end_byte)
    }
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
    fn resolve(&self, definitions: &Definitions, reader: RegionText<'_>) -> Vec<RawLink> {
        let inline = self.inline.iter().map(|&node| RawLink {
            text: reader.child(node, "link_text").into_owned(),
            destination: unwrap_angle_brackets(&reader.child(node, "link_destination")).to_owned(),
            range: node.byte_range(),
            destination_range: inline_destination_range(node, reader.text),
        });
        let references = self.references.iter().filter_map(|&node| {
            if definitions.is_definition_site(node.start_byte()) {
                return None;
            }
            let link_text = reader.child(node, "link_text");
            // `[text][label]` names its label; `[label][]` and `[label]` use their text.
            let label = child(node, "link_label").map(|label| reader.node(label));
            let definition = definitions.get(label.as_deref().unwrap_or(&link_text))?;
            Some(RawLink {
                text: link_text.into_owned(),
                destination: definition.destination.clone(),
                range: node.byte_range(),
                destination_range: definition.range.clone(),
            })
        });
        inline.chain(references).collect()
    }
}

/// The file bytes of an inline link's destination, without `<…>`. A link written with no
/// destination, `[text]()`, gets the empty range just inside its `(`.
fn inline_destination_range(node: Node<'_>, text: &str) -> Range<usize> {
    if let Some(destination) = child(node, "link_destination") {
        return unwrap_angle_range(destination.byte_range(), text);
    }
    let open = child(node, "(").map_or(node.end_byte(), |open| open.end_byte());
    open..open
}

/// A reference definition's destination.
#[derive(Debug, Clone)]
struct Definition {
    /// The destination as written, without `<…>`.
    destination: String,
    /// Where `destination` is written in the file, in bytes.
    range: Range<usize>,
}

/// Reference definitions by normalized label.
#[derive(Debug, Default)]
struct Definitions {
    /// Normalized label → destination. The first definition of a label wins.
    destinations: HashMap<String, Definition>,
    /// Where each definition's `[label]` starts, so the definition isn't read as a link itself.
    sites: Vec<usize>,
}

impl Definitions {
    /// Records `[label]: destination` written at byte `site`, its destination at the file bytes
    /// `written` of `text`, unless the label is already defined.
    fn add(&mut self, label: &str, written: Range<usize>, text: &str, site: usize) {
        self.sites.push(site);
        let _first = self
            .destinations
            .entry(normalize_label(label))
            .or_insert_with(|| {
                let range = unwrap_angle_range(written, text);
                let destination = text.get(range.clone()).unwrap_or_default().to_owned();
                Definition { destination, range }
            });
    }

    /// Takes `other`'s definitions, keeping the earlier one in the file when a label is in both.
    fn absorb(&mut self, other: &Self) {
        self.sites.extend(&other.sites);
        for (label, definition) in &other.destinations {
            match self.destinations.entry(label.clone()) {
                Entry::Occupied(mut held) => {
                    if definition.range.start < held.get().range.start {
                        let _later = held.insert(definition.clone());
                    }
                }
                Entry::Vacant(free) => {
                    let _first = free.insert(definition.clone());
                }
            }
        }
    }

    /// The definition for `label`, matched the way the Markdown spec matches labels.
    fn get(&self, label: &str) -> Option<&Definition> {
        self.destinations.get(&normalize_label(label))
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
            let destination = child(node, "link_destination")
                .map_or(node.end_byte()..node.end_byte(), |destination| {
                    destination.byte_range()
                });
            definitions.add(label, destination, text, node.start_byte());
            continue;
        }
        let mut cursor = node.walk();
        // Reversed so definitions are met in file order and the first one wins.
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        pending.extend(children.into_iter().rev());
    }
    Some(definitions)
}

/// The definitions written inside one comment region.
///
/// The inline grammar reads `[id]: x.rs` as a shortcut link followed by text, so a definition is
/// a `[label]` shortcut that starts its line (after only comment markers and whitespace), is
/// followed directly by `:`, and then by a destination on the same line. As in `CommonMark`, it
/// also starts a block: it can't continue a paragraph, so the line before it is blank, another
/// definition, a heading (`markdown`, where `#` is syntax) or the start of the region, and it
/// can't sit inside a heading itself.
fn comment_definitions(
    references: &[Node<'_>],
    reader: RegionText<'_>,
    markdown: bool,
) -> Definitions {
    let text = reader.text;
    let mut ordered: Vec<Node<'_>> = references
        .iter()
        .copied()
        .filter(|node| node.kind() == "shortcut_link")
        .collect();
    ordered.sort_by_key(Node::start_byte);

    let mut definitions = Definitions::default();
    let mut last_definition_line = None;
    for node in ordered {
        let start = node.start_byte();
        let Some(range) = reader.range_at(start) else {
            continue;
        };
        let physical_line_start = text
            .get(..start)
            .and_then(|before| before.rfind('\n'))
            .map_or(0, |newline| newline + 1);
        let line_start = physical_line_start.max(range.start_byte);
        let starts_line = text
            .get(line_start..start)
            .is_some_and(|prefix| prefix.chars().all(is_comment_decoration));
        // The destination's offsets are counted from just after the `:`.
        let colon_end = node.end_byte() + 1;
        let destination = text
            .get(node.end_byte()..range.end_byte)
            .and_then(|after| after.strip_prefix(':'))
            .and_then(definition_destination)
            .map(|within| colon_end + within.start..colon_end + within.end);
        let starts_block = starts_line
            && (!markdown
                || (starts_block(text, line_start, range.start_byte, last_definition_line)
                    && !in_heading(text, physical_line_start)));
        if let (true, Some(destination)) = (starts_block, destination) {
            last_definition_line = Some(line_start);
            definitions.add(&reader.child(node, "link_text"), destination, text, start);
        }
    }
    definitions
}

/// Whether the line at `line_start` can begin a block: it opens its region (`region_start`),
/// or the line before it is blank or is the definition at `last_definition_line`.
fn starts_block(
    text: &str,
    line_start: usize,
    region_start: usize,
    last_definition_line: Option<usize>,
) -> bool {
    if line_start <= region_start {
        return true;
    }
    let before = &text[region_start..line_start - 1];
    let previous_start = before
        .rfind('\n')
        .map_or(region_start, |at| region_start + at + 1);
    text[previous_start..line_start - 1]
        .chars()
        .all(char::is_whitespace)
        || last_definition_line == Some(previous_start)
}

/// Whether the line at `line_start`, which a region may begin partway along, is an ATX heading:
/// 1-6 `#`s then a space before its text, so a `[label]: dest` in it is a link, not a definition.
fn in_heading(text: &str, line_start: usize) -> bool {
    let line = text.get(line_start..).unwrap_or_default();
    let hashes = line.trim_start().trim_start_matches('#');
    let count = line.trim_start().len() - hashes.len();
    (1..=6).contains(&count) && hashes.starts_with(char::is_whitespace)
}

/// Whether `c` can sit before a definition on its line in a comment: whitespace or a comment
/// marker such as `/`, `*`, `#` or `"`. Text and Markdown link syntax can't.
fn is_comment_decoration(c: char) -> bool {
    !(c.is_alphanumeric() || matches!(c, '[' | ']' | '(' | ')' | '`' | '<' | '>'))
}

/// Where the destination after a definition's `:` is, in bytes of `after_colon`: `<…>` or the
/// next run of non-whitespace.
fn definition_destination(after_colon: &str) -> Option<Range<usize>> {
    let rest = after_colon.trim_start();
    let start = after_colon.len() - rest.len();
    let len = if rest.starts_with('<') {
        rest.find('>')? + 1
    } else {
        rest.find(char::is_whitespace).unwrap_or(rest.len())
    };
    (len > 0).then_some(start..start + len)
}

/// `range` of `text` without the `<…>` the Markdown spec allows around a destination, matching
/// [`unwrap_angle_brackets`].
fn unwrap_angle_range(range: Range<usize>, text: &str) -> Range<usize> {
    let written = text.get(range.clone()).unwrap_or_default();
    if written.len() >= 2 && written.starts_with('<') && written.ends_with('>') {
        range.start + 1..range.end - 1
    } else {
        range
    }
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
                destination_range: 21..38,
            }],
        );
        assert_eq!(
            text.get(13..39),
            Some("[`add`](../src/lib.rs#add)"),
            "the range covers the whole link",
        );
    }

    /// Each link's destination as written in the file, read back through `destination_range`.
    fn written_destinations(file: &SourceFile) -> Vec<String> {
        links(file)
            .into_iter()
            .map(|link| {
                let written = file.text().get(link.destination_range.clone());
                written.unwrap_or_default().to_owned()
            })
            .collect()
    }

    /// The zero-based row of byte `at` in `text`.
    fn row(text: &str, at: usize) -> Option<usize> {
        text.get(..at).map(|before| before.matches('\n').count())
    }

    #[test]
    fn full_reference_link_destination_range_is_in_its_definition_line() {
        let text = "See [the x fn][xfn].\n\n[xfn]: auth:src/x.rs\n";
        let file = markdown_file(text);

        let found = links(&file);
        let link = found
            .first()
            .expect("`[the x fn][xfn]` is defined, so it is a link");

        assert_eq!(
            text.get(link.destination_range.clone()),
            Some("auth:src/x.rs")
        );
        assert_eq!(
            row(text, link.destination_range.start),
            Some(2),
            "the range is in the definition on row 2, not in the link on row 0",
        );
    }

    #[test]
    fn comment_definition_destination_range_leaves_out_angle_brackets() {
        let text =
            "fn a() {}\n/// See [the x fn][xfn].\n///\n/// [xfn]: <auth:src/x.rs>\nfn f() {}\n";
        let file = rust_file(text);

        let found = links(&file);
        let link = found
            .first()
            .expect("`[the x fn][xfn]` is defined, so it is a link");

        assert_eq!(
            text.get(link.destination_range.clone()),
            Some("auth:src/x.rs")
        );
        assert_eq!(row(text, link.destination_range.start), Some(3));
    }

    #[test]
    fn inline_link_in_a_rust_doc_comment_has_its_destination_file_bytes() {
        let text =
            "fn a() {}\n/// See [the session\n/// rules](<../docs/auth.md#sessions>).\nfn f() {}\n";
        let file = rust_file(text);

        let found = links(&file);
        let link = found.first().expect("the two `///` lines hold one link");

        assert_eq!(
            text.get(link.destination_range.clone()),
            Some("../docs/auth.md#sessions"),
            "file coordinates, without the `<…>`",
        );
        assert_eq!(row(text, link.destination_range.start), Some(2));
    }

    #[test]
    fn destination_range_reads_back_as_the_destination() {
        let file = markdown_file(
            "[a](a.rs) [b](<b c.rs>) [c][] [d][D] [e]\n\n[c]: c.rs\n[d]: <d e.rs> \"T\"\n[e]: e.rs\n",
        );

        assert_eq!(destinations(&file), written_destinations(&file));
        assert_eq!(
            destinations(&file),
            ["a.rs", "b c.rs", "c.rs", "d e.rs", "e.rs"]
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
    fn one_character_label_defines_a_markdown_reference() {
        let file = markdown_file("See [x] and [yy].\n\n[x]: a.md\n[yy]: b.md\n");

        assert_eq!(
            pairs(&file),
            [pair("x", "a.md"), pair("yy", "b.md")],
            "`[x]: a.md` is a definition, not a task-list checkbox, and `[yy]` on the next line survives",
        );
    }

    #[test]
    fn mid_paragraph_and_heading_lines_define_nothing_and_the_real_definition_wins() {
        let file = markdown_file(
            "See [x] and [yy].\n\nsome text\n[x]: wrong.md\n\n# [yy]: wrong.md\n\n[x]: a.md\n[yy]: b.md\n",
        );

        assert_eq!(
            pairs(&file),
            [
                pair("x", "a.md"),
                pair("yy", "b.md"),
                pair("x", "a.md"),
                pair("yy", "b.md")
            ],
            "`[x]: wrong.md` continues a paragraph and `# [yy]: wrong.md` is a heading, so both are links, and every link uses the later real definitions",
        );
    }

    #[test]
    fn definition_after_a_heading_still_defines() {
        let file = markdown_file("See [x].\n\n# Title\n[x]: a.md\n");

        assert_eq!(
            pairs(&file),
            [pair("x", "a.md")],
            "a definition directly under a heading starts its own block",
        );
    }

    #[test]
    fn task_list_items_define_nothing() {
        let file = markdown_file("- [x] done\n* [ ] todo\n\nSee [x].\n");

        assert!(
            links(&file).is_empty(),
            "task-list items are not definitions, so `[x]` has no destination",
        );
    }

    #[test]
    fn one_character_label_defines_a_reference_in_a_rust_comment_run() {
        let file =
            rust_file("/// See [x] and [yy].\n///\n/// [x]: a.md\n/// [yy]: b.md\nfn f() {}\n");

        assert_eq!(
            pairs(&file),
            [pair("x", "a.md"), pair("yy", "b.md")],
            "one-character labels define links in `///` runs too",
        );
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
    fn rust_link_wrapped_across_two_line_comments_is_one_link() {
        let text =
            "fn a() {}\n/// See [the session\n/// rules](../docs/auth.md#sessions).\nfn f() {}\n";
        let file = rust_file(text);

        let found = links(&file);
        let link = found.first().expect("the two `///` lines hold one link");

        assert_eq!(
            (link.text.as_str(), link.destination.as_str()),
            ("the session\n rules", "../docs/auth.md#sessions"),
            "the link text is read without the `///` between its lines",
        );
        assert_eq!(
            text.get(link.range.clone()),
            Some("[the session\n/// rules](../docs/auth.md#sessions)"),
            "the range is the file's own, from `[` on row 1 to `)` on row 2",
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn reference_definition_on_another_line_of_the_same_comment_run_applies() {
        let file = rust_file(
            "/// See [the rules][rules] and [Rules].\n///\n/// [rules]: ../docs/auth.md#rules\nfn f() {}\n",
        );

        assert_eq!(
            pairs(&file),
            [
                pair("the rules", "../docs/auth.md#rules"),
                pair("Rules", "../docs/auth.md#rules"),
            ],
            "adjacent `///` lines are one region, so the definition applies to the whole run",
        );
    }

    #[test]
    fn blank_line_code_or_another_comment_kind_splits_a_comment_run() {
        for text in [
            "/// See [the rules][rules].\n\n/// [rules]: ../docs/auth.md#rules\nfn f() {}\n",
            "/// See [the rules][rules].\nfn f() {}\n/// [rules]: ../docs/auth.md#rules\nfn g() {}\n",
            "/// See [the rules][rules].\n// [rules]: ../docs/auth.md#rules\nfn f() {}\n",
            "/// See [the\n\n/// rules](../docs/auth.md#rules).\nfn f() {}\n",
        ] {
            let file = rust_file(text);

            assert_eq!(
                links(&file),
                [],
                "the comments are separate regions: {text:?}"
            );
        }
    }

    #[test]
    fn ts_line_comment_run_joins_a_wrapped_link_and_a_definition() {
        let text = "// See [the\n// guide](guide.md) and [api].\n//\n// [api]: api.md#f\nfunction f() {}\n";
        let file = builtin_file("/repo/src/a.ts", text);

        assert_eq!(
            spans(&file),
            [
                span("the\n guide", "guide.md", "[the\n// guide](guide.md)"),
                span("api", "api.md#f", "[api]"),
            ],
        );
    }

    #[test]
    fn python_hash_comment_run_joins_a_wrapped_link_and_a_definition() {
        let text = "# See [the\n# guide](guide.md) and [api].\n# [api]: api.md#f\n\n# [api]\ndef f():\n    pass\n";
        let file = builtin_file("/repo/src/a.py", text);

        assert_eq!(
            spans(&file),
            [
                span("the\n guide", "guide.md", "[the\n# guide](guide.md)"),
                span("api", "api.md#f", "[api]"),
            ],
            "the `[api]` after the blank line is in another run, where `api` is undefined",
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

    /// Pinned from `printf` of the file's bytes piped to `shasum -a 256`, not from supdoc.
    const HASH_WITH_B: &str = "a75d72742a6c0fb87ad0e9a2abd32c4f7bc13863cfb597f3d7b7652c78675779";
    /// The same bytes with `b.md` spelled `c.md`.
    const HASH_WITH_C: &str = "a415e4296c9a189b7b6ae079968e260507e2290831ed281d5a90406615201863";

    #[test]
    fn comment_link_resolves_relative_to_the_code_file() {
        let file = rust_file("fn a() {}\n\n// see [b](../docs/b.md)\n");
        let raw = links(&file)
            .into_iter()
            .next()
            .expect("the comment holds one link");
        let source = crate::link::SourcePath::from_relative(Path::new("src/server.rs"))
            .expect("the fixture source path should be relative");

        let parsed = crate::link::destination::parse_destination(
            &raw,
            &source,
            &std::collections::BTreeMap::new(),
            &crate::read::cartridge::Languages::builtin(),
        );

        let expected = crate::link::destination::ParsedLink {
            target: "docs/b.md"
                .parse()
                .expect("the expected target should be valid"),
            symbol: None,
        };
        assert_eq!(
            parsed,
            Some(Ok(expected)),
            "`../docs/b.md` written in `src/server.rs` is `docs/b.md` from the project root",
        );
    }

    #[test]
    fn editing_a_comment_link_changes_whole_file_hash_of_its_file() {
        let before = rust_file("fn a() {}\n\n// see [b](../docs/b.md)\n");
        let after = rust_file("fn a() {}\n\n// see [b](../docs/c.md)\n");

        assert_eq!(
            crate::read::hash::hash_file(&before).to_string(),
            HASH_WITH_B
        );
        assert_eq!(
            crate::read::hash::hash_file(&after).to_string(),
            HASH_WITH_C,
            "a whole-file hash covers comments, so editing a link in one changes it (03 R6)",
        );
    }
}
