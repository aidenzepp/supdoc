//! TOML: a link target whose symbols are its keys, tables and array elements.
//!
//! Paths follow the key-path model the data cartridges share ([`crate::cartridges::data`]):
//! `Cargo.toml#package.version`, `Cargo.toml#dependencies.serde.features.0`, and `Cargo.toml#bin.0`
//! for the first `[[bin]]`. A bare key is one segment as written; a quoted key is decoded first, so
//! `"a.b" = 1` is `a~1b`. A dotted key `a.b = 1` names both `a` and `a.b`.
//!
//! A key/value pair's symbol covers its `pair` node. A table (`[a]`, the array `[[a]]` builds, the
//! parent of a dotted key or of `[a.b]`) is one symbol covering its first piece through its last,
//! so text between two pieces is inside its span: an unrelated edit there is reported for review,
//! a change to the table is never missed. A `[[a]]` element covers its header through its last
//! line, and every `[a.b]` that follows it.
//!
//! TOML files are link targets only: the cartridge returns no Markdown regions and the walk never
//! reads them as sources. The walk keeps its own stack and charges every symbol to the file's
//! key-path budget and deadline, so hostile nesting fails closed (SEC-1). It steps through each
//! container's children with a cursor rather than collecting them, holding a frame only for a
//! container whose own symbol it already charged, and it refuses a dotted key with more parts
//! than the segment budget before listing them, so memory beyond the parse grows with the budget,
//! not with the file.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use tree_sitter::{Node, TreeCursor};

use crate::cartridges::data::{KeyId, KeyName, KeyPathError, KeyPaths, index_name, key_name};
use crate::read::cartridge::{Cartridge, LanguageId, MarkdownRegion, SourceFile, SymbolTable};
use crate::read::error::ReadError;

/// The id, in errors and `info` output.
pub(crate) const ID: LanguageId = LanguageId::new("toml");

/// The extensions it reads, lowercase without the dot.
pub(crate) const EXTENSIONS: &[&str] = &["toml"];

/// The node kinds a value can be, so an array's elements are counted the same way TOML counts
/// them: comments and error nodes between elements name nothing.
const VALUE_KINDS: &[&str] = &[
    "string",
    "integer",
    "float",
    "boolean",
    "offset_date_time",
    "local_date_time",
    "local_date",
    "local_time",
    "array",
    "inline_table",
];

/// The TOML language: keys, tables and array elements as symbols, no link regions.
#[derive(Debug)]
pub(crate) struct Toml {
    /// The grammar.
    grammar: tree_sitter::Language,
}

/// The cartridge, built the first time it is used.
static TOML: LazyLock<Toml> = LazyLock::new(|| Toml {
    grammar: tree_sitter_toml_ng::LANGUAGE.into(),
});

/// The TOML cartridge.
#[must_use]
pub(crate) fn cartridge() -> &'static Toml {
    &TOML
}

impl Cartridge for Toml {
    fn id(&self) -> LanguageId {
        ID
    }

    fn extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn grammar(&self) -> &tree_sitter::Language {
        &self.grammar
    }

    fn reads_links(&self) -> bool {
        false
    }

    fn symbols(&self, file: &SourceFile) -> Result<SymbolTable, ReadError> {
        KeyPaths::build(file.key_paths(), file.deadline(), |paths| {
            Walk {
                text: file.text(),
                paths,
                arrays: HashMap::new(),
            }
            .document(file.tree().root_node())
        })
        .map_err(|error| error.read_error(file))
    }

    fn markdown_regions(&self, _file: &SourceFile) -> Result<Vec<MarkdownRegion>, ReadError> {
        Ok(Vec::new())
    }
}

/// What a container's children are.
#[derive(Debug, Clone, Copy)]
enum Children {
    /// A body of pairs: a table, an inline table or the document root.
    Pairs,
    /// An array's elements.
    Elements {
        /// The 0-based index the next element is named by.
        next: usize,
    },
}

/// A container the walk is going through, waiting on the walk's stack.
struct Frame<'tree> {
    /// The symbol its children go under (`None` for the document root's pairs).
    parent: Option<KeyId>,
    /// At the container until its first child is taken, then at the last child taken.
    cursor: TreeCursor<'tree>,
    /// Whether the cursor has gone down into the container's children.
    entered: bool,
    /// What its children are.
    children: Children,
}

