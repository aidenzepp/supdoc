//! The [`Cartridge`] trait that plugs a language into supdoc, the values it exchanges with the
//! rest of supdoc, and the [`Languages`] lookup that finds a cartridge for a file (03).
//!
//! Constructors that need the reading machinery (`SourceFile::parse`, `SymbolTable::resolve`,
//! `Languages::builtin`) live in the sibling modules; this module fixes the shapes they share.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::path::Path;

use crate::link::SymbolPath;
use crate::path::AbsolutePath;

/// One language. The rest of supdoc only ever talks to this trait (03 R3).
///
/// Most languages are data: a grammar plus tree-sitter's own `tags.scm` and `injections.scm`
/// query files. Languages that need real code (Python's `__init__` attributes, Markdown's
/// headings) override `symbols` or `markdown_regions` with their own logic.
pub trait Cartridge: Send + Sync + fmt::Debug {
    /// Stable id: `"rust"`, `"markdown"`. Appears in errors and `info` output.
    fn id(&self) -> LanguageId;

    /// File extensions this language reads: lowercase, without the dot (`["ts", "mts", "cts"]`).
    fn extensions(&self) -> &'static [&'static str];

    /// The tree-sitter grammar.
    fn grammar(&self) -> &tree_sitter::Language;

    /// The cartridge's `tags.scm` and `injections.scm`, compiled once, lazily.
    fn queries(&self) -> &CompiledQueries;

    /// The symbols `file` exposes (03 R4). Query-driven cartridges run `tags.scm`.
    fn symbols(&self, file: &SourceFile) -> SymbolTable;

    /// The regions of `file` written in Markdown, where links are read, in file order (03 R9, R14).
    /// Query-driven cartridges return the `injections.scm` captures whose `injection.language` is
    /// `markdown_inline`, with each run of adjacent line comments joined into one region.
    fn markdown_regions(&self, file: &SourceFile) -> Vec<MarkdownRegion>;
}

/// A cartridge's stable id: `"rust"`, `"markdown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LanguageId(&'static str);

impl LanguageId {
    /// Wraps a built-in id. Ids are lowercase and fixed at compile time.
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    /// The id as text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for LanguageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A cartridge's queries, compiled against its grammar.
#[derive(Debug)]
pub struct CompiledQueries {
    /// `tags.scm`: `@definition.*`, `@name` and the `@supdoc.scope*` extensions.
    tags: tree_sitter::Query,
    /// `injections.scm`: `@injection.content` with `injection.language`.
    injections: tree_sitter::Query,
}

impl CompiledQueries {
    /// Pairs the two compiled query files.
    #[must_use]
    pub fn new(tags: tree_sitter::Query, injections: tree_sitter::Query) -> Self {
        Self { tags, injections }
    }

    /// The compiled `tags.scm`.
    #[must_use]
    pub fn tags(&self) -> &tree_sitter::Query {
        &self.tags
    }

    /// The compiled `injections.scm`.
    #[must_use]
    pub fn injections(&self) -> &tree_sitter::Query {
        &self.injections
    }
}

/// A parsed file. Text and tree are held together, so every byte range in the tree is valid for
/// the text. That is what makes hashing infallible.
#[derive(Debug)]
pub struct SourceFile {
    /// Where the file was read from.
    path: AbsolutePath,
    /// The whole file, UTF-8.
    text: String,
    /// The tree parsed from `text` by `language`'s grammar.
    tree: tree_sitter::Tree,
    /// The language that parsed it.
    language: &'static dyn Cartridge,
}

impl SourceFile {
    /// Parses `text` with `language`'s grammar. Pure: the text is already in memory.
    ///
    /// `None` only if the grammar can't be loaded into a parser or tree-sitter returns no tree.
    /// supdoc sets no timeout or cancellation, so either one is a supdoc bug, not a user error.
    #[must_use]
    pub fn from_text(
        path: AbsolutePath,
        text: String,
        language: &'static dyn Cartridge,
    ) -> Option<Self> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(language.grammar()).ok()?;
        let tree = parser.parse(&text, None)?;
        Some(Self {
            path,
            text,
            tree,
            language,
        })
    }

    /// Where the file was read from.
    #[must_use]
    pub fn path(&self) -> &AbsolutePath {
        &self.path
    }

    /// The whole file.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The syntax tree.
    #[must_use]
    pub fn tree(&self) -> &tree_sitter::Tree {
        &self.tree
    }

    /// The language that parsed the file.
    #[must_use]
    pub fn language(&self) -> &'static dyn Cartridge {
        self.language
    }
}

