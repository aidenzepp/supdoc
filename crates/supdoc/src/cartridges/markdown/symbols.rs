//! Markdown headings as symbols (03 R12).
//!
//! Each heading is one symbol whose range is its whole section: the heading, its body and every
//! subsection, up to the next heading of the same or a higher level (03 R7).
//!
//! A heading answers to two names that never clash, because GitHub anchors never contain `.`:
//!
//! - the **dotted path**: the slugs of its enclosing headings, outermost first, then its own
//!   (`news.whats-new`). Headings elsewhere in the file don't change it, so it survives reordering.
//! - the **GitHub anchor** (`whats-new`), kept as an alias so a link copied from GitHub resolves.
//!
//! Slugs follow GitHub: lowercase, keep letters, numbers, `_`, `-` and combining marks, drop other
//! punctuation, turn each space into `-`. Anchors repeated in a file get `-1`, `-2`, … in document
//! order, as GitHub numbers them. Sibling headings that share a slug get the same suffixes in
//! their dotted path, scoped to the parent, so every dotted path is unique.
//!
//! A heading whose slug is empty (`## !!!`) can't be named: it isn't a symbol, and it is left out
//! of its children's dotted paths.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use tree_sitter::{Node, Parser};

use crate::link::SymbolPath;
use crate::read::cartridge::{SourceFile, Symbol, SymbolTable};

/// The symbols of a Markdown `file`: one per nameable heading, in document order.
#[must_use]
pub fn heading_symbols(file: &SourceFile) -> SymbolTable {
    let root = file.tree().root_node();
    let mut headings = Vec::new();
    collect_headings(root, &mut headings);

    let mut reader = HeadingReader {
        text: file.text(),
        inline: inline_parser(),
    };
    let mut anchors = Siblings::default();
    let mut open = vec![Open::root()];
    let mut symbols = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        while open
            .last()
            .is_some_and(|parent| parent.level >= heading.level)
        {
            let _closed = open.pop();
        }
        let slug = slugify(&reader.display(heading.node));
        if slug.is_empty() {
            continue;
        }
        let Some(parent) = open.last_mut() else {
            continue;
        };
        let mut segments = parent.segments.clone();
        segments.push(parent.children.claim(&slug));
        let end = headings
            .iter()
            .skip(index + 1)
            .find(|next| next.level <= heading.level)
            .map_or(root.end_byte(), |next| next.node.start_byte());
        let range = heading.node.start_byte()..end;
        if let Some(symbol) = symbol(&segments, &anchors.claim(&slug), range) {
            symbols.push(symbol);
        }
        open.push(Open {
            level: heading.level,
            segments,
            children: Siblings::default(),
        });
    }
    SymbolTable::new(symbols)
}

/// A heading found in the block tree.
struct Heading<'tree> {
    /// 1 for `#` and `===`, up to 6.
    level: u8,
    /// The `atx_heading` or `setext_heading` node.
    node: Node<'tree>,
}

/// Appends every heading under `node`, in document order. Only headings at the document's top
/// level count: those inside block quotes or lists have no section of their own.
///
/// The grammar nests headings in `section` nodes, but not reliably for setext headings, so only
/// the headings themselves are read from it.
fn collect_headings<'tree>(node: Node<'tree>, out: &mut Vec<Heading<'tree>>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "section" => collect_headings(child, out),
            "atx_heading" | "setext_heading" => {
                if let Some(level) = level_of(child) {
                    out.push(Heading { level, node: child });
                }
            }
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
    /// Its dotted path. Empty for the root.
    segments: Vec<String>,
    /// The slugs its direct children have claimed.
    children: Siblings,
}

impl Open {
    /// The document itself, which every top-level heading hangs from.
    fn root() -> Self {
        Self {
            level: 0,
            segments: Vec::new(),
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
struct HeadingReader<'text> {
    /// The file's text.
    text: &'text str,
    /// Reads a heading's inline markup.
    inline: Option<Parser>,
}

impl HeadingReader<'_> {
    /// The text `heading` displays, with its markup removed. Empty for an empty heading.
    fn display(&mut self, heading: Node<'_>) -> String {
        let content = if heading.kind() == "setext_heading" {
            let mut cursor = heading.walk();
            heading
                .children(&mut cursor)
                .find(|child| child.kind() == "paragraph")
                .and_then(first_inline)
        } else {
            first_inline(heading)
        };
        let Some(raw) = content.and_then(|node| self.text.get(node.byte_range())) else {
            return String::new();
        };
        self.inline
            .as_mut()
            .and_then(|parser| parser.parse(raw, None))
            .map_or_else(
                || raw.to_owned(),
                |tree| {
                    let mut shown = String::new();
                    displayed_text(tree.root_node(), raw, &mut shown);
                    shown
                },
            )
    }
}

/// The first `inline` child of `node`.
fn first_inline(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "inline")
}

