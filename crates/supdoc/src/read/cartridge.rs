//! The [`Cartridge`] trait that plugs a language into supdoc, the values it exchanges with the
//! rest of supdoc, and the [`Languages`] lookup that finds a cartridge for a file.
//!
//! Constructors that need the reading machinery (`SourceFile::parse`, `SymbolTable::resolve`,
//! `Languages::builtin`) live in the sibling modules; this module fixes the shapes they share.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::ops::{ControlFlow, Range};
use std::path::Path;
use std::time::Instant;

use tree_sitter::{ParseOptions, Parser, Tree};

use crate::cartridges::data::KeyPathBudget;
use crate::limits::TimeLimit;
use crate::link::SymbolPath;
use crate::path::AbsolutePath;
use crate::read::error::{CartridgeLoadError, ReadError};

/// One language. The rest of supdoc only ever talks to this trait.
///
/// Most languages are data: a grammar plus tree-sitter's own `tags.scm` and `injections.scm`
/// query files. Languages that need real code (Python's `__init__` attributes, Markdown's
/// headings) override `symbols` or `markdown_regions` with their own logic.
pub(crate) trait Cartridge: Send + Sync + fmt::Debug {
    /// Stable id: `"rust"`, `"markdown"`. Appears in errors and `info` output.
    fn id(&self) -> LanguageId;

    /// File extensions this language reads: lowercase, without the dot (`["ts", "mts", "cts"]`).
    fn extensions(&self) -> &'static [&'static str];

    /// The tree-sitter grammar. Loading it compiles nothing.
    fn grammar(&self) -> &tree_sitter::Language;

    /// Compiles whatever the cartridge builds before its first file is read, once per run.
    /// [`SourceFile::from_text`] calls it, so every [`SourceFile`] has a loaded cartridge.
    ///
    /// # Errors
    ///
    /// [`CartridgeLoadError`] when a built-in query doesn't compile: a supdoc bug that fails the
    /// run with exit code 70.
    fn load(&self) -> Result<(), CartridgeLoadError> {
        Ok(())
    }

    /// Refuses `_text`, the file at `_path`, before it is parsed, when the grammar can't parse it
    /// faithfully. Runs before the parser is built, so a refused file builds no tree. The default
    /// admits everything.
    ///
    /// # Errors
    ///
    /// The [`ReadError`] naming why the file can't be parsed, such as
    /// [`ReadError::TooManyLines`].
    fn admit(&self, _path: &AbsolutePath, _text: &str) -> Result<(), ReadError> {
        Ok(())
    }

    /// Whether files of this language are walked as link sources.
    ///
    /// `false` makes the language a link target only: its files still resolve as destinations
    /// and expose symbols, but the walk never reads them for links or code blocks.
    fn reads_links(&self) -> bool {
        true
    }

    /// The symbols `file` exposes. Query-driven cartridges run `tags.scm`.
    ///
    /// # Errors
    ///
    /// [`ReadError::Timeout`] when `file`'s [`Deadline`] passes first. No partial table is ever
    /// returned.
    fn symbols(&self, file: &SourceFile) -> Result<SymbolTable, ReadError>;

    /// The regions of `file` written in Markdown, where links are read, in file order.
    /// Query-driven cartridges return the `injections.scm` captures whose `injection.language` is
    /// `markdown_inline`, with each run of adjacent line comments joined into one region.
    ///
    /// # Errors
    ///
    /// [`ReadError::Timeout`] when `file`'s [`Deadline`] passes first.
    fn markdown_regions(&self, file: &SourceFile) -> Result<Vec<MarkdownRegion>, ReadError>;
}

/// A cartridge's stable id: `"rust"`, `"markdown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LanguageId(&'static str);

impl LanguageId {
    /// Wraps a built-in id. Ids are lowercase and fixed at compile time.
    #[must_use]
    pub(crate) const fn new(id: &'static str) -> Self {
        Self(id)
    }

    /// The id as text.
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for LanguageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A query-driven cartridge's queries, compiled against its grammar.
#[derive(Debug)]
pub(crate) struct CompiledQueries {
    /// `tags.scm`: `@definition.*`, `@name` and the `@supdoc.scope*` extensions.
    tags: tree_sitter::Query,
    /// `injections.scm`: `@injection.content` with `injection.language`.
    injections: tree_sitter::Query,
}

impl CompiledQueries {
    /// Pairs the two compiled query files.
    #[must_use]
    pub(crate) fn new(tags: tree_sitter::Query, injections: tree_sitter::Query) -> Self {
        Self { tags, injections }
    }

    /// The compiled `tags.scm`.
    #[must_use]
    pub(crate) fn tags(&self) -> &tree_sitter::Query {
        &self.tags
    }