/// One addressable symbol in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Full dotted path: `Config.validate`, `RingBuffer.new`, `news.whats-new`.
    pub path: SymbolPath,
    /// Extra names that also resolve here (Markdown's GitHub anchor, 03 R12).
    pub aliases: Vec<SymbolPath>,
    /// Byte range in the file; valid for the `SourceFile` the symbol came from.
    pub range: Range<usize>,
}

/// Every symbol a file exposes, in the order the cartridge produced them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolTable {
    /// The symbols.
    symbols: Vec<Symbol>,
}

impl SymbolTable {
    /// Collects a file's symbols.
    #[must_use]
    pub fn new(symbols: Vec<Symbol>) -> Self {
        Self { symbols }
    }

    /// Every symbol, in the order the cartridge produced them.
    pub fn iter(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter()
    }
}

/// One place in a file where links are read, parsed as one piece of inline Markdown (03 R9, R14).
///
/// Usually one byte range: a paragraph, a block comment, a docstring. A run of adjacent line
/// comments is one region of several ranges, one per comment body without its marker, so a link
/// or a reference definition can span its lines while every node keeps the file's own offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownRegion {
    /// Non-empty, in file order and not overlapping: what `Parser::set_included_ranges` accepts.
    ranges: Vec<tree_sitter::Range>,
}

impl MarkdownRegion {
    /// `ranges` read together as one region.
    ///
    /// `None` if `ranges` is empty, or a range ends before it starts or overlaps the one before it.
    #[must_use]
    pub fn new(ranges: Vec<tree_sitter::Range>) -> Option<Self> {
        let well_formed = ranges
            .iter()
            .all(|range| range.start_byte <= range.end_byte);
        let ordered = ranges
            .iter()
            .zip(ranges.iter().skip(1))
            .all(|(before, after)| before.end_byte <= after.start_byte);
        (!ranges.is_empty() && well_formed && ordered).then_some(Self { ranges })
    }

    /// The ranges, in file order.
    #[must_use]
    pub fn ranges(&self) -> &[tree_sitter::Range] {
        &self.ranges
    }

    /// The part of `text[span]` that lies inside this region, its pieces joined.
    ///
    /// For a run of line comments this is the text without the markers between its lines, which
    /// is how a link that wraps across lines reads.
    #[must_use]
    pub fn text_within<'text>(&self, span: Range<usize>, text: &'text str) -> Cow<'text, str> {
        let mut pieces = self.ranges.iter().filter_map(|range| {
            let start = span.start.max(range.start_byte);
            let end = span.end.min(range.end_byte);
            text.get(start..end).filter(|piece| !piece.is_empty())
        });
        let Some(first) = pieces.next() else {
            return Cow::Borrowed("");
        };
        match pieces.next() {
            None => Cow::Borrowed(first),
            Some(second) => {
                let mut joined = [first, second].concat();
                joined.extend(pieces);
                Cow::Owned(joined)
            }
        }
    }
}

impl From<tree_sitter::Range> for MarkdownRegion {
    /// A region of one range.
    fn from(range: tree_sitter::Range) -> Self {
        Self {
            ranges: vec![range],
        }
    }
}

/// Every cartridge supdoc knows about, looked up by file extension.
///
/// Passed in explicitly, never reached through a global, so a project-specific set can be built
/// later without changing anything that consumes it.
#[derive(Debug)]
pub struct Languages {
    /// Every cartridge, each once, in registration order.
    cartridges: Vec<&'static dyn Cartridge>,
    /// Lowercase extension → the first cartridge registered for it.
    by_extension: HashMap<&'static str, &'static dyn Cartridge>,
}

impl Languages {
    /// Registers `cartridges` in order. If two claim the same extension, the first one registered
    /// keeps it; the built-in registry's tests guarantee that never happens there.
    #[must_use]
    pub fn new(cartridges: Vec<&'static dyn Cartridge>) -> Self {
        let mut by_extension = HashMap::new();
        for &cartridge in &cartridges {
            for &extension in cartridge.extensions() {
                let _first = by_extension.entry(extension).or_insert(cartridge);
            }
        }
        Self {
            cartridges,
            by_extension,
        }
    }

