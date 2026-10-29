//! Rust, the reference cartridge: pure data that proves the query contract (03 R3–R5).
//!
//! Symbols come from `tags.scm` and link regions from `injections.scm`, both beside this file.
//! The golden symbol list in the tests is the public contract: users type these paths into docs.

use std::sync::LazyLock;

use crate::read::cartridge::LanguageId;
use crate::read::query::{QueryCartridge, QueryFileError};

/// Symbols: `@definition.*`, `@name` and the `@supdoc.scope*` extensions.
const TAGS: &str = include_str!("tags.scm");

/// Link regions: every comment, as `markdown_inline`.
const INJECTIONS: &str = include_str!("injections.scm");

/// The cartridge, compiled the first time it is used.
static RUST: LazyLock<Result<QueryCartridge, QueryFileError>> = LazyLock::new(|| {
    QueryCartridge::new(
        LanguageId::new("rust"),
        &["rs"],
        tree_sitter_rust::LANGUAGE.into(),
        TAGS,
        INJECTIONS,
    )
});

/// The Rust cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for a built-in query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static QueryCartridge, &'static QueryFileError> {
    RUST.as_ref()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::hash::{Hash, HashSettings};
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{SourceFile, Symbol, SymbolTable};
    use crate::read::error::ResolveError;
    use crate::read::hash::{hash_file, hash_symbol};

    /// Every kind of item the cartridge names, once.
    const FIXTURE: &str = r#"//! A fixture.
use std::fmt;

/// Adds two numbers.
#[inline]
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[derive(Debug)]
pub struct Config {
    pub port: u16,
}

impl Config {
    pub fn validate(&self) -> bool {
        self.port != 0
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.port)
    }
}

pub enum Mode {
    Fast,
    Slow { delay: u32 },
}

pub struct RingBuffer<T> {
    items: Vec<T>,
}

impl<T> RingBuffer<T> {
    pub const CAPACITY: usize = 8;

    pub fn new() -> Self {
        Self { items: Vec::new() }
    }
}

pub trait Shape {
    type Unit;
    fn area(&self) -> f64;
}

pub mod util {
    pub static NAME: &str = "util";

