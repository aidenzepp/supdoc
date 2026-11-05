//! YAML: a link target whose symbols are its keys and sequence elements.
//!
//! Paths follow the key-path model the data cartridges share ([`crate::cartridges::data`]):
//! `.github/workflows/ci.yml#jobs.test.steps.1`, and `deploy.yaml#1.spec.replicas` for the second
//! document of a stream. A plain key is one segment as written, a quoted key is decoded first, so
//! `"a.b": 1` is `a~1b`, and a key that is not a scalar (`? [a, b]`, an alias) names nothing, nor
//! does anything under it. Block and flow collections are named the same way.
//!
//! Symbols are the file as written, not its loaded value. An anchor (`&a`) and a tag (`!!str`)
//! are part of their node's text and name nothing. An alias (`*a`) is never expanded: the key
//! holding it is a leaf, so an alias bomb costs one symbol per alias written. A merge key `<<` is
//! an ordinary key, and the keys it merges in are not symbols of the merging mapping. A stream of
//! one document is named from its root; a stream of several names each document by its 0-based
//! index, and every path inside starts with it.
//!
//! A key's symbol covers its pair: the key through the end of its value, a nested block included.
//! An element covers its `- ` item, or its flow item. A document of a several-document stream
//! covers its `document` node.
//!
//! YAML files are link targets only: the cartridge returns no Markdown regions and the walk never
//! reads them as sources. The walk keeps its own stack and charges every symbol to the file's
//! key-path budget and deadline, so hostile nesting fails closed (SEC-1). It steps through every
//! node's children with a cursor rather than collecting them, checking the deadline at each child,
//! and holds a frame only for a collection whose own symbol it already charged, so memory beyond
//! the parse grows with the budget, not with the file. A YAML key is always one segment, so there
//! is no multi-part key to refuse before it is listed, as TOML's dotted keys need.

use std::ops::Range;
use std::sync::LazyLock;

use tree_sitter::{Node, TreeCursor};

use crate::cartridges::data::{KeyId, KeyPathError, KeyPaths, index_name, key_name};
use crate::read::cartridge::{Cartridge, LanguageId, MarkdownRegion, SourceFile, SymbolTable};
use crate::read::error::ReadError;

/// The id, in errors and `info` output.
pub(crate) const ID: LanguageId = LanguageId::new("yaml");

/// The extensions it reads, lowercase without the dot.
pub(crate) const EXTENSIONS: &[&str] = &["yaml", "yml"];

/// The node kinds whose text can end in a comment that is not part of what the node says.
const BLOCKS: &[&str] = &[
    "document",
    "block_node",
    "block_mapping",
    "block_mapping_pair",
    "block_sequence",
    "block_sequence_item",
];

/// The YAML language: keys and sequence elements as symbols, no link regions.
#[derive(Debug)]
pub(crate) struct Yaml {
    /// The grammar.
    grammar: tree_sitter::Language,
}

/// The cartridge, built the first time it is used.
static YAML: LazyLock<Yaml> = LazyLock::new(|| Yaml {
    grammar: tree_sitter_yaml::LANGUAGE.into(),
});

/// The YAML cartridge.
#[must_use]
pub(crate) fn cartridge() -> &'static Yaml {
    &YAML
}

impl Cartridge for Yaml {
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
            }
            .stream(file.tree().root_node())
        })
        .map_err(|error| error.read_error(file))
    }

    fn markdown_regions(&self, _file: &SourceFile) -> Result<Vec<MarkdownRegion>, ReadError> {
        Ok(Vec::new())
    }
}

/// What a collection's children are.
#[derive(Debug, Clone, Copy)]
enum Children {
    /// A mapping's pairs, block or flow.
    Pairs,
    /// A sequence's elements, block or flow.
    Elements {
        /// The 0-based index the next element is named by.
        next: usize,
    },
}

impl Children {
    /// What `collection`'s children are; `None` when it is not a collection, so nothing is
    /// named under it.
    fn of(collection: Node<'_>) -> Option<Self> {
        match collection.kind() {
            "block_mapping" | "flow_mapping" => Some(Self::Pairs),
            "block_sequence" | "flow_sequence" => Some(Self::Elements { next: 0 }),
            _ => None,
        }
    }