impl<'tree> Frame<'tree> {
    /// A frame about to go through `container`'s children under `parent`.
    fn new(parent: Option<KeyId>, container: Node<'tree>, children: Children) -> Self {
        Self {
            parent,
            cursor: container.walk(),
            entered: false,
            children,
        }
    }

    /// The next child after the last one taken that names something here: a `pair` in a body of
    /// pairs, a value in an array. `None` once there are none. The cursor moves one sibling at a
    /// time, so nothing is allocated, and every child passed over is checked against the
    /// deadline.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn next(&mut self, paths: &mut KeyPaths) -> Result<Option<Node<'tree>>, KeyPathError> {
        loop {
            let moved = if self.entered {
                self.cursor.goto_next_sibling()
            } else {
                self.entered = true;
                self.cursor.goto_first_child()
            };
            if !moved {
                return Ok(None);
            }
            let child = self.cursor.node();
            paths.ensure_live(child.start_byte())?;
            let wanted = match self.children {
                Children::Pairs => child.kind() == "pair",
                Children::Elements { .. } => VALUE_KINDS.contains(&child.kind()),
            };
            if child.is_named() && wanted {
                return Ok(Some(child));
            }
        }
    }
}

/// One file's walk: its text, the builder, and the arrays of tables met so far.
struct Walk<'file, 'paths> {
    /// The file's text, which every node's byte range indexes.
    text: &'file str,
    /// The builder every symbol is made through.
    paths: &'paths mut KeyPaths,
    /// Each array of tables (`[[a]]`) by its symbol, with how many elements it has so far.
    arrays: HashMap<KeyId, usize>,
}

