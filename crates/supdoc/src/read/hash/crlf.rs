//! Witness (B.6, FEAT-7): under `normalize`, a CRLF file hashes every symbol like its LF twin, in
//! every built-in cartridge. One test per cartridge, each over a multi-line fixture with comments.

use std::path::Path;

use crate::hash::{HashSettings, LineEndings};
use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, Languages, SourceFile};
use crate::read::hash::hash_symbol;

const NORMALIZE: HashSettings = HashSettings {
    line_endings: LineEndings::Normalize,
};

fn cartridge(id: &str) -> &'static dyn Cartridge {
    Languages::builtin()
        .iter()
        .find(|cartridge| cartridge.id().as_str() == id)
        .unwrap_or_else(|| panic!("the built-in registry should hold a `{id}` cartridge"))
}

fn parse(language: &'static dyn Cartridge, text: &str) -> SourceFile {
    let extension = language
        .extensions()
        .first()
        .expect("every cartridge should claim an extension");
    let path = AbsolutePath::new(Path::new(&format!("/repo/fixture.{extension}")))
        .expect("the fixture path should be absolute");
    SourceFile::from_text(path, text.to_owned(), language)
        .unwrap_or_else(|| panic!("`{}` should parse the fixture", language.id()))
}

/// Asserts that `lf` and its CRLF twin yield the same symbols with the same hashes.
fn assert_crlf_matches_lf(id: &str, lf: &str) {
    assert!(!lf.contains('\r'), "the LF fixture should hold no CR");
    let crlf = lf.replace('\n', "\r\n");
    let language = cartridge(id);
    let lf_file = parse(language, lf);
    let crlf_file = parse(language, &crlf);
    let lf_symbols: Vec<_> = language.symbols(&lf_file).iter().cloned().collect();
    let crlf_symbols: Vec<_> = language.symbols(&crlf_file).iter().cloned().collect();

    assert!(
        lf_symbols
            .iter()
            .any(|symbol| lf[symbol.range.clone()].contains('\n')),
        "the {id} fixture should yield a multi-line symbol, got {lf_symbols:?}",
    );
    assert_eq!(
        lf_symbols.len(),
        crlf_symbols.len(),
        "{id}: LF and CRLF should yield the same symbols\nLF: {lf_symbols:?}\nCRLF: {crlf_symbols:?}",
    );
    for (lf_symbol, crlf_symbol) in lf_symbols.iter().zip(&crlf_symbols) {
        assert_eq!(
            lf_symbol.path, crlf_symbol.path,
            "{id}: symbol paths differ"
        );
        assert_eq!(
            hash_symbol(&lf_file, lf_symbol, NORMALIZE),
            hash_symbol(&crlf_file, crlf_symbol, NORMALIZE),
            "{id}: `{}` hashes differently under CRLF (LF range {:?}, CRLF range {:?})",
            lf_symbol.path,
            lf_symbol.range,
            crlf_symbol.range,
        );
    }
}

#[test]
fn rust_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "rust",
        "/// Adds.\nfn add(a: u8, b: u8) -> u8 {\n    // sum\n    a + b\n}\n\nstruct Point {\n    x: u8,\n}\n\nimpl Point {\n    fn x(&self) -> u8 {\n        self.x\n    }\n}\n",
    );
}

#[test]
fn markdown_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "markdown",
        "# Title\n\nIntro text.\n\n## Usage\n\n```rust\nfn add() {}\n```\n\n## Notes\n\nMore text.\n",
    );
}

#[test]
fn bash_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "bash",
        "#!/usr/bin/env bash\n\n# Greets.\ngreet() {\n  echo \"hello $1\"\n  echo done\n}\n\nbuild() {\n  cargo build\n}\n",
    );
}

#[test]
fn go_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "go",
        "package main\n\n// Add adds.\nfunc Add(a, b int) int {\n\t// sum\n\treturn a + b\n}\n\ntype Point struct {\n\tX int\n}\n\nfunc (p Point) Get() int {\n\treturn p.X\n}\n",
    );
}

#[test]
fn js_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "javascript",
        "// Adds.\nfunction add(a, b) {\n  // sum\n  return a + b;\n}\n\nclass Point {\n  get() {\n    return 1;\n  }\n}\n\nconst text = `one\ntwo`;\n",
    );
}

#[test]
fn ts_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "typescript",
        "// Adds.\nfunction add(a: number, b: number): number {\n  // sum\n  return a + b;\n}\n\ninterface Point {\n  x: number;\n}\n\nclass Box {\n  get(): number {\n    return 1;\n  }\n}\n",
    );
}

#[test]
fn tsx_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "tsx",
        "// Greets.\nfunction Greeting(props: { name: string }) {\n  return (\n    <div>\n      <b>{props.name}</b>\n    </div>\n  );\n}\n\ninterface Props {\n  name: string;\n}\n",
    );
}

#[test]
fn python_crlf_hashes_like_lf() {
    assert_crlf_matches_lf(
        "python",
        "def add(a, b):\n    \"\"\"Adds.\n\n    Sums.\n    \"\"\"\n    # sum\n    return a + b\n\n\nclass Point:\n    def get(self):\n        return 1\n",
    );
}