    /// Whether `child` names something here. A flow mapping's lone `flow_node` is a key with no
    /// value (`{a, b}`); a flow sequence's `flow_pair` is a one-pair mapping (`[a: 1]`).
    fn names(self, child: Node<'_>) -> bool {
        let kinds: &[&str] = match self {
            Self::Pairs => &["block_mapping_pair", "flow_pair", "flow_node"],
            Self::Elements { .. } => &["block_sequence_item", "flow_node", "flow_pair"],
        };
        kinds.contains(&child.kind())
    }
}

/// A collection the walk is going through, waiting on the walk's stack.
struct Frame<'tree> {
    /// The symbol its children go under (`None` at the root of a one-document stream).
    parent: Option<KeyId>,
    /// At the collection until its first child is taken, then at the last child taken.
    cursor: TreeCursor<'tree>,
    /// Whether the cursor has gone down into the collection's children.
    entered: bool,
    /// What its children are.
    children: Children,
}

impl<'tree> Frame<'tree> {
    /// A frame about to go through `collection`'s children under `parent`.
    fn new(parent: Option<KeyId>, collection: Node<'tree>, children: Children) -> Self {
        Self {
            parent,
            cursor: collection.walk(),
            entered: false,
            children,
        }
    }

    /// The next child after the last one taken that names something here; `None` once there are
    /// none. The cursor moves one sibling at a time, so nothing is allocated, and every child
    /// passed over (a comment, an error) is checked against the deadline.
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
            if self.children.names(child) {
                return Ok(Some(child));
            }
        }
    }
}

/// One file's walk: its text and the builder.
struct Walk<'file, 'paths> {
    /// The file's text, which every node's byte range indexes.
    text: &'file str,
    /// The builder every symbol is made through.
    paths: &'paths mut KeyPaths,
}

