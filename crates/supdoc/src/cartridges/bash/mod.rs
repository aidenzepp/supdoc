//! Bash: pure data over the query contract (03 R3, R4, R9).
//!
//! Symbols come from `tags.scm` and link regions from `injections.scm`, both beside this file.
//! The golden symbol list in the tests is the public contract: users type these paths into docs.

use std::sync::LazyLock;

use crate::read::cartridge::LanguageId;
use crate::read::query::{QueryCartridge, QueryFileError};

/// Symbols: functions, plus top-level variables.
const TAGS: &str = include_str!("tags.scm");

/// Link regions: every comment, as `markdown_inline`.
const INJECTIONS: &str = include_str!("injections.scm");

/// The cartridge, compiled the first time it is used.
static BASH: LazyLock<Result<QueryCartridge, QueryFileError>> = LazyLock::new(|| {
    QueryCartridge::new(
        LanguageId::new("bash"),
        &["sh", "bash"],
        tree_sitter_bash::LANGUAGE.into(),
        TAGS,
        INJECTIONS,
    )
});

/// The Bash cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for a built-in query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static QueryCartridge, &'static QueryFileError> {
    BASH.as_ref()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{SourceFile, Symbol, SymbolTable};
    use crate::read::hash::{hash_file, hash_symbol};

    /// Every kind of definition the cartridge names, once, plus things it must not name.
    const FIXTURE: &str = r#"#!/usr/bin/env bash
# A fixture.
readonly VERSION="1.0"
export PREFIX=/usr/local
declare -r MODE=fast
count=0

# Prints a greeting.
greet() {
  local name="$1"
  echo "hello, $name"
}

function build {
  helper() {
    echo helping
  }
  local target=all
  make "$target"
}

cleanup() { rm -rf "$TMP"; }
"#;

    fn parse(text: &str) -> SourceFile {
        let bash = cartridge().expect("the built-in Bash queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/scripts/run.sh"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), bash).expect("tree-sitter should return a tree")
    }

    fn symbols(file: &SourceFile) -> SymbolTable {
        file.language().symbols(file)
    }

    fn paths(table: &SymbolTable) -> Vec<String> {
        table.iter().map(|symbol| symbol.path.to_string()).collect()
    }

    fn resolved<'t>(file: &SourceFile, table: &'t SymbolTable, raw: &str) -> &'t Symbol {
        let query = raw.parse().expect("the fixture symbol should be valid");
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
    fn bash_exposes_documented_symbols() {
        let file = parse(FIXTURE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "VERSION",
                "PREFIX",
                "MODE",
                "count",
                "greet",
                "build",
                "build.helper",
                "cleanup",
            ],
        );
    }

    #[test]
    fn bash_hashes_match_pinned_values() {
        let file = parse(FIXTURE);
        let table = symbols(&file);
        let pinned: [(&str, &str); 4] = [
            (
                "VERSION",
                "6b6e0ff6e167c61db80698096f4a4a58f804604a4c40f4501aae859e84da875f",
            ),
            (
                "greet",
                "8f4f1edfded1a1529bc6b3cf51f9801bdb3fc3f2b328f7a819bfe6009986abc8",
            ),
            (
                "build",
                "93f849421bd39febc6b637be37ed8c81d30bfe11008de78d5949898dbd48249a",
            ),
            (
                "build.helper",
                "e4aa1e0376d359bc4a28f9ed1f17582e8dae894f63c80e0d127cf967bb89229f",
            ),
        ];

        for (path, hash) in pinned {
            assert_eq!(
                hash_symbol(&file, resolved(&file, &table, path)).to_string(),
                hash,
                "a grammar upgrade moved `{path}`'s node boundaries (02 L8)"
            );
        }
        assert_eq!(
            hash_file(&file).to_string(),
            "0509d3d24229242917f36acff0f5fd9d82da305577cb6aed882076e00750c039",
            "the whole-file hash changed"
        );
    }

    #[test]
    fn bash_symbol_range_matches_tree_sitter_node() {
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
            text_of(&file, resolved(&file, &table, "VERSION")),
            "VERSION=\"1.0\"",
            "a variable's node is the assignment, without `readonly`",
        );
    }

    #[test]
    fn bash_edit_outside_symbol_keeps_hash() {
        let before = parse(FIXTURE);
        let after = parse(&FIXTURE.replace("echo helping", "echo still helping"));
        let (before_table, after_table) = (symbols(&before), symbols(&after));

        let hash = |file: &SourceFile, table: &SymbolTable, path| {
            hash_symbol(file, resolved(file, table, path))
        };

        assert_eq!(
            hash(&before, &before_table, "greet"),
            hash(&after, &after_table, "greet"),
        );
        assert_ne!(
            hash(&before, &before_table, "build.helper"),
            hash(&after, &after_table, "build.helper"),
            "the edit is inside `build.helper`",
        );
        assert_ne!(
            hash(&before, &before_table, "build"),
            hash(&after, &after_table, "build"),
            "an inner function is part of its parent's bytes",
        );
    }

    #[test]
    fn bash_function_keyword_and_parentheses_forms_are_alike() {
        let file = parse("function a { :; }\nb() { :; }\nfunction c() { :; }\n");

        assert_eq!(paths(&symbols(&file)), ["a", "b", "c"]);
    }

    #[test]
    fn bash_locals_inside_a_function_are_not_symbols() {
        let file = parse("f() {\n  local x=1\n  y=2\n}\n");

        assert_eq!(paths(&symbols(&file)), ["f"]);
    }

    #[test]
    fn bash_comments_are_link_regions_and_strings_are_not() {
        let file = parse(
            "# [a](a.md)\n#!/bin/sh\necho \"[b](b.md)\" # [c](c.md)\ncat <<EOF\n[d](d.md)\nEOF\n",
        );

        assert_eq!(
            file.language().markdown_regions(&file).len(),
            3,
            "the two comments and the shebang, not the string or heredoc",
        );
    }
}
