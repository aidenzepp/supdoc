//! Markdown headings as symbols.
//!
//! Each heading is one symbol whose range is its whole section: the heading, its body and every
//! subsection, up to the next heading of the same or a higher level.
//!
//! A heading answers to two names that never clash, because GitHub anchors never contain `.`:
//!
//! - the **dotted path**: the slugs of its enclosing headings, outermost first, then its own
//!   (`news.whats-new`). Headings elsewhere in the file don't change it, so it survives reordering.
//! - the **GitHub anchor** (`whats-new`), kept as an alias so a link copied from GitHub resolves.
//!
//! Slugs follow GitHub: lowercase, keep letters, numbers, marks (general category M), connector
//! punctuation (Pc, which includes `_`) and `-`, drop other punctuation, turn each space into `-`.
//! Anchors repeated in a file get `-1`, `-2`, … in document order, as GitHub numbers them; headings
//! inside block quotes and lists count for that numbering, though they are not sections. Sibling headings that share a slug get the same suffixes in
//! their dotted path, scoped to the parent, so every dotted path is unique.
//!
//! A heading whose slug is empty (`## !!!`) can't be named: it isn't a symbol, and it is left out
//! of its children's dotted paths.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use tree_sitter::{Node, Parser};
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::link::{SymbolPath, SymbolSegment};
use crate::read::cartridge::{SourceFile, Symbol, SymbolTable};
use crate::read::error::ReadError;

/// The symbols of a Markdown `file`: one per nameable heading, in document order.
///
/// # Errors
///
/// [`ReadError::Timeout`] when the file's deadline stops a heading's inline parse. No partial
/// table is returned.
pub(crate) fn heading_symbols(file: &SourceFile) -> Result<SymbolTable, ReadError> {
    let root = file.tree().root_node();
    let mut headings = Vec::new();
    collect_headings(root, true, &mut headings);

    let mut reader = HeadingReader {
        file,
        inline: inline_parser(),
    };
    let mut anchors = Siblings::default();
    let mut open = vec![Open::root()];
    let mut symbols = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        if !heading.section {
            if let Ok(slug) = SymbolSegment::new(slugify(&reader.display(heading.node)?)) {
                let _anchor = anchors.claim(&slug);
            }
            continue;
        }
        while open
            .last()
            .is_some_and(|parent| parent.level >= heading.level)
        {
            let _closed = open.pop();
        }
        let Ok(slug) = SymbolSegment::new(slugify(&reader.display(heading.node)?)) else {
            continue;
        };
        let Some(parent) = open.last_mut() else {
            continue;
        };
        let path = match &parent.path {
            Some(parent_path) => parent_path.child(parent.children.claim(&slug)),
            None => SymbolPath::root(parent.children.claim(&slug)),
        };
        let end = headings
            .iter()
            .skip(index + 1)
            .find(|next| next.section && next.level <= heading.level)
            .map_or(root.end_byte(), |next| next.node.start_byte());
        let range = heading.node.start_byte()..end;
        symbols.push(symbol(path.clone(), anchors.claim(&slug), range));
        open.push(Open {
            level: heading.level,
            path: Some(path),
            children: Siblings::default(),
        });
    }
    Ok(SymbolTable::new(symbols))
}

/// A heading found in the block tree.
struct Heading<'tree> {
    /// 1 for `#` and `===`, up to 6.
    level: u8,
    /// The `atx_heading` or `setext_heading` node.
    node: Node<'tree>,
    /// Whether it sits at the document's top level and so owns a section. Headings inside block
    /// quotes or lists take part in anchor numbering only.
    section: bool,
}

/// Appends every heading under `node`, in document order. Headings inside block quotes or lists
/// are included, marked as having no section, because GitHub numbers their anchors too.
///
/// The grammar nests headings in `section` nodes, but not reliably for setext headings, so only
/// the headings themselves are read from it.
fn collect_headings<'tree>(node: Node<'tree>, section: bool, out: &mut Vec<Heading<'tree>>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "section" => collect_headings(child, section, out),
            "atx_heading" | "setext_heading" => {
                if let Some(level) = level_of(child) {
                    out.push(Heading {
                        level,
                        node: child,
                        section,
                    });
                }
            }
            "block_quote" | "list" | "list_item" => collect_headings(child, false, out),
            _ => {}
        }
    }
}