impl<'tree> Walk<'tree, '_> {
    /// Makes every symbol of the stream rooted at `root`, in file order: each document's from its
    /// root when there is one document, under the document's index when there are several.
    ///
    /// # Errors
    ///
    /// The first [`KeyPathError`]: the deadline passed or the file's key paths are over budget.
    fn stream(&mut self, root: Node<'tree>) -> Result<(), KeyPathError> {
        let several = self.documents(root)? > 1;
        let mut cursor = root.walk();
        let mut index = 0_usize;
        for document in root.children(&mut cursor) {
            self.paths.ensure_live(document.start_byte())?;
            if document.kind() != "document" {
                continue;
            }
            let parent = if several {
                let Some(name) = index_name(index) else {
                    continue;
                };
                index = index.saturating_add(1);
                let span = self.span(document)?;
                Some(self.paths.key(None, name, span)?)
            } else {
                None
            };
            if let Some(content) = self.value(document)? {
                self.collection(parent, content)?;
            }
        }
        Ok(())
    }

    /// How many documents the stream rooted at `root` holds.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn documents(&mut self, root: Node<'tree>) -> Result<usize, KeyPathError> {
        let mut cursor = root.walk();
        let mut documents = 0_usize;
        for child in root.children(&mut cursor) {
            self.paths.ensure_live(child.start_byte())?;
            if child.kind() == "document" {
                documents = documents.saturating_add(1);
            }
        }
        Ok(documents)
    }

    /// Makes a symbol for every pair and element in `collection` under `parent`, and for
    /// everything under their values, depth first in file order. Anything but a collection names
    /// nothing.
    ///
    /// The walk keeps its own stack, so any nesting depth is safe. A frame goes on it only for a
    /// value whose symbol was just made, so the stack never outgrows the budget, and children are
    /// taken one at a time rather than collected.
    ///
    /// # Errors
    ///
    /// As [`Walk::stream`].
    fn collection(
        &mut self,
        parent: Option<KeyId>,
        collection: Node<'tree>,
    ) -> Result<(), KeyPathError> {
        let Some(children) = Children::of(collection) else {
            return Ok(());
        };
        let mut frames = vec![Frame::new(parent, collection, children)];
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
                    self.element(frame.parent, index, child)?
                }
            };
            let Some(value) = made else {
                continue;
            };
            let (id, collection) = value;
            if let Some(children) = Children::of(collection) {
                frames.push(Frame::new(Some(id), collection, children));
            }
        }
        Ok(())
    }

    /// Makes the symbol a pair's key names under `parent`, covering the pair. A flow mapping's
    /// lone `flow_node` is a key with no value. Returns the key and the value to walk into, or
    /// `None` when the key names nothing or there is no value.
    ///
    /// # Errors
    ///
    /// As [`Walk::stream`].
    fn pair(
        &mut self,
        parent: Option<KeyId>,
        pair: Node<'tree>,
    ) -> Result<Option<(KeyId, Node<'tree>)>, KeyPathError> {
        let (key, value) = if pair.kind() == "flow_node" {
            (Some(pair), None)
        } else {
            (self.field(pair, "key")?, self.field(pair, "value")?)
        };
        let Some(key) = key else {
            return Ok(None);
        };
        let Some(text) = self.key_text(key)? else {
            return Ok(None);
        };
        let Some(name) = key_name(&text) else {
            return Ok(None);
        };
        let span = self.span(pair)?;
        let id = self.paths.key(parent, name, span)?;
        let Some(value) = value else {
            return Ok(None);
        };
        Ok(self.content(value)?.map(|content| (id, content)))
    }

    /// Makes the symbol of the element at `index` under `parent`, covering `item`. Returns the
    /// element and the value to walk into, or `None` when there is none. A flow sequence's
    /// `flow_pair` is a one-pair mapping, so its key is made under the element at once.
    ///
    /// # Errors
    ///
    /// As [`Walk::stream`].
    fn element(
        &mut self,
        parent: Option<KeyId>,
        index: usize,
        item: Node<'tree>,
    ) -> Result<Option<(KeyId, Node<'tree>)>, KeyPathError> {
        let Some(name) = index_name(index) else {
            return Ok(None);
        };
        let span = self.span(item)?;
        let id = self.paths.key(parent, name, span)?;
        match item.kind() {
            "flow_pair" => self.pair(Some(id), item),
            "flow_node" => Ok(self.content(item)?.map(|content| (id, content))),
            _ => Ok(self.value(item)?.map(|content| (id, content))),
        }
    }

    /// The bytes a symbol covering `node` hashes: all of it but a trailing comment. A nested block
    /// keeps the comments after its last line inside its node, so the end is where the last thing
    /// that is not a comment ends, found down the chain of last children. Only block structure
    /// can end in a comment (a flow collection ends at its bracket, a scalar at its text), so the
    /// chain stops at the first node that is not a block, and its length is bounded by how far
    /// the file indents.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn span(&mut self, node: Node<'tree>) -> Result<Range<usize>, KeyPathError> {
        let mut last = node;
        loop {
            self.paths.ensure_live(last.start_byte())?;
            if !BLOCKS.contains(&last.kind()) {
                break;
            }
            let mut cursor = last.walk();
            if !cursor.goto_last_child() {
                break;
            }
            let child = cursor.node();
            last = if child.is_extra() {
                // Stepping back one sibling at a time costs a pass over the earlier siblings
                // each, so find the last real child in one forward pass instead.
                match self.last_real_child(last)? {
                    Some(real) => real,
                    None => break,
                }
            } else {
                child
            };
        }
        Ok(node.start_byte()..last.end_byte())
    }

    /// The last child of `parent` that is not a comment, in one forward pass checked against the
    /// deadline at every child.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn last_real_child(
        &mut self,
        parent: Node<'tree>,
    ) -> Result<Option<Node<'tree>>, KeyPathError> {
        let mut cursor = parent.walk();
        let mut real = None;
        let mut moved = cursor.goto_first_child();
        while moved {
            let child = cursor.node();
            self.paths.ensure_live(child.start_byte())?;
            if !child.is_extra() {
                real = Some(child);
            }
            moved = cursor.goto_next_sibling();
        }
        Ok(real)
    }

    /// The content of the first block or flow node among `parent`'s children: a document's root,
    /// or a `- ` item's value.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn value(&mut self, parent: Node<'tree>) -> Result<Option<Node<'tree>>, KeyPathError> {
        let node = self.child(parent, |child, _| {
            matches!(child.kind(), "block_node" | "flow_node")
        })?;
        match node {
            Some(node) => self.content(node),
            None => Ok(None),
        }
    }

    /// What a block or flow node holds, past its anchor and tag: a collection, a scalar or an
    /// alias. `None` for an empty node.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn content(&mut self, node: Node<'tree>) -> Result<Option<Node<'tree>>, KeyPathError> {
        self.child(node, |child, _| {
            child.is_named() && !matches!(child.kind(), "anchor" | "tag" | "comment" | "ERROR")
        })
    }

    /// The child of `parent` in the field `name`.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn field(
        &mut self,
        parent: Node<'tree>,
        name: &str,
    ) -> Result<Option<Node<'tree>>, KeyPathError> {
        self.child(parent, |_, field| field == Some(name))
    }

    /// The first child of `parent` that `wanted` accepts, given the child and its field name.
    /// Every child passed over is checked against the deadline, since any number of comments can
    /// sit between a node's parts.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn child(
        &mut self,
        parent: Node<'tree>,
        wanted: impl Fn(Node<'tree>, Option<&str>) -> bool,
    ) -> Result<Option<Node<'tree>>, KeyPathError> {
        let mut cursor = parent.walk();
        let mut moved = cursor.goto_first_child();
        while moved {
            let child = cursor.node();
            self.paths.ensure_live(child.start_byte())?;
            if wanted(child, cursor.field_name()) {
                return Ok(Some(child));
            }
            moved = cursor.goto_next_sibling();
        }
        Ok(None)
    }

    /// The key a key node spells: a plain scalar as written, a quoted scalar decoded. `None`
    /// for a key that is not a scalar (a collection, an alias, a block scalar), which names
    /// nothing.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn key_text(&mut self, key: Node<'tree>) -> Result<Option<String>, KeyPathError> {
        let Some(scalar) = self.content(key)? else {
            return Ok(None);
        };
        let Some(raw) = self.text.get(scalar.byte_range()) else {
            return Ok(None);
        };
        Ok(match scalar.kind() {
            "plain_scalar" => Some(raw.to_owned()),
            "single_quote_scalar" => single_quoted(raw),
            "double_quote_scalar" => double_quoted(raw),
            _ => None,
        })
    }
}

