//! TSX: pure data that names TypeScript-with-JSX symbols and finds links in comments (03 R3–R5).
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
static TSX: LazyLock<Result<QueryCartridge, QueryFileError>> = LazyLock::new(|| {
    QueryCartridge::new(
        LanguageId::new("tsx"),
        &["tsx"],
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        TAGS,
        INJECTIONS,
    )
});

/// The TSX cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for a built-in query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static QueryCartridge, &'static QueryFileError> {
    TSX.as_ref()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::hash::HashSettings;
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{SourceFile, Symbol, SymbolTable};
    use crate::read::hash::{hash_file, hash_symbol};

    /// Every kind of declaration the cartridge names, once.
    const FIXTURE: &str = r#"/** A fixture. */
import React from "react";

export interface Props {
  label: string;
  onPress(): void;
}

export type Id = string | number;

export enum Mode {
  Fast,
  Slow = 2,
}

/** Adds two numbers. */
export function add(a: number, b: number): number {
  return a + b;
}

export class Counter extends React.Component<Props> {
  count = 0;
  #secret = 1;
  static displayName = "Counter";

  increment(): void {
    this.count += 1;
  }

  render() {
    return <button>{this.props.label}</button>;
  }
}

export const Button = ({ label }: Props) => <button>{label}</button>;

export const MAX = 10;

abstract class Shape {
  abstract area(): number;
}

namespace util {
  export const NAME = "util";
  export function noop() {
    const local = 1;
    return local;
  }
}
"#;

    fn parse(text: &str) -> SourceFile {
        let tsx = cartridge().expect("the built-in TSX queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/app.tsx"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), tsx).expect("tree-sitter should return a tree")
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
    fn tsx_exposes_documented_symbols() {
        let file = parse(FIXTURE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "Props",
                "Props.label",
                "Props.onPress",
                "Id",
                "Mode",
                "Mode.Fast",
                "Mode.Slow",
                "add",
                "Counter",
                "Counter.count",
                "Counter.#secret",
                "Counter.displayName",
                "Counter.increment",
                "Counter.render",
                "Button",
                "MAX",
                "Shape",
                "Shape.area",
                "util",
                "util.NAME",
                "util.noop",
            ],
        );
    }

    #[test]
    fn tsx_hashes_match_pinned_values() {
        let file = parse(FIXTURE);
        let table = symbols(&file);
        let pinned = [
            (
                "Props",
                "8312054d2f6666886ae280a5c7c151e6449a4af5b06ef66e2009faf23a7e5d63",
            ),
            (
                "Id",
                "605d9efd9709a998f68408b5a6e1b1c977b35a5248e9cb0b6958f64daae90862",
            ),
            (
                "Mode.Slow",
                "55135f8ea0248890fd60067280fe9547a7d351d950e22b2f13dad57cd05b7566",
            ),
            (
                "add",
                "e56ebaf1bdbd2d0f7a32a07c1718792fc2134f4e10d28f45e4f34d3fb70d7520",
            ),
            (
                "Counter",
                "02e00ebe6519150a66e2710c9536466631e9eba81305d2b98e730a87387a2b19",
            ),
            (
                "Counter.increment",
                "8e28b8d391e9fb79efc456b26ac6508d7f7ab6f7d20acad017162aadd132a996",
            ),
            (
                "Counter.render",
                "b178d2600b6b05f4b8efa2d766074c907be5d1732de26a315c5ccdbc52edab97",
            ),
            (
                "Button",
                "c1950406558cb6d3759b70a5852b8fac93a9f77445b9da477a94e4ee3026b837",
            ),
            (
                "util.noop",
                "b0bf55ba2d9239980367115b13df7711dc7375406299183f7533afd75d353886",
            ),
        ];

        let actual: Vec<(&str, String)> = pinned
            .iter()
            .map(|(path, _)| {
                (
                    *path,
                    hash_symbol(
                        &file,
                        resolved(&file, &table, path),
                        HashSettings::default(),
                    )
                    .to_string(),
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
            hash_file(&file, HashSettings::default()).to_string(),
            "d952a54179b4af4a6d494ab0906bdf742b38716e97a2a6428b8fb0efd4bef14c",
            "the whole-file hash changed"
        );
    }

    #[test]
    fn tsx_symbol_range_matches_tree_sitter_node() {
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
            text_of(&file, resolved(&file, &table, "Mode.Fast")),
            "Fast",
            "an enum member's node is just the member",
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "Button")),
            "Button = ({ label }: Props) => <button>{label}</button>",
            "a variable's node is its declarator, `export const` sits beside it",
        );
    }

    #[test]
    fn tsx_edit_outside_symbol_keeps_hash() {
        let before = parse(FIXTURE);
        let edited = FIXTURE.replace("this.count += 1", "this.count += 2");
        let after = parse(&edited);
        let (before_table, after_table) = (symbols(&before), symbols(&after));

        let hash = |file: &SourceFile, table: &SymbolTable, path| {
            hash_symbol(file, resolved(file, table, path), HashSettings::default())
        };

        assert_eq!(
            hash(&before, &before_table, "add"),
            hash(&after, &after_table, "add")
        );
        assert_eq!(
            hash(&before, &before_table, "Counter.render"),
            hash(&after, &after_table, "Counter.render"),
        );
        assert_ne!(
            hash(&before, &before_table, "Counter.increment"),
            hash(&after, &after_table, "Counter.increment"),
            "the edit is inside `Counter.increment`",
        );
    }

    #[test]
    fn tsx_jsdoc_and_export_sit_outside_the_item() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        let add = text_of(&file, resolved(&file, &table, "add"));
        let counter = text_of(&file, resolved(&file, &table, "Counter"));

        assert!(
            add.starts_with("function add("),
            "the JSDoc comment and `export` sit beside `add`: {add:?}"
        );
        assert!(
            counter.starts_with("class Counter"),
            "`export` sits beside `Counter`: {counter:?}"
        );
    }

    #[test]
    fn tsx_line_block_and_jsx_comments_are_link_regions() {
        let file =
            parse("/** [a](a.md) */\n// [b](b.md)\nconst view = <div>{/* [c](c.md) */}</div>;\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 3);
    }

    #[test]
    fn tsx_strings_and_jsx_text_are_not_link_regions() {
        let file = parse("const s = \"[a](a.md)\";\nconst v = <p>[b](b.md)</p>;\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 0);
    }

    #[test]
    fn tsx_locals_inside_functions_are_not_symbols() {
        let file = parse("function f() {\n  const local = 1;\n  return local;\n}\n");

        assert_eq!(paths(&symbols(&file)), ["f"]);
    }
}
