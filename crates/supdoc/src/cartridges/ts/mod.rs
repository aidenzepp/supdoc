//! TypeScript: pure data on the query contract (03 R3, R4).
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
static TYPESCRIPT: LazyLock<Result<QueryCartridge, QueryFileError>> = LazyLock::new(|| {
    QueryCartridge::new(
        LanguageId::new("typescript"),
        &["ts", "mts", "cts"],
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        TAGS,
        INJECTIONS,
    )
});

/// The TypeScript cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for a built-in query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static QueryCartridge, &'static QueryFileError> {
    TYPESCRIPT.as_ref()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Cartridge, SourceFile, Symbol, SymbolTable};
    use crate::read::hash::{hash_file, hash_symbol};

    /// Every kind of declaration the cartridge names, once.
    const FIXTURE: &str = r#"// A fixture.
import { readFile } from "fs";

/** Adds two numbers. */
export function add(a: number, b: number): number {
  return a + b;
}

export const PORT = 8080;

const double = (n: number) => n * 2;

export interface Config {
  port: number;
  validate(): boolean;
}

export type Id = string | number;

export enum Mode {
  Fast,
  Slow = 2,
}

export abstract class Shape {
  abstract area(): number;
}

export class RingBuffer<T> {
  static CAPACITY = 8;
  private items: T[] = [];

  constructor(size: number) {
    const local = size;
  }

  push(item: T): void {
    this.items.push(item);
  }
}

namespace util {
  export const NAME = "util";
  export function slug(s: string): string {
    return s;
  }
}
"#;

    fn parse(text: &str) -> SourceFile {
        let ts = cartridge().expect("the built-in TypeScript queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/lib.ts"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), ts).expect("tree-sitter should return a tree")
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
            .unwrap_or_else(|error| panic!("`{raw}` should resolve: {error}"))
    }

    fn text_of<'f>(file: &'f SourceFile, symbol: &Symbol) -> &'f str {
        file.text()
            .get(symbol.range.clone())
            .expect("a symbol's range should be valid for its file")
    }

    #[test]
    fn ts_exposes_documented_symbols() {
        let file = parse(FIXTURE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "add",
                "PORT",
                "double",
                "Config",
                "Config.port",
                "Config.validate",
                "Id",
                "Mode",
                "Mode.Fast",
                "Mode.Slow",
                "Shape",
                "Shape.area",
                "RingBuffer",
                "RingBuffer.CAPACITY",
                "RingBuffer.items",
                "RingBuffer.constructor",
                "RingBuffer.push",
                "util",
                "util.NAME",
                "util.slug",
            ],
        );
    }

    #[test]
    fn ts_hashes_match_pinned_values() {
        let file = parse(FIXTURE);
        let table = symbols(&file);
        let pinned: [(&str, &str); 8] = [
            (
                "add",
                "e56ebaf1bdbd2d0f7a32a07c1718792fc2134f4e10d28f45e4f34d3fb70d7520",
            ),
            (
                "PORT",
                "80e9153d3af16992920039553684d0ca1944e334902f598dbcc68df7ab97adff",
            ),
            (
                "Config",
                "db62d8efedc2b691917dbb8acc5daccb9e19f5e798dce49f0fd44cb0c759c40d",
            ),
            (
                "Config.validate",
                "395fe889375347a6b6abd8c648c0eb2b4ad687ea2d47c8cbd5bfb00cca065d0e",
            ),
            (
                "Mode.Fast",
                "6c582b62e0e5aa05647388bd045f2e3e5e7f51f479d00d9df592634c8088a22b",
            ),
            (
                "RingBuffer.push",
                "587340bb2d168cca686383797d71eb615904a7e8062e68612cb2b2e3ac1d58fb",
            ),
            (
                "util",
                "ec7c17dd4ef14ab000f2846a26dd6983de1fe11df66b1dfa60b50d8ffad30894",
            ),
            (
                "util.slug",
                "626882c3e874ca2669bed5b8045c4df8c328e3d68315afe14f6f077ff2c48a82",
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
            "a1dc2945fa95b1beace1bca7c5c452fe34362ec788d3159fcbaedd4ae323d81d",
            "the whole-file hash changed"
        );
    }

    #[test]
    fn ts_symbol_range_matches_tree_sitter_node() {
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
            text_of(&file, resolved(&file, &table, "PORT")),
            "PORT = 8080",
            "a binding's node is its declarator, without `export const`",
        );
    }

    #[test]
    fn ts_edit_outside_symbol_keeps_hash() {
        let before = parse(FIXTURE);
        let after = parse(&FIXTURE.replace("this.items.push(item)", "this.items.unshift(item)"));
        let (before_table, after_table) = (symbols(&before), symbols(&after));

        let hash = |file: &SourceFile, table: &SymbolTable, path| {
            hash_symbol(file, resolved(file, table, path))
        };

        assert_eq!(
            hash(&before, &before_table, "add"),
            hash(&after, &after_table, "add")
        );
        assert_eq!(
            hash(&before, &before_table, "RingBuffer.constructor"),
            hash(&after, &after_table, "RingBuffer.constructor"),
        );
        assert_ne!(
            hash(&before, &before_table, "RingBuffer.push"),
            hash(&after, &after_table, "RingBuffer.push"),
            "the edit is inside `RingBuffer.push`",
        );
    }

    #[test]
    fn ts_block_comment_link_is_extracted() {
        let file = parse("// [a](a.md)\n/** [b](b.md) */\n/* [c](c.md) */\nfunction f() {}\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 3);
    }

    #[test]
    fn ts_string_literal_is_not_a_link_region() {
        let file = parse("const s = \"[a](b.ts#c)\";\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 0);
    }

    #[test]
    fn ts_locals_inside_functions_are_not_symbols() {
        let file = parse("function outer() {\n  const inner = 1;\n}\n");

        assert_eq!(paths(&symbols(&file)), ["outer"]);
    }

    #[test]
    fn ts_extensions_cover_module_variants() {
        let ts = cartridge().expect("the built-in TypeScript queries should compile");

        assert_eq!(ts.extensions(), ["ts", "mts", "cts"]);
    }
}