    /// The compiled `injections.scm`.
    #[must_use]
    pub(crate) fn injections(&self) -> &tree_sitter::Query {
        &self.injections
    }
}

/// A parsed file. Text and tree are held together, so every byte range in the tree is valid for
/// the text. That is what makes hashing infallible.
#[derive(Debug)]
pub(crate) struct SourceFile {
    /// Where the file was read from.
    path: AbsolutePath,
    /// The whole file, UTF-8.
    text: String,
    /// The tree parsed from `text` by `language`'s grammar.
    tree: tree_sitter::Tree,
    /// The language that parsed it.
    language: &'static dyn Cartridge,
    /// When reading the file must stop: started before the parse, and checked by every later
    /// parse and query of the file (SEC-1).
    deadline: Deadline,
    /// The ceilings a data cartridge builds the file's key paths under (SEC-1).
    key_paths: KeyPathBudget,
}

impl SourceFile {
    /// Parses `text` with `language`'s grammar, giving up when `deadline` passes. Pure apart from
    /// reading the clock: the text is already in memory.
    ///
    /// For text that is never a data file's symbols, such as a Markdown run or a fence: its key
    /// paths, if any were built, would be under the default ceilings. A file read from disk takes
    /// the run's ceilings through [`SourceFile::from_text_under`].
    ///
    /// # Errors
    ///
    /// As [`SourceFile::from_text_under`].
    pub(crate) fn from_text(
        path: AbsolutePath,
        text: String,
        language: &'static dyn Cartridge,
        deadline: Deadline,
    ) -> Result<Self, ReadError> {
        Self::from_text_under(path, text, language, deadline, KeyPathBudget::defaults())
    }

    /// Parses `text` with `language`'s grammar, giving up when `deadline` passes, for a data
    /// cartridge to build its key paths under `key_paths`. Pure apart from reading the clock: the
    /// text is already in memory.
    ///
    /// # Errors
    ///
    /// - [`ReadError::CartridgeLoad`] when `language`'s built-in queries don't compile, which is
    ///   a supdoc bug that fails the run.
    /// - The error [`Cartridge::admit`] refuses `text` with, such as [`ReadError::TooManyLines`],
    ///   before the parser is built.
    /// - [`ReadError::Timeout`] when `deadline` passes during the parse.
    /// - [`ReadError::Internal`] when the grammar can't be loaded into a parser or tree-sitter
    ///   returns no tree for another reason, which is a supdoc bug.
    pub(crate) fn from_text_under(
        path: AbsolutePath,
        text: String,
        language: &'static dyn Cartridge,
        deadline: Deadline,
        key_paths: KeyPathBudget,
    ) -> Result<Self, ReadError> {
        if let Err(error) = language.load() {
            return Err(ReadError::CartridgeLoad { path, error });
        }
        language.admit(&path, &text)?;
        let internal = |path: &AbsolutePath, reason| ReadError::Internal {
            path: path.clone(),
            reason,
        };
        let mut parser = Parser::new();
        if parser.set_language(language.grammar()).is_err() {
            return Err(internal(&path, "the grammar could not be loaded"));
        }
        match deadline.parse(&mut parser, &text) {
            Ok(Some(tree)) => Ok(Self {
                path,
                text,
                tree,
                language,
                deadline,
                key_paths,
            }),
            Ok(None) => Err(internal(&path, "tree-sitter returned no tree")),
            Err(stopped) => Err(ReadError::Timeout {
                limit: deadline.limit(),
                step: "the parse",
                offset: stopped.offset,
                size: text.len(),
                path,
            }),
        }
    }

    /// The error for a step of reading this file that `deadline` stopped at `stopped`: `step`
    /// reads after "stopped", as in "the symbols query".
    #[must_use]
    pub(crate) fn timed_out(&self, step: &'static str, stopped: Stopped) -> ReadError {
        ReadError::Timeout {
            path: self.path.clone(),
            limit: self.deadline.limit(),
            step,
            offset: stopped.offset,
            size: self.text.len(),
        }
    }

    /// When reading this file must stop.
    #[must_use]
    pub(crate) const fn deadline(&self) -> Deadline {
        self.deadline
    }

    /// The ceilings a data cartridge builds this file's key paths under: the run's
    /// `max-key-segments` and `max-key-path-size` for a file read from disk.
    #[must_use]
    pub(crate) const fn key_paths(&self) -> KeyPathBudget {
        self.key_paths
    }

    /// This file under `deadline` instead, so a test can stop a query after a parse that ran.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = deadline;
        self
    }

    /// Where the file was read from.
    #[must_use]
    pub(crate) fn path(&self) -> &AbsolutePath {
        &self.path
    }