impl<'tree> Walk<'tree, '_> {
    /// Makes every symbol of the document rooted at `root`, in file order.
    ///
    /// # Errors
    ///
    /// The first [`KeyPathError`]: the deadline passed or the file's key paths are over budget.
    fn document(&mut self, root: Node<'tree>) -> Result<(), KeyPathError> {
        self.pairs(None, root)?;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            self.paths.ensure_live(child.start_byte())?;
            match child.kind() {
                "table" => self.table(child, false)?,
                "table_array_element" => self.table(child, true)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Makes the symbols of a `[a.b]` table (or, when `array`, a `[[a.b]]` element) and every pair
    /// in it. A header with a key that names nothing is skipped with its pairs.
    ///
    /// # Errors
    ///
    /// As [`Walk::document`].
    fn table(&mut self, node: Node<'tree>, array: bool) -> Result<(), KeyPathError> {
        let Some(key) = first_key(node) else {
            return Ok(());
        };
        let Some(names) = self.key_names(key)? else {
            return Ok(());
        };
        let Some((last, outer)) = names.split_last() else {
            return Ok(());
        };
        let span = node.byte_range();
        let mut parent = None;
        for name in outer {
            parent = Some(self.descend(parent, name.clone(), &span)?);
        }
        let table = self.paths.table(parent, last.clone(), span.clone())?;
        let id = if array {
            let len = self.arrays.entry(table).or_insert(0);
            let index = *len;
            *len = index.saturating_add(1);
            let Some(element) = index_name(index) else {
                return Ok(());
            };
            self.paths.table(Some(table), element, span)?
        } else {
            table
        };
        self.pairs(Some(id), node)
    }

    /// The table `name` under `parent`, widened to cover `span`; when it is an array of tables,
    /// its last element instead, widened the same way, since `[a.b]` after `[[a]]` belongs to
    /// that element.
    ///
    /// # Errors
    ///
    /// As [`Walk::document`].
    fn descend(
        &mut self,
        parent: Option<KeyId>,
        name: KeyName,
        span: &Range<usize>,
    ) -> Result<KeyId, KeyPathError> {
        let table = self.paths.table(parent, name, span.clone())?;
        let last = self
            .arrays
            .get(&table)
            .and_then(|len| len.checked_sub(1))
            .and_then(index_name);
        match last {
            Some(element) => self.paths.table(Some(table), element, span.clone()),
            None => Ok(table),
        }
    }

    /// Makes a symbol for every pair in `container` under `parent`, and for everything under
    /// their values: inline tables' pairs and arrays' elements, depth first in file order.
    ///
    /// The walk keeps its own stack, so any nesting depth is safe. A frame goes on it only for a
    /// value whose symbol was just made, so the stack never outgrows the budget, and children are
    /// taken one at a time rather than collected.
    ///
    /// # Errors
    ///
    /// As [`Walk::document`].
    fn pairs(&mut self, parent: Option<KeyId>, container: Node<'tree>) -> Result<(), KeyPathError> {
        let mut frames = vec![Frame::new(parent, container, Children::Pairs)];
        while let Some(frame) = frames.last_mut() {
            let Some(child) = frame.next(self.paths)? else {
                let _done = frames.pop();
                continue;
            };
            let made = match &mut frame.children {
                Children::Pairs => self.pair(frame.parent, child)?,
                Children::Elements { next } => {
                    let index = *next;
                    *next = index.saturating_add(1);
                    match index_name(index) {
                        Some(name) => Some((
                            self.paths.key(frame.parent, name, child.byte_range())?,
                            child,
                        )),
                        None => None,
                    }
                }
            };
            let Some((id, value)) = made else {
                continue;
            };
            match value.kind() {
                "inline_table" => frames.push(Frame::new(Some(id), value, Children::Pairs)),
                "array" => frames.push(Frame::new(Some(id), value, Children::Elements { next: 0 })),
                _ => {}
            }
        }
        Ok(())
    }

    /// Makes the symbols a pair's key names under `parent`: each part of a dotted key but the
    /// last as a table, the last as a new key covering the pair. Returns the key and the value to
    /// walk into, or `None` when a part names nothing or the pair has no value.
    ///
    /// # Errors
    ///
    /// As [`Walk::document`].
    fn pair(
        &mut self,
        parent: Option<KeyId>,
        node: Node<'tree>,
    ) -> Result<Option<(KeyId, Node<'tree>)>, KeyPathError> {
        let Some(key) = first_key(node) else {
            return Ok(None);
        };
        let Some(value) = value_after(node, key) else {
            return Ok(None);
        };
        let Some(names) = self.key_names(key)? else {
            return Ok(None);
        };
        let Some((last, outer)) = names.split_last() else {
            return Ok(None);
        };
        let span = node.byte_range();
        let mut parent = parent;
        for name in outer {
            parent = Some(self.paths.table(parent, name.clone(), span.clone())?);
        }
        let id = self.paths.key(parent, last.clone(), span)?;
        Ok(Some((id, value)))
    }

    /// The segments a bare, quoted or dotted key names, outermost first. `Ok(None)` when any
    /// part is empty or not a key, so nothing under it is named.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::reaches`]: each part is checked against the deadline and the segment
    /// budget before it is listed, so a key with more parts than a path can hold is refused
    /// before its list grows past the budget.
    fn key_names(&mut self, key: Node<'tree>) -> Result<Option<Vec<KeyName>>, KeyPathError> {
        // A dotted key nests to the left: `a.b.c` is `(a.b).c`. Unwind it without recursion.
        let mut parts = Vec::new();
        let mut node = key;
        loop {
            self.paths
                .reaches(node.start_byte(), parts.len().saturating_add(1))?;
            if node.kind() != "dotted_key" {
                break;
            }
            let Some((left, right)) = halves(node) else {
                return Ok(None);
            };
            parts.push(right);
            node = left;
        }
        parts.push(node);
        Ok(parts
            .iter()
            .rev()
            .map(|part| self.key_text(*part).as_deref().and_then(key_name))
            .collect())
    }

    /// The key a bare or quoted key node spells, decoded from its quoting.
    fn key_text(&self, node: Node<'tree>) -> Option<String> {
        let raw = self.text.get(node.byte_range())?;
        match node.kind() {
            "bare_key" => Some(raw.to_owned()),
            "quoted_key" => unquote(raw),
            _ => None,
        }
    }
}

/// The two keys of a dotted key, `(left, right)`; `None` unless it has exactly two.
fn halves(dotted: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let mut cursor = dotted.walk();
    let mut keys = dotted
        .named_children(&mut cursor)
        .filter(|child| is_key(*child));
    let halves = (keys.next()?, keys.next()?);
    keys.next().is_none().then_some(halves)
}

/// Whether `node` is a key: bare, quoted or dotted.
fn is_key(node: Node<'_>) -> bool {
    matches!(node.kind(), "bare_key" | "quoted_key" | "dotted_key")
}

/// The key of a pair or table header: its first named child that is a key.
fn first_key(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| is_key(*child))
}