/// The level of an ATX or setext heading, from its marker or underline.
fn level_of(heading: Node<'_>) -> Option<u8> {
    let mut cursor = heading.walk();
    heading
        .children(&mut cursor)
        .find_map(|child| match child.kind() {
            "atx_h1_marker" | "setext_h1_underline" => Some(1),
            "atx_h2_marker" | "setext_h2_underline" => Some(2),
            "atx_h3_marker" => Some(3),
            "atx_h4_marker" => Some(4),
            "atx_h5_marker" => Some(5),
            "atx_h6_marker" => Some(6),
            _ => None,
        })
}

/// A heading whose section is still open: where its children are named.
struct Open {
    /// The heading's level; the root is 0.
    level: u8,
    /// Its dotted path. `None` for the root.
    path: Option<SymbolPath>,
    /// The slugs its direct children have claimed.
    children: Siblings,
}

impl Open {
    /// The document itself, which every top-level heading hangs from.
    fn root() -> Self {
        Self {
            level: 0,
            path: None,
            children: Siblings::default(),
        }
    }
}

/// A parser for headings' inline content, or `None` if the grammar can't be loaded (a supdoc
/// bug). Headings are then slugged from their raw text.
fn inline_parser() -> Option<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .ok()?;
    Some(parser)
}

/// Reads what a heading displays.
struct HeadingReader<'file> {
    /// The file whose headings are read, for its text and its deadline.
    file: &'file SourceFile,
    /// Reads a heading's inline markup.
    inline: Option<Parser>,
}

impl HeadingReader<'_> {
    /// The text `heading` displays, with its markup removed. Empty for an empty heading.
    ///
    /// # Errors
    ///
    /// [`ReadError::Timeout`] when the file's deadline stops the inline parse.
    fn display(&mut self, heading: Node<'_>) -> Result<String, ReadError> {
        let content = if heading.kind() == "setext_heading" {
            let mut cursor = heading.walk();
            heading
                .children(&mut cursor)
                .find(|child| child.kind() == "paragraph")
                .and_then(first_inline)
        } else {
            first_inline(heading)
        };
        let Some(raw) = content.and_then(|node| self.file.text().get(node.byte_range())) else {
            return Ok(String::new());
        };
        let tree = match self.inline.as_mut() {
            Some(parser) => self
                .file
                .deadline()
                .parse(parser, raw)
                .map_err(|stopped| self.file.timed_out("a heading's inline parse", stopped))?,
            None => None,
        };
        Ok(tree.map_or_else(
            || raw.to_owned(),
            |tree| {
                let mut shown = String::new();
                displayed_text(tree.root_node(), raw, &mut shown);
                shown
            },
        ))
    }
}

/// The first `inline` child of `node`.
fn first_inline(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "inline")
}

/// The symbol for a section, named by `path` and answering to `anchor` too.
fn symbol(path: SymbolPath, anchor: SymbolSegment, range: Range<usize>) -> Symbol {
    let anchor = SymbolPath::root(anchor);
    let aliases = if anchor == path {
        Vec::new()
    } else {
        vec![anchor]
    };
    Symbol::new(path, aliases, range)
}

/// The text `node` shows once rendered: markup delimiters dropped, links and images reduced to
/// their text, escapes resolved.
fn displayed_text(node: Node<'_>, source: &str, out: &mut String) {
    let mut position = node.start_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        push_between(source, position, child.start_byte(), out);
        position = child.end_byte();
        match child.kind() {
            kind if kind.ends_with("_delimiter") => {}
            "inline_link"
            | "full_reference_link"
            | "collapsed_reference_link"
            | "shortcut_link"
            | "image" => {
                let mut parts = child.walk();
                for part in child.children(&mut parts) {
                    if matches!(part.kind(), "link_text" | "image_description") {
                        displayed_text(part, source, out);
                    }
                }
            }
            "backslash_escape" => {
                if let Some(escaped) = source.get(child.start_byte() + 1..child.end_byte()) {
                    out.push_str(escaped);
                }
            }
            "html_tag" => {}
            _ => displayed_text(child, source, out),
        }
    }
    push_between(source, position, node.end_byte(), out);
}

/// Appends `source[start..end]`, if it is a valid slice.
fn push_between(source: &str, start: usize, end: usize, out: &mut String) {
    if let Some(gap) = source.get(start..end) {
        out.push_str(gap);
    }
}

