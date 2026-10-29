//! The v1 hash scheme: SHA-256 of the bytes of a symbol's node, or of the whole file (03 R6, R7).
//!
//! Nothing is stripped: whitespace and comments inside the range count. The one exception is line
//! endings: under the default `[features.hash] line-endings = "normalize"` (01 D10), each CR
//! directly before an LF is dropped before hashing, so CRLF and LF checkouts agree. `"exact"`
//! hashes every byte. Ranges themselves are never changed; only the bytes fed to SHA-256 are.

use std::ops::Range;

use sha2::{Digest, Sha256};

use crate::hash::{Hash, HashSettings, LineEndings};
use crate::read::cartridge::{SourceFile, Symbol};

/// The hash of `symbol`'s bytes in `file`, `file.text()[symbol.range]`, under `settings`.
///
/// Infallible because a symbol's range comes from `file`'s own tree. A range that doesn't fit
/// `file` (a symbol from some other file) hashes as no bytes rather than panicking.
#[must_use]
pub fn hash_symbol(file: &SourceFile, symbol: &Symbol, settings: HashSettings) -> Hash {
    digest_range(file.text().as_bytes(), symbol.range.clone(), settings)
}

/// The hash of every byte of `file`, under `settings`.
#[must_use]
pub fn hash_file(file: &SourceFile, settings: HashSettings) -> Hash {
    let text = file.text().as_bytes();
    digest_range(text, 0..text.len(), settings)
}

/// SHA-256 of `text[range]` under `settings`, or of no bytes when `range` doesn't fit `text`.
///
/// Under [`LineEndings::Normalize`], a CR is dropped when the byte after it *in `text`* is an LF,
/// so a CR at the range's end whose LF lies just outside it is still dropped (B.4). The bytes
/// between dropped CRs are fed to the hasher in runs, so nothing is copied.
fn digest_range(text: &[u8], range: Range<usize>, settings: HashSettings) -> Hash {
    let Some(bytes) = text.get(range.clone()) else {
        return Hash::digest(b"");
    };
    match settings.line_endings {
        LineEndings::Exact => Hash::digest(bytes),
        LineEndings::Normalize => {
            let after = text.get(range.end);
            let mut hasher = Sha256::new();
            let mut run_start = 0;
            let pairs = bytes.iter().zip(bytes.iter().skip(1).chain(after));
            for (index, (&byte, &next)) in pairs.enumerate() {
                if byte == b'\r' && next == b'\n' {
                    hasher.update(bytes.get(run_start..index).unwrap_or_default());
                    run_start = index + 1;
                }
            }
            hasher.update(bytes.get(run_start..).unwrap_or_default());
            Hash::from_bytes(hasher.finalize().into())
        }
    }
}