    macro_rules! twice {
        ($e:expr) => {
            $e * 2
        };
    }
}
"#;

    fn parse(text: &str) -> SourceFile {
        let rust = cartridge().expect("the built-in Rust queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), rust).expect("tree-sitter should return a tree")
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

    fn resolve<'t>(
        file: &SourceFile,
        table: &'t SymbolTable,
        raw: &str,
    ) -> Result<&'t Symbol, ResolveError> {
        table.resolve(file.path(), &query(raw))
    }

    fn resolved<'t>(file: &SourceFile, table: &'t SymbolTable, raw: &str) -> &'t Symbol {
        resolve(file, table, raw).unwrap_or_else(|error| panic!("`{raw}` should resolve: {error}"))
    }

    fn text_of<'f>(file: &'f SourceFile, symbol: &Symbol) -> &'f str {
        file.text()
            .get(symbol.range.clone())
            .expect("a symbol's range should be valid for its file")
    }

    #[test]
    fn rust_exposes_documented_symbols() {
        let file = parse(FIXTURE);

        assert_eq!(
            paths(&symbols(&file)),
            [
                "add",
                "Config",
                "Config.port",
                "Config.validate",
                "Config.Display.fmt",
                "Mode",
                "Mode.Fast",
                "Mode.Slow",
                "Mode.Slow.delay",
                "RingBuffer",
                "RingBuffer.items",
                "RingBuffer.CAPACITY",
                "RingBuffer.new",
                "Shape",
                "Shape.Unit",
                "Shape.area",
                "util",
                "util.NAME",
                "util.twice",
            ],
        );
    }

    #[test]
    fn rust_hashes_match_pinned_values() {
        let file = parse(FIXTURE);
        let table = symbols(&file);
        let pinned = [
            (
                "add",
                "54cd830f0b802e508232035fe7b0a2c0e3b89ce86eb9328032c154e12653cd2f",
            ),
            (
                "Config",
                "4df22ee701d4cf8f165c806553febcce901e4c6f432dad8f8d7b01bf37640b1c",
            ),
            (
                "Config.port",
                "9326c50bfa1489060595e26fdac1d3dd1cc10ea0f0b3208fa05ea813a859b26a",
            ),
            (
                "Config.validate",
                "3cbb48320e6bac83f67a173b4f2c40dc717c0071ced81c7cb2f5a1fc1aa2d4f7",
            ),
            (
                "Config.Display.fmt",
                "0b48fadf8f4e635d73fe781a0e652f1a3f99d04fe2cf1055e665d01c4f49f1f2",
            ),
            (
                "Mode.Fast",
                "6c582b62e0e5aa05647388bd045f2e3e5e7f51f479d00d9df592634c8088a22b",
            ),
            (
                "RingBuffer.new",
                "ed1fc2eef6cf837f6be91da54d5484d4a6d3996f7ce5f30f090bed9896c0e849",
            ),
            (
                "util",
                "27029d1cd9e9490923c50eb75dbd532c9e2333ed3f82b677a66038920b0f711d",
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
            "6329bcd6afe6c766f02e3289df8aee117259a968938ac6866492a98989270a44",
            "the whole-file hash changed"
        );
    }

    #[test]
    fn rust_symbol_range_matches_tree_sitter_node() {
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
            "an enum variant's node is just the variant",
        );
        assert_eq!(
            text_of(&file, resolved(&file, &table, "Config.port")),
            "pub port: u16"
        );
    }

    #[test]
    fn rust_edit_outside_symbol_keeps_hash() {
        let before = parse(FIXTURE);
        let edited = FIXTURE.replace("self.port != 0", "self.port > 1024");
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
            hash(&before, &before_table, "Config.Display.fmt"),
            hash(&after, &after_table, "Config.Display.fmt"),
        );
        assert_ne!(
            hash(&before, &before_table, "Config.validate"),
            hash(&after, &after_table, "Config.validate"),
            "the edit is inside `Config.validate`",
        );
    }

    #[test]
    fn rust_doc_comment_and_attribute_sit_outside_the_item() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        let add = text_of(&file, resolved(&file, &table, "add"));
        let config = text_of(&file, resolved(&file, &table, "Config"));

        assert!(
            add.starts_with("pub fn add("),
            "`///` and `#[inline]` sit beside `add`: {add:?}"
        );
        assert!(
            config.starts_with("pub struct Config"),
            "`#[derive]` sits beside `Config`: {config:?}"
        );
    }

    #[test]
    fn rust_line_and_block_comments_are_link_regions() {
        let file = parse("/// [a](a.md)\n// [b](b.md)\n/* [c](c.md) */\nfn f() {}\n");

        assert_eq!(file.language().markdown_regions(&file).len(), 3);
    }

    #[test]
    fn rust_generic_impl_method_resolves_without_generics() {
        let file = parse(FIXTURE);
        let table = symbols(&file);

        let new = resolved(&file, &table, "RingBuffer.new");

        assert!(text_of(&file, new).starts_with("pub fn new()"));
        assert_eq!(
            resolve(&file, &table, "new").map(|symbol| symbol.path.to_string()),
            Ok("RingBuffer.new".to_owned()),
            "the one-segment shorthand works when `new` is unique",
        );
    }

    #[test]
    fn rust_trait_and_inherent_method_have_distinct_paths() {
        let file = parse(
            "struct Config;\n\
             impl Config { fn fmt(&self) {} }\n\
             impl std::fmt::Display for Config {\n\
                 fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n\
             }\n",
        );
        let table = symbols(&file);

        let inherent = resolved(&file, &table, "Config.fmt");
        let display = resolved(&file, &table, "Config.Display.fmt");

        assert!(
            text_of(&file, inherent).starts_with("fn fmt(&self)"),
            "{inherent:?}"
        );
        assert!(text_of(&file, display).contains("Formatter"), "{display:?}");
        assert!(
            matches!(
                resolve(&file, &table, "fmt"),
                Err(ResolveError::Ambiguous { .. })
            ),
            "two methods are named `fmt`",
        );
    }

    #[test]
    fn rust_type_path_does_not_fall_back_to_trait_method() {
        let file = parse(
            "struct Config;\n\
             impl std::fmt::Display for Config {\n\
                 fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n\
             }\n",
        );
        let table = symbols(&file);

        let error = resolve(&file, &table, "Config.fmt")
            .expect_err("`Config.fmt` names only an inherent method, and there is none");

        assert!(
            matches!(
                &error,
                ResolveError::NotFound { nearest, .. }
                    if nearest.contains(&query("Config.Display.fmt"))
            ),
            "not found, suggesting the trait method: {error:?}",
        );
    }

    #[test]
    fn rust_impls_differing_by_type_arguments_resolve_by_full_type() {
        let file = parse(
            "struct Foo<T>(T);\n\
             impl Foo<u8> { fn new() -> Self { Foo(0) } }\n\
             impl Foo<u16> { fn new() -> Self { Foo(1) } }\n",
        );
        let table = symbols(&file);

        let narrow = resolved(&file, &table, "Foo<u8>.new");
        let wide = resolved(&file, &table, "Foo<u16>.new");

        assert!(text_of(&file, narrow).contains("Foo(0)"), "{narrow:?}");
        assert!(text_of(&file, wide).contains("Foo(1)"), "{wide:?}");
        assert!(
            matches!(
                resolve(&file, &table, "Foo.new"),
                Err(ResolveError::Ambiguous { ref candidates, .. })
                    if candidates == &[query("Foo.new"), query("Foo.new")]
            ),
            "plain `Foo.new` matches both impls",
        );
    }

    #[test]
    fn rust_trait_impls_differing_by_trait_arguments_resolve_by_full_trait() {
        let file = parse(
            "struct Foo;\n\
             impl From<u8> for Foo { fn from(_: u8) -> Self { Foo } }\n\
             impl From<u16> for Foo { fn from(_: u16) -> Self { Foo } }\n",
        );
        let table = symbols(&file);

        let narrow = resolved(&file, &table, "Foo.From<u8>.from");

        assert!(text_of(&file, narrow).contains("u8"), "{narrow:?}");
        assert!(matches!(
            resolve(&file, &table, "Foo.From.from"),
            Err(ResolveError::Ambiguous { .. })
        ));
    }

    #[test]
    fn rust_methods_of_an_unnameable_impl_are_not_free_functions() {
        let file = parse(
            "trait Sum { fn sum(&self) -> u8; }\nimpl Sum for [u8] { fn sum(&self) -> u8 { 0 } }\n",
        );

        assert_eq!(paths(&symbols(&file)), ["Sum", "Sum.sum"]);
    }

    #[test]
    fn rust_nested_function_is_named_by_containment() {
        let file = parse("fn outer() {\n    fn inner() {}\n}\n");

        assert_eq!(paths(&symbols(&file)), ["outer", "outer.inner"]);
    }

    #[test]
    fn rust_hash_covers_exactly_the_item_bytes() {
        let file = parse("/// doc\nfn add() { 1 }\n");
        let table = symbols(&file);

        assert_eq!(
            hash_symbol(
                &file,
                resolved(&file, &table, "add"),
                HashSettings::default()
            ),
            Hash::digest(b"fn add() { 1 }"),
        );
    }
}