    /// The whole file.
    #[must_use]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The syntax tree.
    #[must_use]
    pub(crate) fn tree(&self) -> &tree_sitter::Tree {
        &self.tree
    }

    /// The language that parsed the file.
    #[must_use]
    pub(crate) fn language(&self) -> &'static dyn Cartridge {
        self.language
    }
}

/// When one read of a file must stop: its [`TimeLimit`], counted from just before its parse. One
/// deadline covers the parse and every later query and inline parse of that read, since a
/// pathological file can stall in any of them (SEC-1). The budget is per read: a file read once
/// as a source and again as a target gets it twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Deadline {
    /// The ceiling it was started with, for the error.
    limit: TimeLimit,
    /// The moment it passes, or `None` for no limit.
    at: Option<Instant>,
    /// For a test, how many checks it allows before it passes, counted on the checking thread,
    /// in place of the clock.
    #[cfg(test)]
    checks: Option<usize>,
}

#[cfg(test)]
thread_local! {
    /// How many times a deadline made by [`Deadline::after_checks`] has been checked on this
    /// thread.
    static CHECKS: Cell<usize> = const { Cell::new(0) };
}

/// How far a parse or query got before its [`Deadline`] stopped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stopped {
    /// The byte tree-sitter had reached.
    pub(crate) offset: usize,
}

impl Deadline {
    /// A deadline `limit` from now. Effect: reads the clock.
    #[must_use]
    pub(crate) fn start(limit: TimeLimit) -> Self {
        Self {
            limit,
            at: limit
                .get()
                .and_then(|duration| Instant::now().checked_add(duration)),
            #[cfg(test)]
            checks: None,
        }
    }

    /// A deadline that passes at its check after `checks` checks on this thread, so a test can
    /// stop a read at an exact point however fast the machine is. Resets this thread's count.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn after_checks(checks: usize) -> Self {
        CHECKS.set(0);
        Self {
            limit: crate::limits::Limits::PARSE_TIMEOUT,
            at: None,
            checks: Some(checks),
        }
    }

    /// The ceiling it was started with.
    #[must_use]
    pub(crate) const fn limit(self) -> TimeLimit {
        self.limit
    }

    /// Whether it has passed. Effect: reads the clock.
    #[must_use]
    pub(crate) fn passed(self) -> bool {
        #[cfg(test)]
        if let Some(checks) = self.checks {
            let seen = CHECKS.get().saturating_add(1);
            CHECKS.set(seen);
            return seen > checks;
        }
        self.at.is_some_and(|at| Instant::now() >= at)
    }

    /// A tree-sitter progress check: breaks once the deadline has passed, recording in `stopped`
    /// the byte the parse or query had reached.
    pub(crate) fn check(self, offset: usize, stopped: &Cell<Option<Stopped>>) -> ControlFlow<()> {
        if self.passed() {
            stopped.set(Some(Stopped { offset }));
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    /// `parser`'s tree for `text`, using whatever language and included ranges `parser` already
    /// has. `Ok(None)` when tree-sitter returns no tree for another reason.
    ///
    /// # Errors
    ///
    /// [`Stopped`] when the deadline passes during the parse. The parser is reset, so it can be
    /// used again.
    pub(crate) fn parse(self, parser: &mut Parser, text: &str) -> Result<Option<Tree>, Stopped> {
        let stopped = Cell::new(None);
        let mut progress =
            |state: &tree_sitter::ParseState| self.check(state.current_byte_offset(), &stopped);
        let bytes = text.as_bytes();
        let tree = parser.parse_with_options(
            &mut |offset, _point| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        );
        match stopped.get() {
            Some(stopped) => {
                parser.reset();
                Err(stopped)
            }
            None => Ok(tree),
        }
    }
}

/// One addressable symbol in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    /// Full dotted path: `Config.validate`, `RingBuffer.new`, `news.whats-new`.
    path: SymbolPath,
    /// Extra names that also resolve here (Markdown's GitHub anchor).
    aliases: Vec<SymbolPath>,
    /// Byte range in the file; valid for the `SourceFile` the symbol came from.
    range: Range<usize>,
}

impl Symbol {
    /// A symbol at `path`, also named by `aliases`, covering `range` of its file.
    #[must_use]
    pub(crate) const fn new(
        path: SymbolPath,
        aliases: Vec<SymbolPath>,
        range: Range<usize>,
    ) -> Self {
        Self {
            path,
            aliases,
            range,
        }
    }

    /// Full dotted path: `Config.validate`, `RingBuffer.new`, `news.whats-new`.
    #[must_use]
    pub(crate) const fn path(&self) -> &SymbolPath {
        &self.path
    }