#[cfg(test)]
mod crlf;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::Cartridge;

    /// The default: each CR directly before an LF is dropped.
    const NORMALIZE: HashSettings = HashSettings {
        line_endings: LineEndings::Normalize,
    };

    /// Every byte counts.
    const EXACT: HashSettings = HashSettings {
        line_endings: LineEndings::Exact,
    };

    fn rust() -> &'static dyn Cartridge {
        crate::cartridges::rust::cartridge().expect("the built-in Rust queries should compile")
    }

    fn parse(text: &str) -> SourceFile {
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), rust()).expect("tree-sitter should return a tree")
    }

    fn symbol_hash(text: &str, path: &str) -> Hash {
        symbol_hash_with(text, path, NORMALIZE)
    }

    fn symbol_hash_with(text: &str, path: &str, settings: HashSettings) -> Hash {
        let file = parse(text);
        let table = file.language().symbols(&file);
        let query: SymbolPath = path.parse().expect("the fixture symbol should be valid");
        let symbol = table
            .resolve(file.path(), &query)
            .unwrap_or_else(|error| panic!("`{path}` should resolve in {text:?}: {error}"));
        hash_symbol(&file, symbol, settings)
    }

    #[test]
    fn whole_file_hash_is_sha256_of_every_byte() {
        let text = "fn add() {}\n\n// trailing\n";

        assert_eq!(
            hash_file(&parse(text), NORMALIZE),
            Hash::digest(text.as_bytes())
        );
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
        assert_eq!(hash_file(&file, NORMALIZE), Hash::digest(text.as_bytes()));
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

        assert_eq!(hash_symbol(&file, &foreign, NORMALIZE), Hash::digest(b""));
    }

    /// A symbol over an explicit byte range of `file`, as a grammar would report it.
    fn range_symbol(range: Range<usize>) -> Symbol {
        Symbol {
            path: "fixture"
                .parse()
                .expect("the fixture symbol should be valid"),
            aliases: Vec::new(),
            range,
        }
    }

    #[test]
    fn crlf_symbol_hashes_like_lf_under_normalize() {
        let lf = "fn add(a: u8) -> u8 {\n    a\n}\n";
        let crlf = lf.replace('\n', "\r\n");

        assert_eq!(
            symbol_hash_with(&crlf, "add", NORMALIZE),
            symbol_hash_with(lf, "add", NORMALIZE),
        );
        assert_eq!(
            hash_file(&parse(&crlf), NORMALIZE),
            hash_file(&parse(lf), NORMALIZE),
        );
    }

    #[test]
    fn crlf_differs_from_lf_under_exact() {
        let lf = "fn add(a: u8) -> u8 {\n    a\n}\n";
        let crlf = lf.replace('\n', "\r\n");

        assert_ne!(
            symbol_hash_with(&crlf, "add", EXACT),
            symbol_hash_with(lf, "add", EXACT),
        );
        assert_eq!(
            hash_file(&parse(&crlf), EXACT),
            Hash::digest(crlf.as_bytes()),
            "exact hashes every byte, CRs included",
        );
    }

    #[test]
    fn lone_cr_is_kept() {
        let text = "fn a() {}\r// b\n";

        assert_eq!(
            hash_file(&parse(text), NORMALIZE),
            Hash::digest(text.as_bytes())
        );
        assert_ne!(
            hash_file(&parse(text), NORMALIZE),
            Hash::digest(b"fn a() {}\n// b\n"),
            "a lone CR must not hash like an LF (B-Q2)",
        );
    }

    #[test]
    fn cr_cr_lf_becomes_cr_lf() {
        let file = parse("fn a() {}\r\r\n");

        assert_eq!(
            hash_file(&file, NORMALIZE),
            Hash::digest(b"fn a() {}\r\n"),
            "only the CR directly before the LF is dropped, in a single pass",
        );
    }

    #[test]
    fn cr_at_range_end_before_lf_outside_range_is_dropped() {
        let file = parse("ab\r\ncd");

        assert_eq!(
            hash_symbol(&file, &range_symbol(0..3), NORMALIZE),
            Hash::digest(b"ab"),
            "the look-ahead reads the file, not the range",
        );
        assert_eq!(
            hash_symbol(&file, &range_symbol(3..6), NORMALIZE),
            Hash::digest(b"\ncd"),
            "an LF whose CR lies before the range is kept",
        );
        assert_eq!(
            hash_symbol(&file, &range_symbol(0..3), EXACT),
            Hash::digest(b"ab\r"),
        );
    }

    #[test]
    fn lf_only_text_hashes_identically_under_both() {
        let text = "fn add(a: u8) -> u8 {\n    a\n}\n\nfn sub() {}\n";
        let file = parse(text);

        assert_eq!(hash_file(&file, NORMALIZE), hash_file(&file, EXACT));
        assert_eq!(hash_file(&file, NORMALIZE), Hash::digest(text.as_bytes()));
        assert_eq!(
            symbol_hash_with(text, "add", NORMALIZE),
            symbol_hash_with(text, "add", EXACT),
        );
        assert_eq!(
            symbol_hash_with(text, "add", NORMALIZE),
            Hash::digest(b"fn add(a: u8) -> u8 {\n    a\n}"),
        );
    }
}
