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
//! The grammar's scanner counts rows in a 16-bit integer, so text on row 32,768 or later parses
//! to an error tree that names the wrong keys, or none. A file past that is refused before the
//! parse with `read-too-many-lines`, counting line breaks as the scanner does: every `\r` and
//! every `\n`, so a `\r\n` file is refused after 16,384 lines. No setting raises the limit,
//! since the parser can't count further.
//!
//! The same scanner writes its stack of indentations into a fixed buffer that a file nested a
//! little past 250 levels overflows, aborting the process. So a file is also refused before the
//! parse with `read-too-deep` when any line's structure, its leading blanks plus a run of compact
//! `- `, `? ` or `: ` indicators, plus the longest such run later in the line (the scanner nests
//! on one after a quoted scalar, an alias, a flow collection or a second key), reaches past
//! column 100. Every line counts, the content lines of a block scalar included, and so does a
//! long run of indicators inside a quoted value. That can refuse a file the scanner would have
//! read: the safe direction for a guard on memory safety.
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
use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, LanguageId, MarkdownRegion, SourceFile, SymbolTable};
use crate::read::error::ReadError;

/// The id, in errors and `info` output.
pub(crate) const ID: LanguageId = LanguageId::new("yaml");

/// The extensions it reads, lowercase without the dot.
pub(crate) const EXTENSIONS: &[&str] = &["yaml", "yml"];

/// The most lines the grammar parses faithfully: its scanner keeps the row in an `int16_t`, so
/// row 32,768 (0-based) wraps.
const MAX_LINES: usize = 32_768;