/// The value of `pair`: the first value node after its key.
fn value_after<'tree>(pair: Node<'tree>, key: Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = pair.walk();
    pair.named_children(&mut cursor)
        .filter(|child| child.start_byte() >= key.end_byte())
        .find(|child| VALUE_KINDS.contains(&child.kind()))
}

/// The key a quoted key spells: a literal string as written between its `'`s, a basic string with
/// its escapes decoded. `None` for anything else, or an escape TOML doesn't allow.
fn unquote(raw: &str) -> Option<String> {
    if let Some(literal) = raw.strip_prefix('\'') {
        return literal.strip_suffix('\'').map(str::to_owned);
    }
    let basic = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut key = String::with_capacity(basic.len());
    let mut chars = basic.chars();
    while let Some(next) = chars.next() {
        if next != '\\' {
            key.push(next);
            continue;
        }
        let decoded = match chars.next()? {
            'b' => '\u{8}',
            't' => '\t',
            'n' => '\n',
            'f' => '\u{c}',
            'r' => '\r',
            '"' => '"',
            '\\' => '\\',
            'u' => scalar(&mut chars, 4)?,
            'U' => scalar(&mut chars, 8)?,
            _ => return None,
        };
        key.push(decoded);
    }
    Some(key)
}

/// The Unicode scalar the next `digits` hex digits of `chars` spell. `None` when they are fewer,
/// not hex, or a surrogate or out of range.
fn scalar(chars: &mut std::str::Chars<'_>, digits: usize) -> Option<char> {
    let hex: String = chars.take(digits).collect();
    if hex.len() != digits || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return None;
    }
    char::from_u32(u32::from_str_radix(&hex, 16).ok()?)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::path::Path;

    use super::{cartridge, unquote};
    use crate::cartridges::data::KeyPathBudget;
    use crate::limits::Limits;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, Deadline, Languages, SourceFile, Symbol, SymbolTable};
    use crate::read::error::ReadError;

    /// A Cargo manifest with every kind of key and table the cartridge names.
    const MANIFEST: &str = r#"[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = { version = "1", features = ["derive", "rc"] }
"a.b" = "2"
'lit~key' = "3"
site.url = "https://example.com"

[workspace]
members = ["crates/a"]

[profile.release]
lto = true

[workspace.dependencies]
toml = "1"

[[bin]]
name = "first"

[bin.metadata]
tag = "one"