/// The symbol for a section, named by `segments` and answering to `anchor` too. `None` only if
/// a segment isn't a valid symbol segment, which a slug never is.
fn symbol(segments: &[String], anchor: &str, range: Range<usize>) -> Option<Symbol> {
    let path = SymbolPath::from_segments(segments.to_vec()).ok()?;
    let anchor = SymbolPath::from_segments(vec![anchor.to_owned()]).ok()?;
    let aliases = if anchor == path {
        Vec::new()
    } else {
        vec![anchor]
    };
    Some(Symbol {
        path,
        aliases,
        range,
    })
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
            c if c.is_alphanumeric() || matches!(c, '_' | '-' | '\u{0300}'..='\u{036F}') => Some(c),
            _ => None,
        })
        .collect()
}

/// Slugs already used in one scope, and how to number the next repeat. File-wide, it numbers
/// GitHub's anchors; per parent, it numbers dotted-path segments.
#[derive(Debug, Default)]
struct Siblings {
    /// Every name handed out, suffixed or not.
    used: HashSet<String>,
    /// The last suffix tried for each slug.
    suffixes: HashMap<String, usize>,
}

impl Siblings {
    /// `slug` if unused, else `slug-1`, `slug-2`, … whichever is free first.
    fn claim(&mut self, slug: &str) -> String {
        let mut name = slug.to_owned();
        while self.used.contains(&name) {
            let next = self.suffixes.entry(slug.to_owned()).or_insert(0);
            *next += 1;
            name = format!("{slug}-{next}");
        }
        let _newly_used = self.used.insert(name.clone());
        name
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::OnceLock;

    use super::*;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, CompiledQueries, LanguageId};
    use crate::read::hash::hash_symbol;

    /// A Markdown cartridge with no queries, enough to parse the block grammar.
    #[derive(Debug)]
    struct TestMarkdown {
        /// The grammar, created on first use.
        grammar: OnceLock<tree_sitter::Language>,
        /// Empty queries, compiled on first use.
        queries: OnceLock<CompiledQueries>,
    }

    impl Cartridge for TestMarkdown {
        fn id(&self) -> LanguageId {
            LanguageId::new("markdown")
        }

        fn extensions(&self) -> &'static [&'static str] {
            &["md"]
        }

        fn grammar(&self) -> &tree_sitter::Language {
            self.grammar.get_or_init(|| tree_sitter_md::LANGUAGE.into())
        }

        fn queries(&self) -> &CompiledQueries {
            self.queries.get_or_init(|| {
                let empty = || {
                    tree_sitter::Query::new(self.grammar(), "")
                        .expect("an empty query should compile against any grammar")
                };
                CompiledQueries::new(empty(), empty())
            })
        }

        fn symbols(&self, file: &SourceFile) -> SymbolTable {
            heading_symbols(file)
        }

        fn markdown_regions(&self, _file: &SourceFile) -> Vec<tree_sitter::Range> {
            Vec::new()
        }
    }

    static MARKDOWN: TestMarkdown = TestMarkdown {
        grammar: OnceLock::new(),
        queries: OnceLock::new(),
    };

    fn parse(text: &str) -> SourceFile {
        let path = AbsolutePath::new(Path::new("/repo/docs/guide.md"))
            .expect("the fixture path should be absolute");
        SourceFile::from_text(path, text.to_owned(), &MARKDOWN)
            .expect("the Markdown grammar should parse any text")
    }

    /// Every symbol as `path` or `path (alias, alias)`, in document order.
    fn names(text: &str) -> Vec<String> {
        heading_symbols(&parse(text))
            .iter()
            .map(|symbol| {
                let aliases: Vec<String> = symbol.aliases.iter().map(ToString::to_string).collect();
                if aliases.is_empty() {
                    symbol.path.to_string()
                } else {
                    format!("{} ({})", symbol.path, aliases.join(", "))
                }
            })
            .collect()
    }

    fn resolved_text<'a>(file: &'a SourceFile, query: &str) -> &'a str {
        let table = heading_symbols(file);
        let symbol = table
            .resolve(
                file.path(),
                &query.parse().expect("the query should be a valid symbol"),
            )
            .expect("the query should resolve to one heading");
        &file.text()[symbol.range.clone()]
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
            let table = heading_symbols(file);
            let symbol = table
                .resolve(
                    file.path(),
                    &query.parse().expect("the query should be valid"),
                )
                .expect("the query should resolve");
            hash_symbol(file, symbol)
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
