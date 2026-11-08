//! JSON: a link target whose symbols are its keys and array elements.
//!
//! Paths follow the key-path model the data cartridges share ([`crate::cartridges::data`]):
//! `package.json#scripts.build`, `package.json#exports.~1/utils` for the key `./utils`, and
//! `tsconfig.json#compilerOptions.paths.@app~1*.0`. A key is its string with the JSON escapes
//! decoded, so `"\u002e"` is the key `.`, written `~1`. A top-level array names its elements by
//! index from the root: `data.json#0.name`. A top-level scalar names nothing.
//!
//! A key's symbol covers its `pair` node (key, `:` and value); an array element's covers its value.
//! Two keys with the same path, which JSON parsers disagree on, are both kept, so a link to that
//! path is ambiguous. Comments parse too, with the same grammar: `.jsonc` files, and the `.json`
//! files that are JSONC in all but name, such as `tsconfig.json` and VS Code's `settings.json`.
//! A placeholder the parser inserts to recover from an error, such as the value a trailing comma
//! seems to promise, names nothing.
//!
//! JSON files are link targets only: the cartridge returns no Markdown regions and the walk never
//! reads them as sources. The walk keeps its own stack and charges every symbol to the file's
//! key-path budget before its path is allocated, and checks the read's deadline at every child it
//! passes, so hostile nesting or width fails closed (SEC-1). It steps through each container's
//! children with a cursor rather than collecting them, holding a frame only for a container whose
//! own symbol it already charged, so memory beyond the parse grows with the budget, not with the
//! file.

use std::sync::LazyLock;

use tree_sitter::{Node, TreeCursor};

use crate::cartridges::data::{KeyId, KeyPathError, KeyPaths, index_name, key_name};
use crate::read::cartridge::{Cartridge, LanguageId, MarkdownRegion, SourceFile, SymbolTable};
use crate::read::error::ReadError;

/// The id, in errors and `info` output.
pub(crate) const ID: LanguageId = LanguageId::new("json");

/// The extensions it reads, lowercase without the dot.
pub(crate) const EXTENSIONS: &[&str] = &["json", "jsonc"];

/// The node kinds a value can be, so an array's elements are counted the way JSON counts them:
/// comments and error nodes between elements name nothing.
const VALUE_KINDS: &[&str] = &[
    "object", "array", "string", "number", "true", "false", "null",
];

/// The JSON language: keys and array elements as symbols, no link regions.
#[derive(Debug)]
pub(crate) struct Json {
    /// The grammar.
    grammar: tree_sitter::Language,
}

/// The cartridge, built the first time it is used.
static JSON: LazyLock<Json> = LazyLock::new(|| Json {
    grammar: tree_sitter_json::LANGUAGE.into(),
});

/// The JSON cartridge.
#[must_use]
pub(crate) fn cartridge() -> &'static Json {
    &JSON
}

impl Cartridge for Json {
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
            walk(file.text(), file.tree().root_node(), paths)
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
    /// The document's top-level values, which name nothing themselves: the members of a top-level
    /// object, or the elements of a top-level array, are named from the root.
    Top,
    /// An object's members.
    Members,
    /// An array's elements.
    Elements {
        /// The 0-based index the next element is named by.
        next: usize,
    },
}

/// A container the walk is going through, waiting on the walk's stack.
struct Frame<'tree> {
    /// The symbol its children go under (`None` at the root).
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

    /// The frame that goes through `value`'s children under `parent`, when it has any: an object's
    /// members or an array's elements.
    fn into(parent: Option<KeyId>, value: Node<'tree>) -> Option<Self> {
        let children = match value.kind() {
            "object" => Children::Members,
            "array" => Children::Elements { next: 0 },
            _ => return None,
        };
        Some(Self::new(parent, value, children))
    }

    /// The next child after the last one taken that names something here: a `pair` in an
    /// object, a value anywhere else, never a placeholder the parser inserted to recover from an
    /// error. `None` once there are none. The cursor moves one sibling at
    /// a time, so nothing is allocated, and every child passed over is checked against the
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
                Children::Members => child.kind() == "pair",
                Children::Top | Children::Elements { .. } => VALUE_KINDS.contains(&child.kind()),
            };
            if child.is_named() && is_written(child) && wanted {
                return Ok(Some(child));
            }
        }
    }
}

/// Whether `node` is written in the file, rather than a placeholder the parser inserted to recover
/// from an error, such as the element a trailing comma in `[1, 2, ]` seems to promise.
fn is_written(node: Node<'_>) -> bool {
    !node.is_missing() && node.start_byte() < node.end_byte()
}