[[bin]]
name = "second"
"#;

    fn parse(text: &str) -> SourceFile {
        parse_under(text, KeyPathBudget::defaults())
    }

    /// `text` parsed as `/repo/Cargo.toml`, its key paths to be built under `budget`.
    fn parse_under(text: &str, budget: KeyPathBudget) -> SourceFile {
        let path = AbsolutePath::new(Path::new("/repo/Cargo.toml"))
            .expect("the fixture path should be absolute");
        SourceFile::from_text_under(
            path,
            text.to_owned(),
            cartridge(),
            Deadline::start(Limits::PARSE_TIMEOUT),
            budget,
        )
        .expect("tree-sitter should return a tree")
    }

    fn symbols(file: &SourceFile) -> SymbolTable {
        file.language()
            .symbols(file)
            .expect("the fixture should read")
    }

    fn paths(table: &SymbolTable) -> Vec<String> {
        table
            .iter()
            .map(|symbol| symbol.path().to_string())
            .collect()
    }

    fn resolved<'t>(file: &SourceFile, table: &'t SymbolTable, raw: &str) -> &'t Symbol {
        table
            .resolve_text(file.path(), raw)
            .unwrap_or_else(|| panic!("`{raw}` should be a symbol path"))
            .unwrap_or_else(|error| panic!("`{raw}` should resolve: {error}"))
    }

    fn text_of<'f>(file: &'f SourceFile, symbol: &Symbol) -> &'f str {
        file.text()
            .get(symbol.range())
            .expect("a symbol's range should be valid for its file")
    }

    #[test]
    fn manifest_names_every_key_table_and_element() {
        let file = parse(MANIFEST);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "package",
                "package.name",
                "package.version",
                "dependencies",
                "dependencies.serde",
                "dependencies.serde.version",
                "dependencies.serde.features",
                "dependencies.serde.features.0",
                "dependencies.serde.features.1",
                "dependencies.a~1b",
                "dependencies.lit~0key",
                "dependencies.site",
                "dependencies.site.url",
                "workspace",
                "workspace.members",
                "workspace.members.0",
                "profile",
                "profile.release",
                "profile.release.lto",
                "workspace.dependencies",
                "workspace.dependencies.toml",
                "bin",
                "bin.0",
                "bin.0.name",
                "bin.0.metadata",
                "bin.0.metadata.tag",
                "bin.1",
                "bin.1.name",
            ],
        );
    }

    #[test]
    fn a_pair_covers_its_key_and_value() {
        let file = parse(MANIFEST);
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "package.version")),
            "version = \"0.1.0\""
        );
        assert_eq!(
            text_of(
                &file,
                resolved(&file, &table, "dependencies.serde.features.0")
            ),
            "\"derive\""
        );
    }

    #[test]
    fn a_table_split_around_another_covers_its_first_piece_through_its_last() {
        let file = parse(MANIFEST);
        let table = symbols(&file);

        let workspace = text_of(&file, resolved(&file, &table, "workspace"));

        assert!(
            workspace.starts_with("[workspace]\n"),
            "the span should start at the first piece: {workspace:?}"
        );
        assert!(
            workspace.contains("[profile.release]"),
            "the table between the pieces should be inside the span: {workspace:?}"
        );
        assert!(
            workspace.trim_end().ends_with("toml = \"1\""),
            "the span should end with the last piece: {workspace:?}"
        );
    }

    #[test]
    fn an_array_element_covers_its_header_and_the_sub_tables_after_it() {
        let file = parse(MANIFEST);
        let table = symbols(&file);

        let first = text_of(&file, resolved(&file, &table, "bin.0"));

        assert!(
            first.starts_with("[[bin]]\nname = \"first\""),
            "the element should start at its header: {first:?}"
        );
        assert!(
            first.trim_end().ends_with("tag = \"one\""),
            "the element should run through `[bin.metadata]`: {first:?}"
        );
        assert!(
            !first.contains("second"),
            "the next element is not part of this one: {first:?}"
        );
    }

    #[test]
    fn a_quoted_key_with_a_dot_is_one_escaped_segment() {
        let file = parse(MANIFEST);
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "dependencies.a~1b")),
            "\"a.b\" = \"2\""
        );
    }

    #[test]
    fn a_duplicate_key_is_ambiguous() {
        let file = parse("[package]\nversion = \"1\"\nversion = \"2\"\n");
        let table = symbols(&file);

        let error = table
            .resolve_text(file.path(), "package.version")
            .expect("the path should parse")
            .expect_err("two keys with the same path should not resolve to one");

        assert_eq!(error.code(), "resolve-ambiguous");
    }

    #[test]
    fn a_value_and_a_table_of_the_same_name_are_ambiguous() {
        let file = parse("a = 1\n\n[a]\nb = 2\n");
        let table = symbols(&file);

        let error = table
            .resolve_text(file.path(), "a")
            .expect("the path should parse")
            .expect_err("a key and a table with the same path should not resolve to one");

        assert_eq!(error.code(), "resolve-ambiguous");
    }

    #[test]
    fn an_empty_key_names_nothing_under_it() {
        let file = parse("\"\" = { a = 1 }\nb.\"\".c = 2\nd = 3\n");

        assert_eq!(paths(&symbols(&file)), ["d"]);
    }

    #[test]
    fn inline_tables_and_nested_arrays_name_every_level() {
        let file = parse("x = { y = [[1, 2], { z = 3 }], w.v = 4 }\n");

        assert_eq!(
            paths(&symbols(&file)),
            [
                "x", "x.y", "x.y.0", "x.y.0.0", "x.y.0.1", "x.y.1", "x.y.1.z", "x.w", "x.w.v",
            ],
        );
    }

    #[test]
    fn quoted_keys_decode_their_escapes() {
        assert_eq!(
            unquote(r#""a\tb\u00e9\U0001F600""#).as_deref(),
            Some("a\tbé😀")
        );
        assert_eq!(unquote(r"'C:\path'").as_deref(), Some(r"C:\path"));
        assert_eq!(unquote(r#""\uD800""#), None, "a surrogate is not a scalar");
        assert_eq!(unquote(r#""\q""#), None, "TOML has no `\\q` escape");
    }

    #[test]
    fn toml_is_a_link_target_only() {
        let file = parse(MANIFEST);

        assert!(!cartridge().reads_links());
        assert_eq!(
            cartridge()
                .markdown_regions(&file)
                .expect("the regions should read")
                .len(),
            0
        );
    }

    #[test]
    fn the_registry_reads_toml_files_with_this_cartridge() {
        let language = Languages::builtin()
            .expect("the built-in cartridges should load")
            .for_path(Path::new("Cargo.toml"))
            .expect("the registry should claim `.toml`");

        assert_eq!(language.id().as_str(), "toml");
        assert!(!language.reads_links());
    }

    #[test]
    fn deep_nesting_fails_closed_instead_of_exhausting_memory() {
        let depth = 100_000;
        let header = format!("[{}]\n", vec!["a"; depth].join("."));
        let file = parse(&header);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("100,000 nested tables should exceed the default segment budget");

        assert!(
            matches!(error, ReadError::TooManyKeys { limit: 200_000, .. }),
            "expected read-too-many-keys at the default limit, got {error:?}"
        );
    }

    #[test]
    fn deep_inline_tables_fail_closed_without_recursion() {
        let depth = 100_000;
        let nested = format!("{}1{}\n", "a = { ".repeat(depth), " }".repeat(depth));
        let file = parse(&nested);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("100,000 nested inline tables should not build");

        assert!(
            matches!(
                error,
                ReadError::TooManyKeys { .. } | ReadError::Timeout { .. }
            ),
            "expected a budget or deadline failure, got {error:?}"
        );
    }

    #[test]
    fn sibling_keys_past_the_default_limit_fail_closed() {
        // One key past the 200,000-segment default is enough to show no partial table survives.
        let wide = (0..=200_000).fold(String::new(), |mut text, key| {
            let _ = writeln!(text, "k{key} = 1");
            text
        });
        let file = parse(&wide);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("200,001 keys should exceed the default segment budget");

        assert_eq!(error.code(), "read-too-many-keys");
    }

    #[test]
    fn the_file_s_budget_bounds_its_key_paths() {
        let file = parse_under(
            MANIFEST,
            KeyPathBudget {
                segments: 3,
                bytes: usize::MAX,
            },
        );

        let error = file
            .language()
            .symbols(&file)
            .expect_err("the manifest holds more than three segments");

        assert!(
            matches!(error, ReadError::TooManyKeys { limit: 3, .. }),
            "expected read-too-many-keys at 3, got {error:?}"
        );
    }

    #[test]
    fn a_syntax_error_still_names_the_keys_around_it() {
        let file = parse("[package]\nname = \"demo\"\nbroken = = =\nversion = \"1\"\n");

        assert!(file.has_syntax_errors(), "the fixture is invalid TOML");
        let found = paths(&symbols(&file));
        assert!(
            found.contains(&"package.name".to_owned()),
            "keys before the error should still be symbols: {found:?}"
        );
    }

    #[test]
    fn invalid_utf8_is_refused_before_the_cartridge_reads_it() {
        let dir = tempfile::tempdir().expect("a temp dir should be creatable");
        let path = AbsolutePath::new(&dir.path().join("Cargo.toml"))
            .expect("a temp dir path should be absolute");
        fs::write(path.as_path(), b"a = \"\xff\"\n").expect("the fixture should be writable");

        let error = SourceFile::read(
            path,
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            KeyPathBudget::defaults(),
        )
        .expect_err("a file with an invalid UTF-8 byte should be rejected");

        assert_eq!(error.code(), "read-not-utf8");
    }
}
