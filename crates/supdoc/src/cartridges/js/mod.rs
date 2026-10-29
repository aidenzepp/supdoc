//! JavaScript, a data-only cartridge on its own grammar, not the TypeScript one (03 R1).
//!
//! Symbols come from `tags.scm` and link regions from `injections.scm`, both beside this file.
//! The golden symbol list in the tests is the public contract: users type these paths into docs.

use std::sync::LazyLock;

use crate::read::cartridge::LanguageId;
use crate::read::query::{QueryCartridge, QueryFileError};

/// Symbols: `@definition.*` and `@name`.
const TAGS: &str = include_str!("tags.scm");

/// Link regions: every comment, as `markdown_inline`.
const INJECTIONS: &str = include_str!("injections.scm");

/// The cartridge, compiled the first time it is used.
static JAVASCRIPT: LazyLock<Result<QueryCartridge, QueryFileError>> = LazyLock::new(|| {
    QueryCartridge::new(
        LanguageId::new("javascript"),
        &["js", "mjs", "cjs", "jsx"],
        tree_sitter_javascript::LANGUAGE.into(),
        TAGS,
        INJECTIONS,
    )
});

/// The JavaScript cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for a built-in query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static QueryCartridge, &'static QueryFileError> {
    JAVASCRIPT.as_ref()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::hash::HashSettings;
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Languages, SourceFile, Symbol, SymbolTable};
    use crate::read::hash::{hash_file, hash_symbol};

    /// Every kind of declaration the cartridge names, once.
    const FIXTURE: &str = r#"// A fixture.
import fs from "node:fs";

/** Adds two numbers. */
export function add(a, b) {
  const sum = a + b;
  return sum;
}

function* ids() {
  yield 1;
}

class Counter {
  step = 1;
  #count = 0;

  constructor(start) {
    this.#count = start;
  }

  increment() {
    this.#count += this.step;
  }

  #reset() {
    this.#count = 0;
  }
}

export const double = (n) => n * 2;
const LIMIT = 10;
var legacy = function () {};
let { skipped } = {};

const api = {
  timeout: 30,
  get() {
    return fs;
  },
  post: async (url) => url,
};