    /// Extra names that also resolve here (Markdown's GitHub anchor).
    #[must_use]
    pub(crate) fn aliases(&self) -> &[SymbolPath] {
        &self.aliases
    }

    /// Byte range in the file; valid for the `SourceFile` the symbol came from.
    #[must_use]
    pub(crate) fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Widens the range to also cover `other`, for twins merged into one symbol.
    pub(crate) fn cover(&mut self, other: &Range<usize>) {
        self.range.start = self.range.start.min(other.start);
        self.range.end = self.range.end.max(other.end);
    }
}

/// Every symbol a file exposes, in the order the cartridge produced them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SymbolTable {
    /// The symbols.
    symbols: Vec<Symbol>,
}

impl SymbolTable {
    /// Collects a file's symbols.
    #[must_use]
    pub(crate) fn new(symbols: Vec<Symbol>) -> Self {
        Self { symbols }
    }

    /// Every symbol, in the order the cartridge produced them.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter()
    }
}

/// One place in a file where links are read, parsed as one piece of inline Markdown.
///
/// Usually one byte range: a paragraph, a block comment, a docstring. A run of adjacent line
/// comments is one region of several ranges, one per comment body without its marker, so a link
/// or a reference definition can span its lines while every node keeps the file's own offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkdownRegion {
    /// Non-empty, in file order and not overlapping: what `Parser::set_included_ranges` accepts.
    ranges: Vec<tree_sitter::Range>,
}

impl MarkdownRegion {
    /// `ranges` read together as one region.
    ///
    /// `None` if `ranges` is empty, or a range ends before it starts or overlaps the one before it.
    #[must_use]
    pub(crate) fn new(ranges: Vec<tree_sitter::Range>) -> Option<Self> {
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
    pub(crate) fn ranges(&self) -> &[tree_sitter::Range] {
        &self.ranges
    }

    /// The part of `text[span]` that lies inside this region, its pieces joined.
    ///
    /// For a run of line comments this is the text without the markers between its lines, which
    /// is how a link that wraps across lines reads.
    #[must_use]
    pub(crate) fn text_within<'text>(
        &self,
        span: Range<usize>,
        text: &'text str,
    ) -> Cow<'text, str> {
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
pub(crate) struct Languages {
    /// Every cartridge, each once, in registration order.
    cartridges: Vec<&'static dyn Cartridge>,
    /// Lowercase extension → the first cartridge registered for it.
    by_extension: HashMap<&'static str, &'static dyn Cartridge>,
}

impl Languages {
    /// Registers `cartridges` in order. If two claim the same extension, the first one registered
    /// keeps it; the built-in registry's tests guarantee that never happens there.
    #[must_use]
    pub(crate) fn new(cartridges: Vec<&'static dyn Cartridge>) -> Self {
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
    pub(crate) fn for_path(&self, path: &Path) -> Option<&'static dyn Cartridge> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        self.by_extension.get(extension.as_str()).copied()
    }

    /// Every cartridge, each once, in registration order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &'static dyn Cartridge> {
        self.cartridges.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;
    use crate::limits::Limits;

    /// A data-only cartridge over the Rust grammar, standing in for any language.
    #[derive(Debug)]
    struct TestCartridge {
        /// Its id.
        id: &'static str,
        /// Its extensions.
        extensions: &'static [&'static str],
        /// The grammar, created on first use.
        grammar: OnceLock<tree_sitter::Language>,
    }

    impl TestCartridge {
        const fn new(id: &'static str, extensions: &'static [&'static str]) -> Self {
            Self {
                id,
                extensions,
                grammar: OnceLock::new(),
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

        fn symbols(&self, _file: &SourceFile) -> Result<SymbolTable, ReadError> {
            Ok(SymbolTable::default())
        }

        fn markdown_regions(&self, _file: &SourceFile) -> Result<Vec<MarkdownRegion>, ReadError> {
            Ok(Vec::new())
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

        let file = SourceFile::from_text(
            path.clone(),
            "fn add() {}".to_owned(),
            &RUST,
            Deadline::start(Limits::PARSE_TIMEOUT),
        )
        .expect("the Rust grammar should parse Rust text");

        assert_eq!(file.path(), &path);
        assert_eq!(file.text(), "fn add() {}");
        assert_eq!(file.tree().root_node().kind(), "source_file");
        assert_eq!(file.tree().root_node().byte_range(), 0..file.text().len());
        assert_eq!(file.language().id(), LanguageId::new("rust"));
    }

    #[test]
    fn symbol_table_iterates_in_cartridge_order() {
        let symbol = |path: &str, range: Range<usize>| {
            Symbol::new(
                path.parse().expect("the fixture symbol should be valid"),
                Vec::new(),
                range,
            )
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
