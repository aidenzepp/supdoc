//! Python: a query-driven cartridge plus one override, `self.x` attributes of `__init__` (03 R1,
//! Overrides).
//!
//! Symbols come from `tags.scm` and link regions (comments and docstrings) from `injections.scm`,
//! both beside this file. Queries can't match at an arbitrary depth, so the attributes a class
//! assigns anywhere inside `__init__` are found here instead, by walking the tree.

use std::sync::LazyLock;

use tree_sitter::Node;

use crate::link::SymbolPath;
use crate::read::cartridge::{
    Cartridge, CompiledQueries, LanguageId, MarkdownRegion, SourceFile, Symbol, SymbolTable,
};
use crate::read::query::{QueryCartridge, QueryFileError};

/// Symbols: `@definition.*` and `@name`.
const TAGS: &str = include_str!("tags.scm");

/// Link regions: every `#` comment and docstring, as `markdown_inline`.
const INJECTIONS: &str = include_str!("injections.scm");

/// The method whose `self.x = …` assignments declare a class's attributes.
const INIT: &str = "__init__";

/// The receiver name those assignments are made through.
const RECEIVER: &str = "self";

/// The cartridge, compiled the first time it is used.
static PYTHON: LazyLock<Result<PythonCartridge, QueryFileError>> = LazyLock::new(|| {
    QueryCartridge::new(
        LanguageId::new("python"),
        &["py", "pyi"],
        tree_sitter_python::LANGUAGE.into(),
        TAGS,
        INJECTIONS,
    )
    .map(|queries| PythonCartridge { queries })
});

/// The Python cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for a built-in query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static PythonCartridge, &'static QueryFileError> {
    PYTHON.as_ref()
}

/// Python: the data-driven cartridge, plus the attributes `__init__` assigns to `self`.
#[derive(Debug)]
pub struct PythonCartridge {
    /// Everything the queries express.
    queries: QueryCartridge,
}

impl Cartridge for PythonCartridge {
    fn id(&self) -> LanguageId {
        self.queries.id()
    }

    fn extensions(&self) -> &'static [&'static str] {
        self.queries.extensions()
    }

    fn grammar(&self) -> &tree_sitter::Language {
        self.queries.grammar()
    }

    fn queries(&self) -> &CompiledQueries {
        self.queries.queries()
    }

    fn symbols(&self, file: &SourceFile) -> SymbolTable {
        let tagged = self.queries.symbols(file);
        let attributes = init_attributes(file, &tagged);
        if attributes.is_empty() {
            return tagged;
        }
        let mut symbols: Vec<Symbol> = tagged.iter().cloned().chain(attributes).collect();
        symbols.sort_by_key(|symbol| (symbol.range.start, std::cmp::Reverse(symbol.range.end)));
        SymbolTable::new(symbols)
    }

    fn markdown_regions(&self, file: &SourceFile) -> Vec<MarkdownRegion> {
        self.queries.markdown_regions(file)
    }
}

/// One symbol per `self.x` that some class's `__init__` assigns, named `Class.x`.
///
/// The symbol is the first assignment to `x`. A class attribute of the same name, already in
/// `tagged`, keeps the name. Assignments inside a nested function or class belong to that scope's
/// own `self`, so they are skipped.
fn init_attributes(file: &SourceFile, tagged: &SymbolTable) -> Vec<Symbol> {
    let mut attributes: Vec<Symbol> = Vec::new();
    let mut pending = vec![file.tree().root_node()];
    while let Some(node) = pending.pop() {
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
        if node.kind() != "class_definition" {
            continue;
        }
        let Some(class) = tagged
            .iter()
            .find(|symbol| symbol.range == node.byte_range())
        else {
            continue;
        };
        for init in node
            .child_by_field_name("body")
            .into_iter()
            .flat_map(|body| init_methods(body, file.text()))
        {
            for (name, assignment) in assigned_attributes(init, file.text()) {
                let mut segments = class.path.segments().to_vec();
                segments.push(name);
                let Ok(path) = SymbolPath::from_segments(segments) else {
                    continue;
                };
                let taken = |symbol: &Symbol| symbol.path == path;
                if !tagged.iter().any(taken) && !attributes.iter().any(taken) {
                    attributes.push(Symbol {
                        path,
                        aliases: Vec::new(),
                        range: assignment.byte_range(),
                    });
                }
            }
        }
    }
    attributes.sort_by_key(|symbol| symbol.range.start);
    attributes
}