function outer() {
  function inner() {}
}
"#;

    fn parse(text: &str) -> SourceFile {
        let js = cartridge().expect("the built-in JavaScript queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/index.js"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), js).expect("tree-sitter should return a tree")
    }

    fn symbols(file: &SourceFile) -> SymbolTable {
        file.language().symbols(file)
    }

    fn paths(table: &SymbolTable) -> Vec<String> {
        table.iter().map(|symbol| symbol.path.to_string()).collect()
    }

    fn resolved<'t>(file: &SourceFile, table: &'t SymbolTable, raw: &str) -> &'t Symbol {
        let query: SymbolPath = raw.parse().expect("the fixture symbol should be valid");
        table
            .resolve(file.path(), &query)
            .unwrap_or_else(|error| panic!("`{raw}` should resolve: {error}"))
    }

    fn text_of<'f>(file: &'f SourceFile, symbol: &Symbol) -> &'f str {
        file.text()
            .get(symbol.range.clone())
            .expect("a symbol's range should be valid for its file")
    }

    #[test]
    fn javascript_exposes_documented_symbols() {
        let file = parse(FIXTURE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "add",
                "ids",
                "Counter",
                "Counter.step",
                "Counter.#count",
                "Counter.constructor",
                "Counter.increment",
                "Counter.#reset",
                "double",
                "LIMIT",
                "legacy",
                "api",
                "api.timeout",
                "api.get",
                "api.post",
                "outer",
                "outer.inner",
            ],
        );
    }

    #[test]
    fn javascript_hashes_match_pinned_values() {
        let file = parse(FIXTURE);
        let table = symbols(&file);
        let pinned = [
            (
                "add",
                "5148c48351a48e33195f9543013aa435d4a9d0920f8e2e7b62fce1f99160e1d0",
            ),
            (
                "Counter",
                "dd4fcec06abf88b1ed5490167f47a4033cfe830668ec6d84ed9da0c0917abae1",
            ),
            (
                "Counter.increment",
                "e58a8d59f9cb906f6478d43dc1260177d5589091cc8dd52113f071d8944766b8",
            ),
            (
                "Counter.step",
                "4c0c13828a696388e828d13abb99de5077596c420522a209850ebedcdb168e83",
            ),
            (
                "LIMIT",
                "a5ea39047eefc794477998d32416c9c1294911bf425045a8659144539a347936",
            ),
            (
                "api.get",
                "5c45846f67ddb2940f17cb382897edca81bcf07aeef083f56aa4cce627b2f967",
            ),
            (
                "api.timeout",
                "fab6f492d9389e1e26e53beb4f7641b50cf41eadfea9fb82e5569e1074eac7c5",
            ),
            (
                "outer.inner",
                "48dd2d8c74e3a66d39cd6ab78b788a34fe3954057a3a9398a3ccac2973d41af0",
            ),
        ];

        for (path, hash) in pinned {
            assert_eq!(
                hash_symbol(
                    &file,
                    resolved(&file, &table, path),
                    HashSettings::default()
                )
                .to_string(),
                hash,
                "a grammar upgrade moved `{path}`'s node boundaries (02 L8)"
            );
        }
        assert_eq!(
            hash_file(&file, HashSettings::default()).to_string(),
            "e53ae02c65b0ef81525d81f00824a0f2e9b038b0485b844a7739476cb1559268",
            "the whole-file hash changed"
        );
    }

    #[test]
    fn javascript_symbol_range_matches_tree_sitter_node() {
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
            text_of(&file, resolved(&file, &table, "Counter.step")),
            "step = 1"
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "LIMIT")),
            "LIMIT = 10",
            "a binding is its declarator, without the `const`",
        );
    }

    #[test]
    fn javascript_edit_outside_symbol_keeps_hash() {
        let before = parse(FIXTURE);
        let after = parse(&FIXTURE.replace("this.#count += this.step", "this.#count += 2"));
        let (before_table, after_table) = (symbols(&before), symbols(&after));

        let hash = |file: &SourceFile, table: &SymbolTable, path| {
            hash_symbol(file, resolved(file, table, path), HashSettings::default())
        };

        assert_eq!(
            hash(&before, &before_table, "add"),
            hash(&after, &after_table, "add")
        );
        assert_ne!(
            hash(&before, &before_table, "Counter.increment"),
            hash(&after, &after_table, "Counter.increment"),
            "the edit is inside `Counter.increment`",
        );
        assert_ne!(
            hash(&before, &before_table, "Counter"),
            hash(&after, &after_table, "Counter"),
            "a class includes its methods",
        );
    }

    #[test]
    fn javascript_doc_comment_sits_outside_the_declaration() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        let add = text_of(&file, resolved(&file, &table, "add"));

        assert!(
            add.starts_with("function add("),
            "the JSDoc block and `export` sit beside the function: {add:?}"
        );
    }

    #[test]
    fn javascript_line_and_block_comments_are_link_regions() {
        let file = parse("// [a](a.md)\n/** [b](b.md) */\n/* [c](c.md) */\nfunction f() {}\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 3);
    }

    #[test]
    fn javascript_string_literals_are_not_link_regions() {
        let file = parse("const s = \"[a](b.js#c)\";\nconst t = `[a](b.js#c)`;\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 0);
    }

    #[test]
    fn javascript_files_use_javascript_grammar() {
        let languages = Languages::builtin();

        for name in ["a.js", "a.mjs", "a.cjs", "a.jsx"] {
            let cartridge = languages
                .for_path(Path::new(name))
                .unwrap_or_else(|| panic!("{name} should have a cartridge"));
            assert_eq!(cartridge.id().as_str(), "javascript", "{name}");
            assert_eq!(cartridge.grammar().name(), Some("javascript"), "{name}");
        }
    }

    #[test]
    fn javascript_bindings_inside_functions_are_not_symbols() {
        let file = parse("function f() {\n  const local = 1;\n  return local;\n}\n");

        assert_eq!(paths(&symbols(&file)), ["f"]);
    }

    #[test]
    fn javascript_jsx_parses_with_the_javascript_grammar() {
        let file = parse("function App() {\n  return <div>{1}</div>;\n}\n");

        assert!(!file.tree().root_node().has_error());
        assert_eq!(paths(&symbols(&file)), ["App"]);
    }
}
