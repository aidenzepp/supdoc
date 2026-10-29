//! The v1 hash scheme: SHA-256 of the exact bytes of a symbol's node, or of the whole file
//! (03 R6, R7). Nothing is stripped or normalized. Changing this rule bumps the lockfile version.

use crate::hash::Hash;
use crate::read::cartridge::{SourceFile, Symbol};

/// The hash of `symbol`'s bytes in `file`: exactly `file.text()[symbol.range]`.
///
/// Infallible because a symbol's range comes from `file`'s own tree. A range that doesn't fit
/// `file` (a symbol from some other file) hashes as no bytes rather than panicking.
#[must_use]
pub fn hash_symbol(file: &SourceFile, symbol: &Symbol) -> Hash {
    let bytes = file
        .text()
        .as_bytes()
        .get(symbol.range.clone())
        .unwrap_or_default();
    Hash::digest(bytes)
}

/// The hash of every byte of `file`.
#[must_use]
pub fn hash_file(file: &SourceFile) -> Hash {
    Hash::digest(file.text().as_bytes())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::Cartridge;

    fn rust() -> &'static dyn Cartridge {
        crate::cartridges::rust::cartridge().expect("the built-in Rust queries should compile")
    }

    fn parse(text: &str) -> SourceFile {
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), rust()).expect("tree-sitter should return a tree")
    }

    fn symbol_hash(text: &str, path: &str) -> Hash {
        let file = parse(text);
        let table = file.language().symbols(&file);
        let query: SymbolPath = path.parse().expect("the fixture symbol should be valid");
        let symbol = table
            .resolve(file.path(), &query)
            .unwrap_or_else(|error| panic!("`{path}` should resolve in {text:?}: {error}"));
        hash_symbol(&file, symbol)
    }

    #[test]
    fn whole_file_hash_is_sha256_of_every_byte() {
        let text = "fn add() {}\n\n// trailing\n";

        assert_eq!(hash_file(&parse(text)), Hash::digest(text.as_bytes()));
    }

    #[test]
    fn whitespace_only_edit_inside_symbol_changes_hash() {
        let before = symbol_hash("fn add(a: u8) -> u8 { a }\n", "add");
        let after = symbol_hash("fn add(a: u8) -> u8 {  a }\n", "add");

        assert_ne!(
            before, after,
            "R6 keeps whitespace inside the symbol's range"
        );
    }

    #[test]
    fn comment_edit_inside_symbol_changes_hash() {
        let before = symbol_hash("fn add() {\n    // one\n}\n", "add");
        let after = symbol_hash("fn add() {\n    // two\n}\n", "add");

        assert_ne!(before, after, "R6 keeps comments inside the symbol's range");
    }

    #[test]
    fn file_with_syntax_error_hashes_and_reports_it() {
        let text = "fn add(a: u8 -> u8 { a }\n\nfn sub() {}\n";
        let file = parse(text);

        assert!(
            file.has_syntax_errors(),
            "an unclosed parameter list is a syntax error"
        );
        assert_eq!(hash_file(&file), Hash::digest(text.as_bytes()));
        assert_eq!(
            symbol_hash(text, "sub"),
            Hash::digest(b"fn sub() {}"),
            "symbols past the error are still found and hashed",
        );
    }

    #[test]
    fn range_outside_the_file_hashes_as_no_bytes() {
        let file = parse("fn add() {}\n");
        let foreign = Symbol {
            path: "elsewhere"
                .parse()
                .expect("the fixture symbol should be valid"),
            aliases: Vec::new(),
            range: 100..200,
        };

        assert_eq!(hash_symbol(&file, &foreign), Hash::digest(b""));
    }
}