/// Makes a symbol for every key and array element of the document rooted at `root`, depth first
/// in file order.
///
/// The walk keeps its own stack, so any nesting depth is safe. A frame goes on it only for a
/// value whose symbol was just made (or a top-level value), so the stack never outgrows the
/// budget, and children are taken one at a time rather than collected.
///
/// # Errors
///
/// The first [`KeyPathError`]: the deadline passed or the file's key paths are over budget.
fn walk(text: &str, root: Node<'_>, paths: &mut KeyPaths) -> Result<(), KeyPathError> {
    let mut frames = vec![Frame::new(None, root, Children::Top)];
    while let Some(frame) = frames.last_mut() {
        let Some(child) = frame.next(paths)? else {
            let _done = frames.pop();
            continue;
        };
        let parent = frame.parent;
        let made = match &mut frame.children {
            Children::Top => Some((None, child)),
            Children::Members => member(text, parent, child, paths)?,
            Children::Elements { next } => {
                let index = *next;
                *next = index.saturating_add(1);
                match index_name(index) {
                    Some(name) => Some((Some(paths.key(parent, name, child.byte_range())?), child)),
                    None => None,
                }
            }
        };
        if let Some(inner) = made.and_then(|(id, value)| Frame::into(id, value)) {
            frames.push(inner);
        }
    }
    Ok(())
}

/// Makes the symbol a `pair` names under `parent`, covering the pair. Returns it and the value to
/// walk into, or `None` when the key names nothing (empty, or not a string JSON allows) or the
/// pair has no value, so nothing under it is named.
///
/// # Errors
///
/// As [`walk`].
fn member<'tree>(
    text: &str,
    parent: Option<KeyId>,
    pair: Node<'tree>,
    paths: &mut KeyPaths,
) -> Result<Option<(Option<KeyId>, Node<'tree>)>, KeyPathError> {
    let Some(name) = pair
        .child_by_field_name("key")
        .and_then(|key| text.get(key.byte_range()))
        .and_then(unquote)
        .as_deref()
        .and_then(key_name)
    else {
        return Ok(None);
    };
    let id = paths.key(parent, name, pair.byte_range())?;
    Ok(pair
        .child_by_field_name("value")
        .map(|value| (Some(id), value)))
}

/// The key a JSON string spells, with its escapes decoded. `None` when it is not quoted, holds an
/// escape JSON doesn't allow, or spells a lone surrogate, which no string can hold.
fn unquote(raw: &str) -> Option<String> {
    let body = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut key = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(next) = chars.next() {
        if next != '\\' {
            key.push(next);
            continue;
        }
        let decoded = match chars.next()? {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => unicode(&mut chars)?,
            _ => return None,
        };
        key.push(decoded);
    }
    Some(key)
}

/// The character a `\u` escape spells, its `u` already taken from `chars`: one scalar, or a high
/// surrogate followed by `\u` and its low surrogate. `None` for anything else.
fn unicode(chars: &mut std::str::Chars<'_>) -> Option<char> {
    let first = hex4(chars)?;
    if !(0xD800..0xDC00).contains(&first) {
        return char::from_u32(first);
    }
    if chars.next()? != '\\' || chars.next()? != 'u' {
        return None;
    }
    let low = hex4(chars)?;
    if !(0xDC00..0xE000).contains(&low) {
        return None;
    }
    char::from_u32(0x1_0000 + ((first - 0xD800) << 10) + (low - 0xDC00))
}