    /// The cartridge for `path`'s extension, matched case-insensitively.
    #[must_use]
    pub fn for_path(&self, path: &Path) -> Option<&'static dyn Cartridge> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        self.by_extension.get(extension.as_str()).copied()
    }

    /// Every cartridge, each once, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &'static dyn Cartridge> {
        self.cartridges.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    /// A data-only cartridge over the Rust grammar, standing in for any language.
    #[derive(Debug)]
    struct TestCartridge {
        /// Its id.
        id: &'static str,
        /// Its extensions.
        extensions: &'static [&'static str],
        /// The grammar, created on first use.
        grammar: OnceLock<tree_sitter::Language>,
        /// Empty queries, compiled on first use.
        queries: OnceLock<CompiledQueries>,
    }

    impl TestCartridge {
        const fn new(id: &'static str, extensions: &'static [&'static str]) -> Self {
            Self {
                id,
                extensions,
                grammar: OnceLock::new(),
                queries: OnceLock::new(),
            }
        }
    }

    impl Cartridge for TestCartridge {
        fn id(&self) -> LanguageId {
            LanguageId::new(self.id)
        }

        fn extensions(&self) -> &'static [&'static str] {
            self.extensions
        }

        fn grammar(&self) -> &tree_sitter::Language {
            self.grammar
                .get_or_init(|| tree_sitter_rust::LANGUAGE.into())
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

        fn symbols(&self, _file: &SourceFile) -> SymbolTable {
            SymbolTable::default()
        }

        fn markdown_regions(&self, _file: &SourceFile) -> Vec<MarkdownRegion> {
            Vec::new()
        }
    }

    static TYPESCRIPT: TestCartridge = TestCartridge::new("ts", &["ts", "mts", "cts"]);
    static RUST: TestCartridge = TestCartridge::new("rust", &["rs"]);
    static CLAIMS_RS_TOO: TestCartridge = TestCartridge::new("rust-again", &["rs"]);

    fn id_for(languages: &Languages, path: &str) -> Option<&'static str> {
        languages
            .for_path(Path::new(path))
            .map(|cartridge| cartridge.id().as_str())
    }

    #[test]
    fn for_path_finds_the_cartridge_for_each_of_its_extensions() {
        let languages = Languages::new(vec![&TYPESCRIPT, &RUST]);

        assert_eq!(id_for(&languages, "web/api.ts"), Some("ts"));
        assert_eq!(id_for(&languages, "web/api.cts"), Some("ts"));
        assert_eq!(id_for(&languages, "src/lib.rs"), Some("rust"));
    }

    #[test]
    fn for_path_ignores_extension_case() {
        let languages = Languages::new(vec![&RUST]);

        assert_eq!(id_for(&languages, "src/LIB.RS"), Some("rust"));
    }

    #[test]
    fn for_path_is_none_for_unknown_or_missing_extensions() {
        let languages = Languages::new(vec![&RUST]);

        assert_eq!(id_for(&languages, "logo.png"), None);
        assert_eq!(id_for(&languages, "Makefile"), None);
        assert_eq!(
            id_for(&languages, ".rs"),
            None,
            "a dotfile has no extension"
        );
    }

    #[test]
    fn iter_yields_each_cartridge_once_in_registration_order() {
        let languages = Languages::new(vec![&TYPESCRIPT, &RUST]);

        assert_eq!(
            languages
                .iter()
                .map(|cartridge| cartridge.id().as_str())
                .collect::<Vec<_>>(),
            ["ts", "rust"],
        );
    }

    #[test]
    fn first_cartridge_registered_keeps_a_shared_extension() {
        let languages = Languages::new(vec![&RUST, &CLAIMS_RS_TOO]);

        assert_eq!(id_for(&languages, "src/lib.rs"), Some("rust"));
        assert_eq!(
            languages.iter().count(),
            2,
            "both cartridges stay registered"
        );
    }

    #[test]
    fn source_file_parses_text_with_its_cartridge_grammar() {
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");

        let file = SourceFile::from_text(path.clone(), "fn add() {}".to_owned(), &RUST)
            .expect("the Rust grammar should parse Rust text");

        assert_eq!(file.path(), &path);
        assert_eq!(file.text(), "fn add() {}");
        assert_eq!(file.tree().root_node().kind(), "source_file");
        assert_eq!(file.tree().root_node().byte_range(), 0..file.text().len());
        assert_eq!(file.language().id(), LanguageId::new("rust"));
        assert_eq!(file.language().queries().tags().pattern_count(), 0);
    }

    #[test]
    fn symbol_table_iterates_in_cartridge_order() {
        let symbol = |path: &str, range: Range<usize>| Symbol {
            path: path.parse().expect("the fixture symbol should be valid"),
            aliases: Vec::new(),
            range,
        };
        let table = SymbolTable::new(vec![symbol("b", 0..1), symbol("a", 2..3)]);

        assert_eq!(
            table
                .iter()
                .map(|symbol| symbol.path.to_string())
                .collect::<Vec<_>>(),
            ["b", "a"],
        );
    }
}