/// The furthest column a line's structure may reach: its leading spaces and tabs plus any run of
/// compact `-`, `?` or `:` indicators, each followed by a blank or the line's end.
///
/// The grammar's scanner keeps a stack of indentations and writes it, 4 bytes an entry after a
/// 10-byte header, into tree-sitter's 1024-byte serialization buffer. Past 253 entries the write
/// runs out of bounds and the parse aborts the process. A mapping and a sequence can share one
/// column (`k:` with `-` under it at the same indent), so the stack holds up to two entries per
/// column: at column 100 at most about 200, with room to spare; at 127 it overflows.
const MAX_STRUCTURE_COLUMN: usize = 100;

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

    fn admit(&self, path: &AbsolutePath, text: &str) -> Result<(), ReadError> {
        if let Some((line, column)) = too_deep(text) {
            return Err(ReadError::TooDeep {
                path: path.clone(),
                line: u64::try_from(line).unwrap_or(u64::MAX),
                column: u64::try_from(column).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_STRUCTURE_COLUMN).unwrap_or(u64::MAX),
            });
        }
        let lines = lines(text);
        if lines <= MAX_LINES {
            return Ok(());
        }
        Err(ReadError::TooManyLines {
            path: path.clone(),
            lines: u64::try_from(lines).unwrap_or(u64::MAX),
            limit: u64::try_from(MAX_LINES).unwrap_or(u64::MAX),
        })
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
    /// Makes every symbol of the stream rooted at `root`, in file order: from its root when at
    /// most one document has content, else under each document's index. An empty document (only
    /// comments or directives, such as after a trailing `---`) doesn't make a stream several,
    /// but it still takes an index, so indices match YAML tools.
    ///
    /// # Errors
    ///
    /// The first [`KeyPathError`]: the deadline passed or the file's key paths are over budget.
    fn stream(&mut self, root: Node<'tree>) -> Result<(), KeyPathError> {
        let several = self.documents_with_content(root)? > 1;
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

    /// How many documents of the stream rooted at `root` hold a node, not only comments or
    /// directives.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn documents_with_content(&mut self, root: Node<'tree>) -> Result<usize, KeyPathError> {
        let mut cursor = root.walk();
        let mut documents = 0_usize;
        for child in root.children(&mut cursor) {
            self.paths.ensure_live(child.start_byte())?;
            if child.kind() == "document" && self.value(child)?.is_some() {
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
        let end = if last.kind() == "block_scalar" && self.keeps_trailing_lines(last) {
            self.blank_lines_after(last.end_byte())?
        } else {
            last.end_byte()
        };
        Ok(node.start_byte()..end)
    }

    /// Whether `scalar`'s header asks for keep chomping (`|+`, `>2+`): its trailing blank lines
    /// are part of its value, though the grammar ends the node at its last line of text.
    fn keeps_trailing_lines(&self, scalar: Node<'tree>) -> bool {
        self.text
            .get(scalar.byte_range())
            .and_then(|text| text.split([' ', '\t', '\r', '\n', '#']).next())
            .is_some_and(|header| header.contains('+'))
    }

    /// Where the run of blank lines after `offset` ends: past the rest of `offset`'s line and
    /// every line after it holding only spaces and tabs, so at the start of the next line with
    /// text, or the end of the file. Each line is checked against the deadline.
    ///
    /// # Errors
    ///
    /// As [`KeyPaths::ensure_live`].
    fn blank_lines_after(&mut self, offset: usize) -> Result<usize, KeyPathError> {
        let bytes = self.text.as_bytes();
        let mut end = offset;
        let mut at = offset;
        while let Some(&byte) = bytes.get(at) {
            match byte {
                b' ' | b'\t' => at = at.saturating_add(1),
                b'\r' | b'\n' => {
                    self.paths.ensure_live(at)?;
                    at = at.saturating_add(1);
                    end = at;
                }
                _ => return Ok(end),
            }
        }
        Ok(bytes.len())
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

/// The lines of `text` as the grammar's scanner counts rows: every `\r` byte and every `\n` byte
/// ends one, so `\r\n` ends two, and text after the last break is one more. A file has more than
/// [`MAX_LINES`] exactly when some byte follows its 32,768th break.
fn lines(text: &str) -> usize {
    let breaks = text
        .bytes()
        .filter(|byte| matches!(byte, b'\r' | b'\n'))
        .count();
    let tail = !text.is_empty() && !text.ends_with(['\r', '\n']);
    breaks.saturating_add(usize::from(tail))
}

/// The first line of `text` (1-based) whose structural column passes [`MAX_STRUCTURE_COLUMN`],
/// and that column; `None` when every line is within it. Lines split at every `\r` and every
/// `\n`, as the scanner counts rows.
///
/// A line's structural column is its prefix, the bytes of its leading spaces and tabs and of
/// each `-`, `?` or `:` followed by a blank or the line's end, repeated, plus the longest run of
/// such indicators later in the line. An indicator there starts after a blank, and only blanks
/// separate it from the next. The scanner also nests on a run that follows a quoted scalar, an
/// alias, a flow collection or a second key on the line (`"a" - - -`, `*a - - -`, `[a] - - -`,
/// `a: b: - - -`), one entry per indicator. Measuring the run, not the token before it, needs no
/// YAML lexer and can't be fooled by one; ordinary text, whose indicators sit between words,
/// never forms a long run. Every byte counted is ASCII, so the column is also the scanner's.
fn too_deep(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let is_blank = |at: usize| matches!(bytes.get(at), Some(b' ' | b'\t'));
    let blank_or_end =
        |at: usize| matches!(bytes.get(at), None | Some(b' ' | b'\t' | b'\r' | b'\n'));
    let indicator = |at: usize| {
        matches!(bytes.get(at), Some(b'-' | b'?' | b':')) && blank_or_end(at.saturating_add(1))
    };
    let mut at = 0_usize;
    let mut line = 1_usize;
    while at < bytes.len() {
        let start = at;
        loop {
            while is_blank(at) {
                at = at.saturating_add(1);
            }
            if !indicator(at) {
                break;
            }
            at = at.saturating_add(1);
        }
        let prefix = at.saturating_sub(start);
        let mut deepest = prefix;
        let mut run = None;
        while let Some(&byte) = bytes.get(at) {
            if matches!(byte, b'\r' | b'\n') {
                break;
            }
            if indicator(at) && at.checked_sub(1).is_some_and(is_blank) {
                let first = *run.get_or_insert(at);
                at = at.saturating_add(1);
                while is_blank(at) {
                    at = at.saturating_add(1);
                }
                deepest = deepest.max(prefix.saturating_add(at.saturating_sub(first)));
                continue;
            }
            if !is_blank(at) {
                run = None;
            }
            at = at.saturating_add(1);
        }
        if deepest > MAX_STRUCTURE_COLUMN {
            return Some((line, deepest));
        }
        at = at.saturating_add(1);
        line = line.saturating_add(1);
    }
    None
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

    /// Why reading `text` as `/repo/big.yaml` under `deadline` is refused.
    fn refusal_under(text: &str, deadline: Deadline) -> ReadError {
        let path = AbsolutePath::new(Path::new("/repo/big.yaml"))
            .expect("the fixture path should be absolute");
        SourceFile::from_text_under(
            path,
            text.to_owned(),
            cartridge(),
            deadline,
            KeyPathBudget::defaults(),
        )
        .expect_err("the file should be refused")
    }

    fn refusal(text: &str) -> ReadError {
        refusal_under(text, Deadline::start(Limits::PARSE_TIMEOUT))
    }

    /// `lines` lines `kN: 1`, each ended by `end`.
    fn keys(lines: usize, end: &str) -> String {
        (0..lines).fold(String::new(), |mut text, key| {
            let _ = write!(text, "k{key}: 1{end}");
            text
        })
    }

    /// Asserts `error` is the line limit, counting `lines` lines.
    fn assert_too_many_lines(error: &ReadError, lines: u64) {
        assert!(
            matches!(error, ReadError::TooManyLines { lines: found, limit: 32_768, .. } if *found == lines),
            "expected read-too-many-lines for {lines} lines at 32,768, got {error:?}"
        );
        assert_eq!(error.code(), "read-too-many-lines");
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
        // A flow mapping on one line: 200,001 lines would pass the 32,768-line limit. Its parse
        // is slow in a debug build, so the read has no deadline and only the budget can stop it.
        let keys = (0..=200_000).fold(String::new(), |mut text, key| {
            let _ = write!(text, "k{key}: 1, ");
            text
        });
        let path = AbsolutePath::new(Path::new("/repo/wide.yaml"))
            .expect("the fixture path should be absolute");
        let unlimited: TimeLimit = "0s".parse().expect("`0s` should mean no time limit");
        let file = SourceFile::from_text_under(
            path,
            format!("{{{keys}}}\n"),
            cartridge(),
            Deadline::start(unlimited),
            KeyPathBudget::defaults(),
        )
        .expect("tree-sitter should return a tree");

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
    fn a_file_of_32_768_lines_reads_to_its_last_key() {
        let file = parse(&keys(32_768, "\n"));
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "k32767")),
            "k32767: 1"
        );
        let unended = keys(32_768, "\n");
        let unended = unended
            .strip_suffix('\n')
            .expect("the fixture ends with a newline");
        assert!(
            !parse(unended).has_syntax_errors(),
            "no final newline is still 32,768 lines"
        );
    }

    #[test]
    fn a_32_769th_line_is_refused_with_or_without_a_final_newline() {
        let ended = keys(32_769, "\n");
        let unended = ended
            .strip_suffix('\n')
            .expect("the fixture ends with a newline");

        assert_too_many_lines(&refusal(&ended), 32_769);
        assert_too_many_lines(&refusal(unended), 32_769);
    }

    #[test]
    fn a_carriage_return_counts_as_a_line_break_of_its_own() {
        assert!(!parse(&keys(16_384, "\r\n")).has_syntax_errors());

        assert_too_many_lines(&refusal(&keys(16_385, "\r\n")), 32_770);
        assert_too_many_lines(&refusal(&keys(32_769, "\r")), 32_769);
    }

    #[test]
    fn a_flow_only_file_past_the_limit_is_refused_too() {
        let flow = format!("[\n{}]\n", "  1,\n".repeat(39_998));

        assert_too_many_lines(&refusal(&flow), 40_000);
    }

    #[test]
    fn the_line_limit_is_checked_before_the_parse() {
        let expired: TimeLimit = "1ms".parse().expect("`1ms` should be a time limit");
        let deadline = Deadline::start(expired);
        std::thread::sleep(std::time::Duration::from_millis(5));

        assert_too_many_lines(&refusal_under(&keys(40_000, "\n"), deadline), 40_000);
    }

    #[test]
    fn a_root_key_is_never_matched_to_a_nested_twin_past_the_limit() {
        // The audit's false pass: past row 32,768 the nested `replicas` read as a root key.
        let children = keys(32_800, "\n").replace('k', "  k");
        let text = format!("replicas: 3\ntop:\n{children}  replicas: 3\n");

        let error = refusal(&text);

        assert_too_many_lines(&error, 32_803);
        assert_eq!(
            error.message_naming(&"big.yaml"),
            "big.yaml is too long for the YAML parser: it has 32803 lines and the parser reads \
             at most 32768 (it counts `\\r\\n` as two line breaks)"
        );
    }

    #[test]
    fn other_languages_read_past_the_yaml_line_limit() {
        let languages = Languages::builtin().expect("the built-in cartridges should load");
        for (name, text) in [
            ("/repo/big.toml", keys(40_000, "\n").replace(':', " =")),
            (
                "/repo/big.json",
                format!("[\n{}1\n]\n", "1,\n".repeat(39_997)),
            ),
        ] {
            let path = AbsolutePath::new(Path::new(name)).expect("the path should be absolute");
            let language = languages
                .for_path(path.as_path())
                .unwrap_or_else(|| panic!("the registry should claim `{name}`"));
            let file = SourceFile::from_text_under(
                path,
                text,
                language,
                Deadline::start(Limits::PARSE_TIMEOUT),
                KeyPathBudget::defaults(),
            );
            assert!(
                file.is_ok(),
                "`{name}` of 40,000 lines should read: {file:?}"
            );
        }
    }

    /// Asserts `error` is the depth limit at `line` and `column`.
    fn assert_too_deep(error: &ReadError, line: u64, column: u64) {
        assert!(
            matches!(
                error,
                ReadError::TooDeep { line: l, column: c, limit: 100, .. } if *l == line && *c == column
            ),
            "expected read-too-deep at line {line}, column {column}, got {error:?}"
        );
        assert_eq!(error.code(), "read-too-deep");
    }

    #[test]
    fn the_audit_s_602_byte_file_is_refused_before_it_overflows_the_scanner() {
        let error = refusal(&format!("{}x\n", "- ".repeat(300)));

        assert_too_deep(&error, 1, 600);
        assert_eq!(
            error.message_naming(&"evil.yaml"),
            "evil.yaml nests too deeply for the YAML parser: line 1 reaches column 600 in its \
             indentation and `- `/`? `/`: ` indicators, and the parser handles at most column 100"
        );
    }

    #[test]
    fn a_structure_reaching_column_100_reads_and_101_is_refused() {
        let file = parse(&format!("{}x\n", "- ".repeat(50)));
        assert_eq!(symbols(&file).iter().count(), 50);

        assert_too_deep(&refusal(&format!("{} x\n", "- ".repeat(50))), 1, 101);
    }

    #[test]
    fn a_mapping_and_a_sequence_sharing_each_column_pins_the_bound_at_100() {
        // Two scanner stack entries per column: the overflow comes at column 127, so 128 would
        // be too loose a bound, and 100 leaves room.
        let shape = |last: usize| {
            (0..=last).fold(String::new(), |mut text, indent| {
                let pad = " ".repeat(indent);
                let _ = write!(text, "{pad}k:\n{pad}-\n");
                text
            })
        };

        let _read = parse(&shape(49));
        assert_too_deep(&refusal(&shape(100)), 202, 101);
    }

    #[test]
    fn deep_indentation_and_runs_of_indicators_are_refused() {
        let indented = (0..300).fold(String::new(), |mut text, indent| {
            let _ = writeln!(text, "{}k:", " ".repeat(indent));
            text
        });

        assert_too_deep(&refusal(&indented), 102, 101);
        assert_too_deep(&refusal(&format!("{}x\n", "? ".repeat(300))), 1, 600);
        assert_too_deep(&refusal(&format!("{}x\n", ": ".repeat(300))), 1, 600);
        assert_too_deep(&refusal(&format!("{}x\n", " ".repeat(40_000))), 1, 40_000);
    }

    /// The lines that put a run of 300 block indicators after a token the line-start prefix
    /// doesn't count, each with the 1-based line it sits on. The unguarded scanner aborts on
    /// the quoted, alias, flow and second-key shapes; the rest are refused at the same bound.
    pub(super) fn deep_runs() -> Vec<(String, u64)> {
        let run = "- ".repeat(300);
        let mut shapes: Vec<(String, u64)> = [
            "--- ",
            "&a ",
            "!t ",
            "!!seq ",
            "\"a\" ",
            "'a' ",
            "a: ",
            "? ",
            "[a] ",
            "{a: 1} ",
            "a: \"b\" ",
            "a: b: ",
            "\"a\": \"b\" ",
            "!t \"a\" ",
            "a: [x] ",
        ]
        .into_iter()
        .flat_map(|before| [format!("{before}{run}x\n"), format!("- {before}{run}x\n")])
        .map(|text| (text, 1))
        .collect();
        shapes.push((format!("b: &x 1\na: *x {run}x\n"), 2));
        shapes.push((format!("\"a\" {}x\n", "? ".repeat(300)), 1));
        shapes.push((format!("\"a\" {}x\n", "-  ".repeat(300)), 1));
        shapes
    }

    #[test]
    fn a_run_of_indicators_after_any_token_is_refused_at_the_same_bound() {
        for (text, line) in deep_runs() {
            let error = refusal(&text);
            assert!(
                matches!(error, ReadError::TooDeep { line: l, column, limit: 100, .. } if l == line && column > 100),
                "{text:.40?}… should be read-too-deep on line {line}, got {error:?}"
            );
        }
        assert_too_deep(&refusal(&format!("\"a\" {}x\n", "- ".repeat(300))), 1, 600);
        assert_too_deep(
            &refusal(&format!("- \"a\" {}x\n", "- ".repeat(300))),
            1,
            602,
        );
    }

    #[test]
    fn tabs_count_as_blanks_in_the_depth_measure() {
        let admitted = |text: &str| {
            let path = AbsolutePath::new(Path::new("/repo/tabs.yaml"))
                .expect("the fixture path should be absolute");
            cartridge().admit(&path, text)
        };

        let indented = admitted(&format!("{}x\n", "\t".repeat(101)))
            .expect_err("101 tabs of indentation should pass column 100");
        assert_too_deep(&indented, 1, 101);
        let separated = admitted(&format!("{}x\n", "-\t".repeat(51)))
            .expect_err("51 tab-separated indicators should pass column 100");
        assert_too_deep(&separated, 1, 102);
        let later = admitted(&format!("\"a\" {}x\n", "-\t".repeat(51)))
            .expect_err("a later run of 51 tab-separated indicators should pass column 100");
        assert_too_deep(&later, 1, 102);
        assert!(
            admitted(&format!("{}x\n", "-\t".repeat(50))).is_ok(),
            "50 tab-separated indicators reach column 100 exactly"
        );
    }

    #[test]
    fn wide_lines_and_indicators_between_words_are_not_refused() {
        let wide = [
            format!("k: {}\n", "x".repeat(40_000)),
            format!("[{}1]\n", "1, ".repeat(19_999)),
            format!("k: {}\n", "prose - with: dashes - and: colons ".repeat(10)),
            format!(
                "k: {}\n",
                "step one - step two : then ? maybe - more ".repeat(10)
            ),
            format!("rule: \"{}\"\n", "- ".repeat(40)),
            format!("- \"a\" {}x\n", "- \"a\" ".repeat(300)),
        ];

        for text in wide {
            let _read = parse(&text);
        }
    }

    #[test]
    fn carriage_returns_split_lines_as_the_scanner_counts_rows() {
        let deep = format!("{}x", "- ".repeat(60));

        assert_too_deep(&refusal(&format!("a: 1\r{deep}\r")), 2, 120);
        assert_too_deep(&refusal(&format!("a: 1\r\n{deep}\r\n")), 3, 120);
    }

    #[test]
    fn the_depth_limit_is_checked_before_the_parse() {
        let expired: TimeLimit = "1ms".parse().expect("`1ms` should be a time limit");
        let deadline = Deadline::start(expired);
        std::thread::sleep(std::time::Duration::from_millis(5));

        assert_too_deep(
            &refusal_under(&format!("{}x\n", "- ".repeat(300)), deadline),
            1,
            600,
        );
    }

    #[test]
    fn other_languages_read_past_the_yaml_depth_limit() {
        let languages = Languages::builtin().expect("the built-in cartridges should load");
        let pad = " ".repeat(200);
        for (name, text) in [
            ("/repo/deep.toml", format!("[a]\n{pad}b = 1\n")),
            ("/repo/deep.json", format!("{{\n{pad}\"b\": 1\n}}\n")),
        ] {
            let path = AbsolutePath::new(Path::new(name)).expect("the path should be absolute");
            let language = languages
                .for_path(path.as_path())
                .unwrap_or_else(|| panic!("the registry should claim `{name}`"));
            let file = SourceFile::from_text_under(
                path,
                text,
                language,
                Deadline::start(Limits::PARSE_TIMEOUT),
                KeyPathBudget::defaults(),
            );
            assert!(
                file.is_ok(),
                "`{name}` indented 200 spaces should read: {file:?}"
            );
        }
    }

    #[test]
    fn keep_chomping_puts_the_trailing_blank_lines_in_the_key_s_span() {
        for header in ["|+", ">+"] {
            let one = parse(&format!("a: {header}\n  keep\n\nnext: 1\n"));
            let three = parse(&format!("a: {header}\n  keep\n\n\n\nnext: 1\n"));

            assert_ne!(
                text_of(&one, resolved(&one, &symbols(&one), "a")),
                text_of(&three, resolved(&three, &symbols(&three), "a")),
                "a blank line added to a `{header}` value should change its span"
            );
        }
    }

    #[test]
    fn clip_and_strip_chomping_leave_the_trailing_blank_lines_out() {
        for header in ["|", ">", "|-"] {
            let one = parse(&format!("a: {header}\n  keep\n\nnext: 1\n"));
            let three = parse(&format!("a: {header}\n  keep\n\n\n\nnext: 1\n"));

            assert_eq!(
                text_of(&one, resolved(&one, &symbols(&one), "a")),
                text_of(&three, resolved(&three, &symbols(&three), "a")),
                "the blank lines after a `{header}` value are not part of it"
            );
        }
    }

    #[test]
    fn one_document_and_a_trailing_marker_keep_plain_paths() {
        let file = parse("a:\n  b: 1\n---\n");

        assert_eq!(paths(&symbols(&file)), ["a", "a.b"]);
    }

    #[test]
    fn empty_documents_take_an_index_but_never_make_a_stream_several() {
        let lone = parse("---\n# nothing here\n---\nb: 2\n");
        let two = parse("a: 1\n---\n# nothing here\n---\nb: 2\n");

        assert_eq!(paths(&symbols(&lone)), ["b"]);
        assert_eq!(paths(&symbols(&two)), ["0", "0.a", "1", "2", "2.b"]);
    }

    #[test]
    fn the_deadline_stops_the_walk_inside_a_run_of_children_that_name_nothing() {
        // The comments name nothing, so only the per-child check can stop the walk among them;
        // without it the walk would run on to `last` and stop there. The deadline counts checks
        // rather than time: the walk makes a few dozen before the comments, so it passes at
        // the thousandth, inside the run of 2,000, on any machine.
        let comments = "  # nothing\n".repeat(2_000);
        let text = format!("a: [first,\n{comments}  last]\n");
        let last = text
            .rfind("last")
            .expect("the fixture should end with `last`");
        let file = parse(&text).with_deadline(Deadline::after_checks(1_000));

        let error = file
            .language()
            .symbols(&file)
            .expect_err("the walk should stop at the thousandth check");

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