/// The `__init__` methods directly in a class body, decorated or not.
fn init_methods<'tree>(body: Node<'tree>, text: &str) -> Vec<Node<'tree>> {
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .filter_map(|statement| match statement.kind() {
            "decorated_definition" => statement.child_by_field_name("definition"),
            _ => Some(statement),
        })
        .filter(|node| node.kind() == "function_definition")
        .filter(|node| {
            node.child_by_field_name("name")
                .and_then(|name| text.get(name.byte_range()))
                == Some(INIT)
        })
        .collect()
}

/// The `(x, assignment)` pairs for every `self.x = …` in `function`, in source order.
fn assigned_attributes<'tree>(function: Node<'tree>, text: &str) -> Vec<(String, Node<'tree>)> {
    let mut found = Vec::new();
    let mut pending: Vec<Node<'tree>> = function.child_by_field_name("body").into_iter().collect();
    while let Some(node) = pending.pop() {
        match node.kind() {
            "function_definition" | "class_definition" | "lambda" => continue,
            "assignment" => {
                if let Some(name) = node
                    .child_by_field_name("left")
                    .and_then(|left| self_attribute(left, text))
                {
                    found.push((name, node));
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        pending.extend(node.children(&mut cursor));
    }
    found.sort_by_key(|(_, node)| node.start_byte());
    found
}

/// `x` if `target` is exactly `self.x`.
fn self_attribute(target: Node<'_>, text: &str) -> Option<String> {
    if target.kind() != "attribute" {
        return None;
    }
    let object = target.child_by_field_name("object")?;
    let attribute = target.child_by_field_name("attribute")?;
    (object.kind() == "identifier" && text.get(object.byte_range()) == Some(RECEIVER))
        .then(|| text.get(attribute.byte_range()).map(str::to_owned))
        .flatten()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::error::ResolveError;
    use crate::read::hash::{hash_file, hash_symbol};

    /// Every kind of definition the cartridge names, once.
    const FIXTURE: &str = r#""""A fixture. See [guide](guide.md)."""
import os

LIMIT = 10


def add(a, b):
    """Adds two numbers."""
    return a + b


@dataclass
class Config:
    port: int = 0
    label = "config"

    def __init__(self, port):
        self.port = port
        self.host = "localhost"  # [host](host.md)
        if port:
            self.secure = True
        else:
            for _ in range(2):
                self.retries = 3

        def helper():
            self.ignored = 1

    @property
    def url(self):
        return self.host

    class Inner:
        def run(self):
            self.not_an_attribute = 1


def outer():
    def inner():
        pass
"#;

    fn parse(text: &str) -> SourceFile {
        let python = cartridge().expect("the built-in Python queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/app.py"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), python).expect("tree-sitter should return a tree")
    }

    fn symbols(file: &SourceFile) -> SymbolTable {
        file.language().symbols(file)
    }

    fn paths(table: &SymbolTable) -> Vec<String> {
        table.iter().map(|symbol| symbol.path.to_string()).collect()
    }

    fn query(raw: &str) -> SymbolPath {
        raw.parse().expect("the fixture symbol should be valid")
    }

    fn resolved<'t>(file: &SourceFile, table: &'t SymbolTable, raw: &str) -> &'t Symbol {
        table
            .resolve(file.path(), &query(raw))
            .unwrap_or_else(|error: ResolveError| panic!("`{raw}` should resolve: {error}"))
    }

    fn text_of<'f>(file: &'f SourceFile, symbol: &Symbol) -> &'f str {
        file.text()
            .get(symbol.range.clone())
            .expect("a symbol's range should be valid for its file")
    }

    /// Each region's text, comment markers left out, in file order.
    fn region_texts(file: &SourceFile) -> Vec<String> {
        file.language()
            .markdown_regions(file)
            .iter()
            .map(|region| {
                region
                    .text_within(0..file.text().len(), file.text())
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn python_exposes_documented_symbols() {
        let file = parse(FIXTURE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "LIMIT",
                "add",
                "Config",
                "Config.port",
                "Config.label",
                "Config.__init__",
                "Config.host",
                "Config.secure",
                "Config.retries",
                "Config.__init__.helper",
                "Config.url",
                "Config.Inner",
                "Config.Inner.run",
                "outer",
                "outer.inner",
            ],
        );
    }

    #[test]
    fn python_hashes_match_pinned_values() {
        let file = parse(FIXTURE);
        let table = symbols(&file);
        let pinned: [(&str, &str); 8] = [
            (
                "LIMIT",
                "a5ea39047eefc794477998d32416c9c1294911bf425045a8659144539a347936",
            ),
            (
                "add",
                "81684265d23ae048698d76fe862eb1e1f28cc2103ccc732a143069fb15c30157",
            ),
            (
                "Config",
                "8bc9ff5614ce7940135e63c9d255fcc5af8eeb7492f6fd840b49ed1bb67995dd",
            ),
            (
                "Config.port",
                "afebc25ee05c95d897386811d2c264033ba5d158ef1d01c2f7cd958c066f09dc",
            ),
            (
                "Config.host",
                "ad7ad56d9824fdb47945a2ec6f024cb2d8c385dc8fc0bf4fa99588559452f98b",
            ),
            (
                "Config.secure",
                "3e4374a032131a74610fc3dd906d8d9ccf4c0a1c454c33b84b640f707efae30d",
            ),
            (
                "Config.url",
                "df850b32ef2a500d52c49ba6d99b8cc6eca6c951de78b63e3398b159da658d4f",
            ),
            (
                "Config.Inner.run",
                "8eff1b16e6446f28fc949de643dd15cae1605e6e9d470100e07df0a1d38627c6",
            ),
        ];

        let actual: Vec<(&str, String)> = pinned
            .iter()
            .map(|(path, _)| {
                (
                    *path,
                    hash_symbol(&file, resolved(&file, &table, path)).to_string(),
                )
            })
            .collect();
        let expected: Vec<(&str, String)> = pinned
            .iter()
            .map(|(path, hash)| (*path, (*hash).to_owned()))
            .collect();
        assert_eq!(
            actual, expected,
            "a grammar upgrade moved a symbol's node boundaries (02 L8)"
        );
        assert_eq!(
            hash_file(&file).to_string(),
            "31a0358069808a3562e770e5d2624c9c9a65c8c472e06a19ff37e0dfa6df29fa",
            "the whole-file hash changed"
        );
    }

    #[test]
    fn python_symbol_range_matches_tree_sitter_node() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        for symbol in table.iter() {
            let node = file
                .tree()
                .root_node()
                .descendant_for_byte_range(symbol.range.start, symbol.range.end)
                .expect("a symbol's range should lie inside the tree");
            assert_eq!(
                node.byte_range(),
                symbol.range,
                "`{}` should cover exactly one node, not {:?}",
                symbol.path,
                text_of(&file, symbol),
            );
        }
        assert_eq!(
            text_of(&file, resolved(&file, &table, "Config.secure")),
            "self.secure = True",
        );
    }

    #[test]
    fn python_edit_outside_symbol_keeps_hash() {
        let before = parse(FIXTURE);
        let after = parse(&FIXTURE.replace("return a + b", "return b + a"));
        let (before_table, after_table) = (symbols(&before), symbols(&after));

        let hash = |file: &SourceFile, table: &SymbolTable, path| {
            hash_symbol(file, resolved(file, table, path))
        };

        assert_eq!(
            hash(&before, &before_table, "Config.url"),
            hash(&after, &after_table, "Config.url"),
        );
        assert_eq!(
            hash(&before, &before_table, "Config.host"),
            hash(&after, &after_table, "Config.host"),
        );
        assert_ne!(
            hash(&before, &before_table, "add"),
            hash(&after, &after_table, "add"),
            "the edit is inside `add`",
        );
    }

    #[test]
    fn python_init_attribute_inside_if_block_is_a_symbol() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        assert_eq!(
            text_of(&file, resolved(&file, &table, "Config.secure")),
            "self.secure = True",
            "assigned inside an `if`",
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "Config.retries")),
            "self.retries = 3",
            "assigned inside an `else` and a `for`",
        );
    }

    #[test]
    fn python_init_attribute_assigned_twice_names_the_first_assignment() {
        let file =
            parse("class A:\n    def __init__(self):\n        self.x = 1\n        self.x = 2\n");
        let table = symbols(&file);

        assert_eq!(paths(&table), ["A", "A.__init__", "A.x"]);
        assert_eq!(text_of(&file, resolved(&file, &table, "A.x")), "self.x = 1");
    }

    #[test]
    fn python_class_attribute_keeps_the_name_over_an_init_assignment() {
        let file = parse("class A:\n    x = 0\n    def __init__(self):\n        self.x = 1\n");
        let table = symbols(&file);

        assert_eq!(text_of(&file, resolved(&file, &table, "A.x")), "x = 0");
    }

    #[test]
    fn python_attributes_outside_init_or_on_other_receivers_are_not_symbols() {
        let file = parse(
            "class A:\n    def __init__(self, other):\n        other.x = 1\n        self.a.b = 2\n        \
             self.c += 3\n    def reset(self):\n        self.y = 0\n",
        );

        assert_eq!(
            paths(&symbols(&file)),
            ["A", "A.__init__", "A.reset"],
            "only `self.<name> = …` inside `__init__` counts",
        );
    }

    #[test]
    fn python_decorator_sits_outside_the_definition() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        let config = text_of(&file, resolved(&file, &table, "Config"));
        let url = text_of(&file, resolved(&file, &table, "Config.url"));

        assert!(
            config.starts_with("class Config"),
            "`@dataclass` sits beside `Config`: {config:?}"
        );
        assert!(
            url.starts_with("def url"),
            "`@property` sits beside `url`: {url:?}"
        );
    }

    #[test]
    fn python_docstring_is_inside_the_function_hash() {
        let before = parse("def f():\n    \"\"\"Old.\"\"\"\n    return 1\n");
        let after = parse("def f():\n    \"\"\"New.\"\"\"\n    return 1\n");

        let hash = |file: &SourceFile| {
            let table = symbols(file);
            hash_symbol(file, resolved(file, &table, "f"))
        };

        assert_ne!(hash(&before), hash(&after));
    }

    #[test]
    fn python_docstring_and_hash_comment_links_are_extracted() {
        let file = parse(FIXTURE);

        let texts = region_texts(&file);

        assert!(
            texts
                .iter()
                .any(|text| text.contains("[guide](guide.md)") && text.starts_with("\"\"\"")),
            "the module docstring: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text == "\"\"\"Adds two numbers.\"\"\""),
            "a function docstring: {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text == " [host](host.md)\n"),
            "a `#` comment, its body after the marker through the end of its line: {texts:?}"
        );
    }

    #[test]
    fn python_docstring_follows_leading_comments() {
        let file = parse("# header\n\"\"\"Doc.\"\"\"\n");

        assert_eq!(region_texts(&file), [" header\n", "\"\"\"Doc.\"\"\""]);
    }

    #[test]
    fn python_string_literal_that_looks_like_a_link_is_not_a_region() {
        let file =
            parse("s = \"[a](b.py#c)\"\n\ndef f():\n    x = 1\n    \"[not](a-docstring.md)\"\n");

        assert_eq!(region_texts(&file), Vec::<&str>::new());
    }

    #[test]
    fn python_files_use_the_python_cartridge() {
        let languages = crate::read::cartridge::Languages::builtin();

        for name in ["tools/build.py", "stubs/app.pyi"] {
            assert_eq!(
                languages
                    .for_path(Path::new(name))
                    .map(|cartridge| cartridge.id().as_str()),
                Some("python"),
                "{name}",
            );
        }
    }
}