/// The number the next four hex digits of `chars` spell. `None` when they are fewer or not hex.
fn hex4(chars: &mut std::str::Chars<'_>) -> Option<u32> {
    let hex: String = chars.take(4).collect();
    if hex.len() != 4 || !hex.chars().all(|digit| digit.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(&hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::path::Path;

    use super::{cartridge, unquote, walk};
    use crate::cartridges::data::{KeyPathBudget, KeyPathError, KeyPaths};
    use crate::limits::Limits;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, Deadline, Languages, SourceFile, Symbol, SymbolTable};
    use crate::read::error::ReadError;

    /// A package manifest with nested keys, a key holding `.`, an array and an escaped key.
    const PACKAGE: &str = r#"{
  "name": "demo",
  "scripts": {
    "build": "tsc -p .",
    "test": "vitest"
  },
  "exports": {
    ".": "./dist/index.js",
    "./utils": "./dist/utils.js"
  },
  "files": ["dist", "README.md"],
  "caf\u00e9~\u002e": true
}
"#;

    fn parse(text: &str) -> SourceFile {
        parse_under(text, KeyPathBudget::defaults())
    }

    /// `text` parsed as `/repo/package.json`, its key paths to be built under `budget`.
    fn parse_under(text: &str, budget: KeyPathBudget) -> SourceFile {
        let path = crate::path::fixture("/repo/package.json");
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
    fn package_json_names_every_key_and_element() {
        let file = parse(PACKAGE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "name",
                "scripts",
                "scripts.build",
                "scripts.test",
                "exports",
                "exports.~1",
                "exports.~1/utils",
                "files",
                "files.0",
                "files.1",
                "café~0~1",
            ],
        );
    }

    #[test]
    fn a_key_holding_a_dot_is_one_escaped_segment() {
        let file = parse(PACKAGE);
        let table = symbols(&file);

        // `./utils` is `~1` for the `.`, then `/utils` as written.
        assert_eq!(
            text_of(&file, resolved(&file, &table, "exports.~1/utils")),
            r#""./utils": "./dist/utils.js""#
        );
    }

    #[test]
    fn a_pair_covers_its_key_and_value_and_an_element_its_value() {
        let file = parse(PACKAGE);
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "scripts.build")),
            r#""build": "tsc -p .""#
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "files.1")),
            r#""README.md""#
        );
        assert!(
            text_of(&file, resolved(&file, &table, "scripts"))
                .ends_with("\"test\": \"vitest\"\n  }"),
            "an object's pair should cover its whole value"
        );
    }

    #[test]
    fn a_unicode_escaped_key_is_named_by_what_it_spells() {
        let file = parse(PACKAGE);
        let table = symbols(&file);

        let symbol = resolved(&file, &table, "café~0~1");

        assert!(text_of(&file, symbol).starts_with(r#""caf\u00e9~\u002e""#));
    }

    #[test]
    fn json_escapes_decode_and_a_lone_surrogate_names_nothing() {
        assert_eq!(
            unquote(r#""a\tb\/c\"\\\u00e9\ud83d\ude00""#).as_deref(),
            Some("a\tb/c\"\\\u{e9}\u{1f600}")
        );
        assert_eq!(unquote(r#""\ud800""#), None, "a lone surrogate");
        assert_eq!(
            unquote(r#""\ud800\u0041""#),
            None,
            "a high surrogate then a scalar"
        );
        assert_eq!(unquote(r#""\x41""#), None, "JSON has no `\\x` escape");
        assert_eq!(unquote(r#""\u12""#), None, "a short `\\u` escape");
    }

    #[test]
    fn keys_written_with_escapes_are_named_by_what_they_decode_to() {
        let file = parse(r#"{"caf\u00e9": {"\ud83d\ude00": 1}, "a\u002eb": 2}"#);

        assert_eq!(
            paths(&symbols(&file)),
            ["caf\u{e9}", "caf\u{e9}.\u{1f600}", "a~1b"]
        );
    }

    #[test]
    fn an_escaped_and_a_literal_spelling_of_one_key_are_ambiguous() {
        let file = parse("{\"caf\\u00e9\": 1, \"caf\u{e9}\": 2}");
        let table = symbols(&file);

        let error = table
            .resolve_text(file.path(), "caf\u{e9}")
            .expect("the path should parse")
            .expect_err("two spellings of one key should not resolve to one");

        assert_eq!(error.code(), "resolve-ambiguous");
    }

    #[test]
    fn a_trailing_comma_in_an_array_names_no_placeholder_element() {
        let file = parse(r#"{"b": [1, 2, ]}"#);

        assert_eq!(paths(&symbols(&file)), ["b", "b.0", "b.1"]);
    }

    #[test]
    fn a_tsconfig_with_a_trailing_comma_names_only_its_written_elements() {
        let file = parse(
            "{\n  // TypeScript allows comments and trailing commas\n  \"compilerOptions\": {\n    \"lib\": [\"es2022\", \"dom\",],\n  },\n}\n",
        );

        let found = paths(&symbols(&file));

        assert_eq!(
            found,
            [
                "compilerOptions",
                "compilerOptions.lib",
                "compilerOptions.lib.0",
                "compilerOptions.lib.1",
            ],
        );
    }

    #[test]
    fn a_duplicate_key_is_ambiguous() {
        let file = parse(r#"{"scripts": {"build": "a", "build": "b"}}"#);
        let table = symbols(&file);

        let error = table
            .resolve_text(file.path(), "scripts.build")
            .expect("the path should parse")
            .expect_err("two keys with the same path should not resolve to one");

        assert_eq!(error.code(), "resolve-ambiguous");
    }

    #[test]
    fn a_top_level_array_names_its_elements_from_the_root() {
        let file = parse(r#"[{"name": "a"}, [1, 2], "x"]"#);

        assert_eq!(
            paths(&symbols(&file)),
            ["0", "0.name", "1", "1.0", "1.1", "2"]
        );
    }

    #[test]
    fn a_top_level_scalar_names_nothing() {
        let file = parse("\"1.2\"\n");

        assert_eq!(paths(&symbols(&file)), Vec::<String>::new());
    }

    #[test]
    fn an_empty_key_names_nothing_under_it() {
        let file = parse(r#"{"": {"a": 1}, "b": {"": 2, "c": 3}}"#);

        assert_eq!(paths(&symbols(&file)), ["b", "b.c"]);
    }

    #[test]
    fn jsonc_comments_name_nothing_and_do_not_shift_indices() {
        let file = parse(
            "{\n  // the build\n  \"list\": [\n    /* first */ \"a\",\n    // second\n    \"b\"\n  ]\n}\n",
        );

        assert!(!file.has_syntax_errors(), "the fixture is valid JSONC");
        assert_eq!(paths(&symbols(&file)), ["list", "list.0", "list.1"]);
    }

    #[test]
    fn json_is_a_link_target_only() {
        let file = parse(PACKAGE);

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
    fn the_registry_reads_json_and_jsonc_files_with_this_cartridge() {
        let languages = Languages::builtin().expect("the built-in cartridges should load");

        for name in ["package.json", "tsconfig.jsonc"] {
            let language = languages
                .for_path(Path::new(name))
                .unwrap_or_else(|| panic!("the registry should claim `{name}`"));

            assert_eq!(language.id().as_str(), "json", "{name}");
            assert!(!language.reads_links(), "{name}");
        }
    }

    #[test]
    fn deep_nesting_fails_closed_instead_of_exhausting_memory() {
        let depth = 100_000;
        let nested = format!("{}1{}\n", "{\"a\": ".repeat(depth), "}".repeat(depth));
        let file = parse(&nested);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("100,000 nested objects should not build");

        assert!(
            matches!(
                error,
                ReadError::TooManyKeys { .. } | ReadError::Timeout { .. }
            ),
            "expected a budget or deadline failure, got {error:?}"
        );
    }

    #[test]
    fn deep_arrays_fail_closed_without_recursion() {
        let depth = 100_000;
        let nested = format!("{}{}\n", "[".repeat(depth), "]".repeat(depth));
        let file = parse(&nested);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("100,000 nested arrays should not build");

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
        let mut wide = (0..=200_000).fold(String::from("{"), |mut text, key| {
            let _ = write!(text, "\"k{key}\": 1,");
            text
        });
        let _ = wide.pop();
        wide.push('}');
        let file = parse(&wide);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("200,001 keys should exceed the default segment budget");

        assert_eq!(error.code(), "read-too-many-keys");
    }

    #[test]
    fn a_wide_array_past_the_default_limit_fails_closed() {
        let wide = format!("[{}0]", "0,".repeat(200_000));
        let file = parse(&wide);

        let error = file
            .language()
            .symbols(&file)
            .expect_err("200,001 elements should exceed the default segment budget");

        assert_eq!(error.code(), "read-too-many-keys");
    }

    #[test]
    fn the_file_s_budget_bounds_its_key_paths() {
        let file = parse_under(
            PACKAGE,
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
    fn a_passed_deadline_stops_the_walk_at_a_child_that_names_nothing() {
        // Only a comment: no symbol is ever charged, so only the per-child check can stop it.
        let file = parse("// nothing but a comment\n");
        let deadline = Deadline::start("1ms".parse().expect("1ms should be a time limit"));
        while !deadline.passed() {
            std::hint::spin_loop();
        }

        let built = KeyPaths::build(KeyPathBudget::defaults(), deadline, |paths| {
            walk(file.text(), file.tree().root_node(), paths)
        });

        assert!(
            matches!(built, Err(KeyPathError::Timeout(stopped)) if stopped.offset == 0),
            "{built:?}",
        );
    }

    #[test]
    fn a_syntax_error_still_names_the_keys_around_it() {
        let file = parse(r#"{"name": "demo", "broken": , "version": "1"}"#);

        assert!(file.has_syntax_errors(), "the fixture is invalid JSON");
        let found = paths(&symbols(&file));
        assert!(
            found.contains(&"name".to_owned()),
            "keys before the error should still be symbols: {found:?}"
        );
    }

    #[test]
    fn invalid_utf8_is_refused_before_the_cartridge_reads_it() {
        let dir = tempfile::tempdir().expect("a temp dir should be creatable");
        let path = AbsolutePath::new(&dir.path().join("package.json"))
            .expect("a temp dir path should be absolute");
        fs::write(path.as_path(), b"{\"a\": \"\xff\"}\n").expect("the fixture should be writable");

        let error = SourceFile::read(
            path,
            Languages::builtin().expect("the built-in cartridges should load"),
            Limits::PARSE_TIMEOUT,
            Limits::MAX_FILE_SIZE,
            KeyPathBudget::defaults(),
        )
        .expect_err("a file with an invalid UTF-8 byte should be rejected");

        assert_eq!(error.code(), "read-not-utf8");
    }
}
