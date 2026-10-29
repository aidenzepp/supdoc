//! Markdown, the cartridge that combines headings and body regions (03 R9, R12, R14).
//!
//! It is code rather than data: its symbols are [`heading_symbols`] and its link regions are
//! [`markdown_regions`]. Its query files are empty, because neither answer comes from a query.

use std::sync::LazyLock;

use crate::cartridges::markdown::regions::markdown_regions;
use crate::cartridges::markdown::symbols::heading_symbols;
use crate::read::cartridge::{
    Cartridge, CompiledQueries, LanguageId, MarkdownRegion, SourceFile, SymbolTable,
};
use crate::read::query::QueryFileError;

/// The Markdown language: block grammar, headings as symbols, body text as link regions.
#[derive(Debug)]
pub struct Markdown {
    /// The block grammar.
    grammar: tree_sitter::Language,
    /// Empty `tags.scm` and `injections.scm`: Markdown reads its own structure.
    queries: CompiledQueries,
}

/// The cartridge, compiled the first time it is used.
static MARKDOWN: LazyLock<Result<Markdown, QueryFileError>> = LazyLock::new(|| {
    let grammar: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    let id = LanguageId::new("markdown");
    let compile = |file: &'static str| {
        tree_sitter::Query::new(&grammar, "").map_err(|error| QueryFileError {
            language: id,
            file,
            error,
        })
    };
    let queries = CompiledQueries::new(compile("tags.scm")?, compile("injections.scm")?);
    Ok(Markdown { grammar, queries })
});

/// The Markdown cartridge.
///
/// # Errors
///
/// The [`QueryFileError`] for an empty query that doesn't compile: a supdoc bug, caught by the
/// registry's tests.
pub fn cartridge() -> Result<&'static Markdown, &'static QueryFileError> {
    MARKDOWN.as_ref()
}

impl Cartridge for Markdown {
    fn id(&self) -> LanguageId {
        LanguageId::new("markdown")
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["md", "markdown"]
    }

    fn grammar(&self) -> &tree_sitter::Language {
        &self.grammar
    }

    fn queries(&self) -> &CompiledQueries {
        &self.queries
    }

    fn symbols(&self, file: &SourceFile) -> SymbolTable {
        heading_symbols(file)
    }

    fn markdown_regions(&self, file: &SourceFile) -> Vec<MarkdownRegion> {
        markdown_regions(file)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::link::SymbolPath;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Languages, SourceFile};
    use crate::read::hash::hash_symbol;
    use crate::read::links::links;

    const GUIDE: &str =
        "# News\n\n## What's new\n\nSee [the code](../src/lib.rs#add).\n\n## Fixes\n";

    /// `GUIDE` parsed with whichever cartridge the built-in registry picks for `name`.
    fn registered_file(name: &str) -> SourceFile {
        let language = Languages::builtin()
            .for_path(Path::new(name))
            .unwrap_or_else(|| panic!("the registry should claim `{name}`"));
        let path = AbsolutePath::new(&Path::new("/repo/docs").join(name))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, GUIDE.to_owned(), language)
            .expect("tree-sitter should return a tree")
    }

    fn section_of(file: &SourceFile, raw: &str) -> String {
        let query: SymbolPath = raw.parse().expect("the fixture symbol should be valid");
        let table = file.language().symbols(file);
        let symbol = table
            .resolve(file.path(), &query)
            .unwrap_or_else(|error| panic!("`{raw}` should resolve: {error}"));
        file.text()
            .get(symbol.range.clone())
            .expect("a symbol's range should be valid for its file")
            .to_owned()
    }

    #[test]
    fn registry_claims_md_and_markdown_as_markdown() {
        for name in ["guide.md", "guide.markdown"] {
            assert_eq!(registered_file(name).language().id().as_str(), "markdown");
        }
    }

    #[test]
    fn github_anchor_and_dotted_path_resolve_to_the_same_section() {
        let file = registered_file("guide.md");

        let anchor = section_of(&file, "whats-new");
        let dotted = section_of(&file, "news.whats-new");

        assert_eq!(anchor, dotted);
        assert!(anchor.starts_with("## What's new"), "{anchor:?}");
        assert!(!anchor.contains("## Fixes"), "{anchor:?}");
    }

    #[test]
    fn body_of_a_markdown_file_yields_link_regions() {
        let file = registered_file("guide.md");

        assert_eq!(
            links(&file).len(),
            1,
            "the one link in the body should be read through the registered cartridge"
        );
    }

    /// The hex SHA-256 of the section `raw` names in `GUIDE`.
    fn section_hash(raw: &str) -> String {
        let file = registered_file("guide.md");
        let query: SymbolPath = raw.parse().expect("the fixture symbol should be valid");
        let table = file.language().symbols(&file);
        let symbol = table
            .resolve(file.path(), &query)
            .unwrap_or_else(|error| panic!("`{raw}` should resolve: {error}"));
        hash_symbol(&file, symbol).to_string()
    }

    /// Pinned from `printf` of the section's bytes piped to `shasum -a 256`, not from supdoc: a
    /// section runs from its heading to the next heading of the same or a higher level.
    #[test]
    fn markdown_section_hashes_are_pinned_to_independent_sha256() {
        assert_eq!(
            section_hash("whats-new"),
            "0f1311f2de2734964c2ff0a1df8bef599fec308f179542477797cb323ecfcb06",
            "`## What's new` through the blank line before `## Fixes`",
        );
        assert_eq!(
            section_hash("fixes"),
            "24a186ba602b4335f8a11d784b44b4421711733df8d922231ed52965208e2380",
            "`## Fixes` to the end of the file",
        );
        assert_eq!(
            section_hash("news"),
            "8aeceb4240bb33ae5c1aa610838fdb9a7c5b66c0e88b808c8570d2fd38f068a3",
            "`# News` holds both subsections, so it is the whole file",
        );
    }
}
