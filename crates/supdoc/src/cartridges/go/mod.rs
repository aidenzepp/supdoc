//! Go: pure data over the query contract (03 R3–R5), with methods named by their receiver.
//!
//! Symbols come from `tags.scm` and link regions from `injections.scm`, both beside this file.
//! The golden symbol list in the tests is the public contract: users type these paths into docs.

use std::sync::LazyLock;

use crate::read::cartridge::LanguageId;
use crate::read::query::{QueryCartridge, QueryFileError};

/// Symbols: `@definition.*`, `@name` and `@supdoc.scope.name` for receivers.
const TAGS: &str = include_str!("tags.scm");

/// Link regions: every comment, as `markdown_inline`.
const INJECTIONS: &str = include_str!("injections.scm");

/// The cartridge, compiled the first time it is used.
static GO: LazyLock<Result<QueryCartridge, QueryFileError>> = LazyLock::new(|| {
    QueryCartridge::new(
        LanguageId::new("go"),
        &["go"],
        tree_sitter_go::LANGUAGE.into(),
        TAGS,
        INJECTIONS,
    )
});

/// The Go cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for a built-in query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static QueryCartridge, &'static QueryFileError> {
    GO.as_ref()
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
    const FIXTURE: &str = r#"// Package config is a fixture.
package config

import "fmt"

const Version = "1"

const (
	ModeFast = iota
	ModeSlow
)

var Default = Config{Port: 80}

// Add adds two numbers.
func Add(a, b int) int {
	return a + b
}

// Config holds settings.
type Config struct {
	Port int
	Host string
}

func (c *Config) Validate() bool {
	return c.Port != 0
}

func (c Config) String() string {
	return fmt.Sprint(c.Port)
}

type Shape interface {
	Area() float64
}

type Alias = Config

type Ring[T any] struct {
	items []T
}

func (r *Ring[T]) Push(item T) {
	var local int
	r.items = append(r.items, item)
}
"#;

    fn parse(text: &str) -> SourceFile {
        let go = cartridge().expect("the built-in Go queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/config.go"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), go).expect("tree-sitter should return a tree")
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
    fn go_exposes_documented_symbols() {
        let file = parse(FIXTURE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "Version",
                "ModeFast",
                "ModeSlow",
                "Default",
                "Add",
                "Config",
                "Config.Port",
                "Config.Host",
                "Config.Validate",
                "Config.String",
                "Shape",
                "Shape.Area",
                "Alias",
                "Ring",
                "Ring.items",
                "Ring.Push",
            ],
        );
    }

    #[test]
    fn go_hashes_match_pinned_values() {
        let file = parse(FIXTURE);
        let table = symbols(&file);
        let pinned: [(&str, &str); 8] = [
            (
                "Add",
                "71ebd9cb7debeff0ebacc52c390ad19a0d0ae1b848857be29ba865ee2e3f1348",
            ),
            (
                "Config",
                "a737a51ac83b4f017c1310f973b1009099e73d086b7baca480beafe9a08fb0cc",
            ),
            (
                "Config.Port",
                "d814efd96cdace71fcf2cb18ef8916194f1dd3107ef1ab37fe479723937125bd",
            ),
            (
                "Config.Validate",
                "be683d4b922c72c40bc5405be46c29c5c3711760599348c482ce4ee9b85a792f",
            ),
            (
                "Shape.Area",
                "3a979029a9d1e33b5678c2d3097d57d6ef585eb3a4b3db0f76e0d2998a2ffbcd",
            ),
            (
                "Ring.Push",
                "ce5912860db0dda1e7b7769c3ef6a11483e96ced434a99b3ea07fac9cf2e631b",
            ),
            (
                "ModeSlow",
                "2cc3b0443dc3f16149346f8464d882cbe0cdd6bce41b713f447b2603a07988dd",
            ),
            (
                "Version",
                "95e8c1f1dd3ef9999a8544001a8b128c69e5f240b440337fcc70d426388a14f9",
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
            "3627b03fd4a8f1d285fb811e66831ac7daf345e10a98b15c37bad51fc633a6b2",
            "the whole-file hash changed"
        );
    }

    #[test]
    fn go_symbol_range_matches_tree_sitter_node() {
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
    }

    #[test]
    fn go_edit_outside_symbol_keeps_hash() {
        let before = parse(FIXTURE);
        let after = parse(&FIXTURE.replace("c.Port != 0", "c.Port > 1024"));
        let (before_table, after_table) = (symbols(&before), symbols(&after));

        let hash = |file: &SourceFile, table: &SymbolTable, path| {
            hash_symbol(file, resolved(file, table, path), HashSettings::default())
        };

        assert_eq!(
            hash(&before, &before_table, "Add"),
            hash(&after, &after_table, "Add")
        );
        assert_eq!(
            hash(&before, &before_table, "Config.String"),
            hash(&after, &after_table, "Config.String"),
        );
        assert_ne!(
            hash(&before, &before_table, "Config.Validate"),
            hash(&after, &after_table, "Config.Validate"),
            "the edit is inside `Config.Validate`",
        );
    }

    #[test]
    fn go_method_scoped_by_receiver() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        let pointer = text_of(&file, resolved(&file, &table, "Config.Validate"));
        let value = text_of(&file, resolved(&file, &table, "Config.String"));
        let generic = text_of(&file, resolved(&file, &table, "Ring.Push"));

        assert!(
            pointer.starts_with("func (c *Config) Validate()"),
            "{pointer:?}"
        );
        assert!(value.starts_with("func (c Config) String()"), "{value:?}");
        assert!(
            generic.starts_with("func (r *Ring[T]) Push("),
            "type arguments are stripped from the receiver: {generic:?}"
        );
    }

    #[test]
    fn go_doc_comment_sits_outside_the_declaration() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        let add = text_of(&file, resolved(&file, &table, "Add"));

        assert!(
            add.starts_with("func Add("),
            "`//` sits beside `Add`: {add:?}"
        );
    }

    #[test]
    fn go_function_local_declarations_are_not_symbols() {
        let file = parse(FIXTURE);

        assert!(
            !paths(&symbols(&file))
                .iter()
                .any(|path| path.contains("local")),
            "a local `var` is not addressable"
        );
    }

    #[test]
    fn go_comments_are_link_regions() {
        let file = parse("// [a](a.md)\n/* [b](b.md) */\nfunc f() {}\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 2);
    }

    #[test]
    fn go_string_literal_is_not_a_link_region() {
        let file = parse("package p\nvar s = \"[a](b.go#c)\"\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 0);
    }
}