/// GitHub's heading slug of `text`. Empty if nothing in `text` survives.
fn slugify(text: &str) -> String {
    text.trim()
        .trim_end_matches('#')
        .trim_end()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c == '-' || c.is_alphanumeric() || is_mark_or_connector(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// Whether `c` is in general category M (marks) or Pc (connector punctuation), which GitHub keeps.
fn is_mark_or_connector(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
            | GeneralCategory::ConnectorPunctuation
    )
}

/// Slugs already used in one scope, and how to number the next repeat. File-wide, it numbers
/// GitHub's anchors; per parent, it numbers dotted-path segments.
#[derive(Debug, Default)]
struct Siblings {
    /// Every name handed out, suffixed or not.
    used: HashSet<SymbolSegment>,
    /// The last suffix tried for each slug.
    suffixes: HashMap<SymbolSegment, usize>,
}

impl Siblings {
    /// `slug` if unused, else `slug-1`, `slug-2`, … whichever is free first.
    fn claim(&mut self, slug: &SymbolSegment) -> SymbolSegment {
        let mut name = slug.clone();
        while self.used.contains(&name) {
            let next = self.suffixes.entry(slug.clone()).or_insert(0);
            *next += 1;
            name = slug.numbered(*next);
        }
        let _newly_used = self.used.insert(name.clone());
        name
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::hash::HashSettings;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::Deadline;
    use crate::read::hash::hash_symbol;

    fn parse(text: &str) -> SourceFile {
        let path = AbsolutePath::new(Path::new("/repo/docs/guide.md"))
            .expect("the fixture path should be absolute");
        let markdown = cartridge();
        SourceFile::parse(path, text.to_owned(), markdown)
            .expect("the Markdown grammar should parse any text")
    }

    /// Every symbol as `path` or `path (alias, alias)`, in document order.
    fn names(text: &str) -> Vec<String> {
        heading_symbols(&parse(text))
            .expect("the fixture should read")
            .iter()
            .map(|symbol| {
                let aliases: Vec<String> =
                    symbol.aliases().iter().map(ToString::to_string).collect();
                if aliases.is_empty() {
                    symbol.path().to_string()
                } else {
                    format!("{} ({})", symbol.path(), aliases.join(", "))
                }
            })
            .collect()
    }

    #[test]
    fn a_passed_deadline_stops_a_headings_inline_parse() {
        let deadline = Deadline::start("1ms".parse().expect("1ms should be a time limit"));
        while !deadline.passed() {
            std::hint::spin_loop();
        }
        // One long heading: the block parse has no query, so the inline parse meets the deadline.
        let file = parse(&format!("# {}\n", "[a](b.md) ".repeat(20_000))).with_deadline(deadline);

        let error =
            heading_symbols(&file).expect_err("the inline parse should stop at the deadline");

        assert_eq!(error.code(), "read-timeout", "{error}");
        assert!(
            matches!(
                error,
                ReadError::Timeout {
                    step: "a heading's inline parse",
                    ..
                }
            ),
            "the timeout should name the heading's inline parse: {error:?}"
        );
    }

    fn resolved_text<'a>(file: &'a SourceFile, query: &str) -> &'a str {
        let table = heading_symbols(file).expect("the fixture should read");
        let symbol = table
            .resolve(
                file.path(),
                &query.parse().expect("the query should be a valid symbol"),
            )
            .expect("the query should resolve to one heading");
        file.text()
            .get(symbol.range().clone())
            .expect("a heading range should lie on character boundaries")
    }

    const NEWS: &str = "# News\n\nIntro.\n\n## What's new\n\nA lot.\n\n### Details\n\nMore.\n\n## Roadmap\n\nSoon.\n";

    #[test]
    fn markdown_github_anchor_and_dotted_path_resolve_same_section() {
        let file = parse(NEWS);

        let by_anchor = resolved_text(&file, "whats-new");
        let by_path = resolved_text(&file, "news.whats-new");

        assert_eq!(by_anchor, by_path, "both names should reach one section");
        assert!(
            by_path.starts_with("## What's new") && by_path.contains("### Details"),
            "a section runs from its heading through its subsections, got {by_path:?}"
        );
        assert!(
            !by_path.contains("## Roadmap"),
            "a section stops at the next heading of its level, got {by_path:?}"
        );
    }

    #[test]
    fn markdown_headings_nest_into_dotted_paths_with_anchor_aliases() {
        assert_eq!(
            names(NEWS),
            [
                "news",
                "news.whats-new (whats-new)",
                "news.whats-new.details (details)",
                "news.roadmap (roadmap)",
            ],
        );
    }

    #[test]
    fn markdown_duplicate_headings_get_suffixed_anchors() {
        let text = "# Guide\n\n## Setup\n\na\n\n## Setup\n\nb\n\n## Setup\n\nc\n";

        assert_eq!(
            names(text),
            [
                "guide",
                "guide.setup (setup)",
                "guide.setup-1 (setup-1)",
                "guide.setup-2 (setup-2)",
            ],
        );
    }

    #[test]
    fn markdown_anchor_suffix_counts_across_parents_but_dotted_path_does_not() {
        let text = "# A\n\n## Setup\n\n# B\n\n## Setup\n";

        assert_eq!(
            names(text),
            ["a", "a.setup (setup)", "b", "b.setup (setup-1)"],
        );
    }

    #[test]
    fn markdown_subsection_edit_changes_parent_hash() {
        let before = parse("# Guide\n\n## Setup\n\nInstall it.\n\n## Usage\n\nRun it.\n");
        let after = parse("# Guide\n\n## Setup\n\nInstall it twice.\n\n## Usage\n\nRun it.\n");
        let hash = |file: &SourceFile, query: &str| {
            let table = heading_symbols(file).expect("the fixture should read");
            let symbol = table
                .resolve(
                    file.path(),
                    &query.parse().expect("the query should be valid"),
                )
                .expect("the query should resolve");
            hash_symbol(file, symbol, HashSettings::default())
                .expect("the symbol should lie in its own file")
        };

        assert_ne!(
            hash(&before, "guide"),
            hash(&after, "guide"),
            "a section includes its subsections"
        );
        assert_ne!(hash(&before, "setup"), hash(&after, "setup"));
        assert_eq!(
            hash(&before, "usage"),
            hash(&after, "usage"),
            "a sibling section is untouched"
        );
    }

    #[test]
    fn markdown_slugs_follow_github_rules() {
        let text = "# Hello, World!\n\n## C++ & Rust\n\n## `code` and *emphasis*\n\n## A [link](x.md) here\n\n## Café Ünïcode\n\n## snake_case-name\n";

        assert_eq!(
            names(text),
            [
                "hello-world",
                "hello-world.c--rust (c--rust)",
                "hello-world.code-and-emphasis (code-and-emphasis)",
                "hello-world.a-link-here (a-link-here)",
                "hello-world.café-ünïcode (café-ünïcode)",
                "hello-world.snake_case-name (snake_case-name)",
            ],
        );
    }

    #[test]
    fn markdown_quoted_and_listed_headings_count_in_anchor_numbering() {
        assert_eq!(
            names("## Note\n\n> ## Note\n\n## Note\n"),
            ["note", "note-1 (note-2)"],
            "the quoted heading takes #note-1 on GitHub, so the last one is #note-2"
        );
        assert_eq!(
            names("## Note\n\n- ## Note\n\n## Note\n"),
            ["note", "note-1 (note-2)"],
        );
    }

    #[test]
    fn markdown_slugs_keep_marks_outside_the_latin_block() {
        for (heading, slug) in [
            ("हिन्दी", "हिन्दी"),
            ("ไทย ที่", "ไทย-ที่"),
            ("⚠️ Warning", "\u{fe0f}-warning"),
            ("a\u{20DD}b", "a\u{20DD}b"),
        ] {
            assert_eq!(
                names(&format!("# {heading}\n")),
                [slug],
                "the slug of {heading:?} should keep every mark"
            );
        }
    }

    #[test]
    fn markdown_setext_and_closing_hash_headings_are_symbols() {
        let text = "Title\n=====\n\nSub\n---\n\n### Closed ###\n";

        assert_eq!(
            names(text),
            ["title", "title.sub (sub)", "title.sub.closed (closed)"],
        );
    }

    #[test]
    fn markdown_heading_without_a_slug_is_skipped_and_leaves_its_children_unprefixed() {
        let text = "# Guide\n\n## !!!\n\n### Inner\n";

        assert_eq!(names(text), ["guide", "guide.inner (inner)"]);
    }

    #[test]
    fn markdown_text_before_the_first_heading_and_headings_in_fences_are_not_symbols() {
        let text = "Preface.\n\n```\n# not a heading\n```\n\n# Real\n";

        assert_eq!(names(text), ["real"]);
    }

    #[test]
    fn markdown_document_without_headings_has_no_symbols() {
        assert_eq!(names("Just prose.\n"), Vec::<String>::new());
    }
}