/// The key a single-quoted scalar spells: the text between its `'`s, each `''` one `'`. `None`
/// for a key over several lines, whose folding names nothing.
fn single_quoted(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('\'')?.strip_suffix('\'')?;
    (!inner.contains('\n')).then(|| inner.replace("''", "'"))
}

/// The key a double-quoted scalar spells, its escapes decoded. `None` for an escape YAML doesn't
/// allow, or a key over several lines, whose folding names nothing.
fn double_quoted(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('\n') {
        return None;
    }
    let mut key = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(next) = chars.next() {
        if next != '\\' {
            key.push(next);
            continue;
        }
        let decoded = match chars.next()? {
            '0' => '\0',
            'a' => '\u{7}',
            'b' => '\u{8}',
            't' | '\t' => '\t',
            'n' => '\n',
            'v' => '\u{b}',
            'f' => '\u{c}',
            'r' => '\r',
            'e' => '\u{1b}',
            ' ' => ' ',
            '"' => '"',
            '/' => '/',
            '\\' => '\\',
            'N' => '\u{85}',
            '_' => '\u{a0}',
            'L' => '\u{2028}',
            'P' => '\u{2029}',
            'x' => scalar(&mut chars, 2)?,
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
    use std::path::Path;

    use super::{cartridge, double_quoted, single_quoted};
    use crate::cartridges::data::KeyPathBudget;
    use crate::limits::{Limits, TimeLimit};
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, Deadline, Languages, SourceFile, Symbol, SymbolTable};
    use crate::read::error::ReadError;

    /// A GitHub workflow: block mappings and sequences, a flow sequence, quoted keys and a tag.
    const WORKFLOW: &str = r#"name: ci
on:
  push:
    branches: [main, "release/*"]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Test
        run: cargo test # the suite
        env: {RUST_LOG: debug, "a.b": "1"}
      - !!map {'it''s': x}
"#;

    /// Two Kubernetes objects in one stream.
    const MANIFESTS: &str = "apiVersion: v1
kind: Service
metadata:
  name: web
---
apiVersion: apps/v1
kind: Deployment
metadata:
  labels:
    app.kubernetes.io/name: web
spec:
  replicas: 3
";

    fn parse(text: &str) -> SourceFile {
        parse_under(text, KeyPathBudget::defaults())
    }

    /// `text` parsed as `/repo/ci.yml`, its key paths to be built under `budget`.
    fn parse_under(text: &str, budget: KeyPathBudget) -> SourceFile {
        let path = AbsolutePath::new(Path::new("/repo/ci.yml"))
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
    fn a_workflow_names_every_key_and_step() {
        let file = parse(WORKFLOW);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "name",
                "on",
                "on.push",
                "on.push.branches",
                "on.push.branches.0",
                "on.push.branches.1",
                "jobs",
                "jobs.test",
                "jobs.test.runs-on",
                "jobs.test.steps",
                "jobs.test.steps.0",
                "jobs.test.steps.0.uses",
                "jobs.test.steps.1",
                "jobs.test.steps.1.name",
                "jobs.test.steps.1.run",
                "jobs.test.steps.1.env",
                "jobs.test.steps.1.env.RUST_LOG",
                "jobs.test.steps.1.env.a~1b",
                "jobs.test.steps.2",
                "jobs.test.steps.2.it's",
            ],
        );
    }

    #[test]
    fn a_step_covers_its_item_and_a_key_its_pair_without_a_trailing_comment() {
        let file = parse(WORKFLOW);
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "jobs.test.steps.1")),
            "- name: Test\n        run: cargo test # the suite\n        env: {RUST_LOG: debug, \"a.b\": \"1\"}"
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "jobs.test.steps.1.run")),
            "run: cargo test"
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "on.push.branches.1")),
            "\"release/*\""
        );
    }

    #[test]
    fn a_comment_after_a_nested_block_is_outside_every_span_that_ends_there() {
        let file = parse("a:\n  b:\n    c: 1\n    # trailing\nd: 2\n");
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "a")),
            "a:\n  b:\n    c: 1"
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "a.b")),
            "b:\n    c: 1"
        );
    }

    #[test]
    fn several_documents_are_named_by_their_index() {
        let file = parse(MANIFESTS);
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "1.spec.replicas")),
            "replicas: 3"
        );
        assert_eq!(
            text_of(
                &file,
                resolved(&file, &table, "1.metadata.labels.app~1kubernetes~1io/name")
            ),
            "app.kubernetes.io/name: web"
        );
        assert!(
            text_of(&file, resolved(&file, &table, "0")).ends_with("name: web"),
            "the first document should be a symbol of its own"
        );
    }

    #[test]
    fn one_document_has_no_index_even_after_a_leading_marker() {
        let file = parse("---\nspec:\n  replicas: 3\n");

        assert_eq!(paths(&symbols(&file)), ["spec", "spec.replicas"]);
    }

    #[test]
    fn a_trailing_marker_starts_an_empty_second_document() {
        // YAML reads `---` with nothing after it as an empty document, so the stream holds two
        // and every path takes its document's index, as `yaml.load_all` sees it.
        let file = parse("a: 1\n---\n");

        assert_eq!(paths(&symbols(&file)), ["0", "0.a", "1"]);
    }

    #[test]
    fn anchors_tags_aliases_and_merge_keys_name_what_is_written() {
        let file = parse(
            "base: &defaults\n  timeout: 5\nprod:\n  <<: *defaults\n  replicas: 2\nalias: *defaults\n!!str tagged: !!int 1\n",
        );
        let table = symbols(&file);

        assert_eq!(
            paths(&table),
            [
                "base",
                "base.timeout",
                "prod",
                "prod.<<",
                "prod.replicas",
                "alias",
                "tagged",
            ],
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "alias")),
            "alias: *defaults"
        );
    }

    #[test]
    fn flow_collections_name_lone_keys_and_one_pair_elements() {
        let file = parse("x: [a: 1, b]\ny: {c, d: [2]}\n");

        assert_eq!(
            paths(&symbols(&file)),
            ["x", "x.0", "x.0.a", "x.1", "y", "y.c", "y.d", "y.d.0"],
        );
    }

    #[test]
    fn keys_escape_dots_and_tildes_and_scalars_keep_their_text() {
        let file = parse("\"a.b\": 1\n~: 2\n200: 3\n1.5: 4\n'q': 5\n");

        assert_eq!(paths(&symbols(&file)), ["a~1b", "~0", "200", "1~15", "q"]);
    }

    #[test]
    fn an_empty_or_non_scalar_key_names_nothing_under_it() {
        let file = parse("\"\": {a: 1}\n? [k, v]\n: {b: 2}\n*ref : {c: 3}\nd: 4\n");

        assert_eq!(paths(&symbols(&file)), ["d"]);
    }

    #[test]
    fn a_duplicate_key_is_ambiguous() {
        let file = parse("a: 1\na: 2\n");
        let table = symbols(&file);

        let error = table
            .resolve_text(file.path(), "a")
            .expect("the path should parse")
            .expect_err("two keys with the same path should not resolve to one");

        assert_eq!(error.code(), "resolve-ambiguous");
    }

    #[test]
    fn an_alias_bomb_costs_one_symbol_per_alias_written() {
        let mut bomb = String::from("a: &a [lol, lol, lol, lol, lol, lol, lol, lol, lol]\n");
        for level in 1..9_u8 {
            let previous = char::from(b'a' + level - 1);
            let name = char::from(b'a' + level);
            let aliases = vec![format!("*{previous}"); 9].join(", ");
            let _ = writeln!(bomb, "{name}: &{name} [{aliases}]");
        }
        let file = parse(&bomb);

        // Nine keys, each with nine elements: expanded, the last alone would be 9^9 elements.
        assert_eq!(symbols(&file).iter().count(), 9 * 10);
    }

    #[test]
    fn deep_flow_nesting_fails_closed_without_recursion() {
        let depth = 100_000;
        let nested = format!("{}1{}\n", "[".repeat(depth), "]".repeat(depth));
        let file = parse(&nested);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("100,000 nested flow sequences should not build");

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
        // A flow mapping on one line: the grammar's scanner counts rows in 16 bits, so a block
        // mapping of 200,001 lines would not parse past line 32,768.
        let keys = (0..=200_000).fold(String::new(), |mut text, key| {
            let _ = write!(text, "k{key}: 1, ");
            text
        });
        let file = parse(&format!("{{{keys}}}\n"));

        let error = file
            .language()
            .symbols(&file)
            .expect_err("200,001 keys should exceed the default segment budget");

        assert_eq!(error.code(), "read-too-many-keys");
    }

    #[test]
    fn the_file_s_budget_bounds_its_key_paths() {
        let file = parse_under(
            WORKFLOW,
            KeyPathBudget {
                segments: 3,
                bytes: usize::MAX,
            },
        );

        let error = file
            .language()
            .symbols(&file)
            .expect_err("the workflow holds more than three segments");

        assert!(
            matches!(error, ReadError::TooManyKeys { limit: 3, .. }),
            "expected read-too-many-keys at 3, got {error:?}"
        );
    }

    #[test]
    fn the_deadline_stops_the_walk_inside_a_run_of_children_that_name_nothing() {
        // The comments name nothing, so only the per-child check can stop the walk among them;
        // without it the walk would run on to `last` and stop there.
        let comments = "  # nothing\n".repeat(500_000);
        let text = format!("a: [first,\n{comments}  last]\n");
        let last = text
            .rfind("last")
            .expect("the fixture should end with `last`");
        let limit: TimeLimit = "1ms".parse().expect("`1ms` should be a time limit");
        let file = parse(&text).with_deadline(Deadline::start(limit));

        let error = file
            .language()
            .symbols(&file)
            .expect_err("half a million comments should outlast a 1 ms deadline");

        assert!(
            matches!(error, ReadError::Timeout { offset, .. } if offset < last),
            "the walk should stop among the comments, before byte {last}: {error:?}"
        );
    }

    #[test]
    fn a_syntax_error_still_names_the_keys_around_it() {
        let file = parse("a: 1\nb: [unclosed\nc: 2\n");

        assert!(file.has_syntax_errors(), "the fixture is invalid YAML");
        let found = paths(&symbols(&file));
        assert!(
            found.contains(&"a".to_owned()),
            "keys before the error should still be symbols: {found:?}"
        );
    }

    #[test]
    fn quoted_keys_decode_their_escapes() {
        assert_eq!(
            double_quoted(r#""a\tb\x41\u00e9\U0001F600\N""#).as_deref(),
            Some("a\tbAé😀\u{85}")
        );
        assert_eq!(double_quoted(r#""\q""#), None, "YAML has no `\\q` escape");
        assert_eq!(
            double_quoted(r#""\uD800""#),
            None,
            "a surrogate is not a scalar"
        );
        assert_eq!(single_quoted("'it''s'").as_deref(), Some("it's"));
    }

    #[test]
    fn yaml_is_a_link_target_only_read_for_both_extensions() {
        let file = parse(WORKFLOW);
        let languages = Languages::builtin().expect("the built-in cartridges should load");

        assert!(!cartridge().reads_links());
        assert_eq!(
            cartridge()
                .markdown_regions(&file)
                .expect("the regions should read")
                .len(),
            0
        );
        for name in ["ci.yml", "compose.yaml"] {
            let language = languages
                .for_path(Path::new(name))
                .unwrap_or_else(|| panic!("the registry should claim `{name}`"));
            assert_eq!(language.id().as_str(), "yaml");
        }
    }
}
