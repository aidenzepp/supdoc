//! [`QueryCartridge`]: a language that is pure data, a grammar plus its `tags.scm` and
//! `injections.scm` (03 R3, The query contract).
//!
//! Both files use tree-sitter's own capture names, so they stay ordinary tree-sitter queries:
//!
//! | Capture | Meaning |
//! |---|---|
//! | `@definition.<kind>` | An addressable symbol. Its node's range is what gets hashed |
//! | `@name` | Its name, one path segment |
//! | `@injection.content` + `injection.language` | With `markdown_inline`: a region where links are read |
//! | `@supdoc.scope` | A container that adds segments to definitions inside it but isn't addressable (a Rust `impl`) |
//! | `@supdoc.scope.name` | That segment. Inside a definition pattern, a parent segment containment can't express (a Go receiver) |
//! | `@supdoc.scope.qualifier` | A second segment after the name (the trait in `impl Trait for Type`) |
//! | `@supdoc.scope.arguments`, `@supdoc.scope.qualifier.arguments` | Type arguments of the name or qualifier, for the full-type alias (03 R5) |
//!
//! See [`tag_symbols`] for how a path is built from them.

use tree_sitter::{QueryCursor, StreamingIterator};

use crate::read::cartridge::{Cartridge, CompiledQueries, LanguageId, SourceFile, SymbolTable};
use crate::read::symbols::tag_symbols;

/// The `injection.language` that marks a region where links are read (03 R9).
pub const MARKDOWN_INLINE: &str = "markdown_inline";

/// A cartridge that is only data: its grammar and two compiled query files.
#[derive(Debug)]
pub struct QueryCartridge {
    /// Stable id.
    id: LanguageId,
    /// Lowercase extensions without the dot.
    extensions: &'static [&'static str],
    /// The grammar.
    grammar: tree_sitter::Language,
    /// `tags.scm` and `injections.scm`, compiled against `grammar`.
    queries: CompiledQueries,
}

impl QueryCartridge {
    /// Compiles `tags` and `injections` against `grammar`.
    ///
    /// Built-in cartridges are built once, on first use. A test compiles every one of them, so a
    /// failure here is a supdoc bug or a grammar upgrade that renamed node kinds.
    ///
    /// # Errors
    ///
    /// [`QueryFileError`] naming the query file that doesn't compile and tree-sitter's reason.
    pub fn new(
        id: LanguageId,
        extensions: &'static [&'static str],
        grammar: tree_sitter::Language,
        tags: &str,
        injections: &str,
    ) -> Result<Self, QueryFileError> {
        let compile = |file: &'static str, source: &str| {
            tree_sitter::Query::new(&grammar, source).map_err(|error| QueryFileError {
                language: id,
                file,
                error,
            })
        };
        let queries = CompiledQueries::new(
            compile("tags.scm", tags)?,
            compile("injections.scm", injections)?,
        );
        Ok(Self {
            id,
            extensions,
            grammar,
            queries,
        })
    }
}

impl Cartridge for QueryCartridge {
    fn id(&self) -> LanguageId {
        self.id
    }

    fn extensions(&self) -> &'static [&'static str] {
        self.extensions
    }

    fn grammar(&self) -> &tree_sitter::Language {
        &self.grammar
    }

    fn queries(&self) -> &CompiledQueries {
        &self.queries
    }

    fn symbols(&self, file: &SourceFile) -> SymbolTable {
        tag_symbols(file)
    }

    fn markdown_regions(&self, file: &SourceFile) -> Vec<tree_sitter::Range> {
        injected_markdown(file)
    }
}

/// The ranges `file`'s `injections.scm` marks as `markdown_inline`, in file order (03 R9, R14).
///
/// The language comes from a `#set! injection.language` on the pattern or from an
/// `@injection.language` capture, tree-sitter's two ways of naming it.
#[must_use]
pub fn injected_markdown(file: &SourceFile) -> Vec<tree_sitter::Range> {
    let query = file.language().queries().injections();
    let content = query.capture_index_for_name("injection.content");
    let language = query.capture_index_for_name("injection.language");
    let text = file.text();

    let mut regions = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, file.tree().root_node(), text.as_bytes());
    while let Some(found) = matches.next() {
        let set = query
            .property_settings(found.pattern_index)
            .iter()
            .any(|property| {
                &*property.key == "injection.language"
                    && property.value.as_deref() == Some(MARKDOWN_INLINE)
            });
        let captured = found.captures().iter().any(|capture| {
            Some(capture.index) == language
                && text.get(capture.node.byte_range()) == Some(MARKDOWN_INLINE)
        });
        if !set && !captured {
            continue;
        }
        regions.extend(
            found
                .captures()
                .iter()
                .filter(|capture| Some(capture.index) == content)
                .map(|capture| capture.node.range()),
        );
    }
    regions.sort_by_key(|range| (range.start_byte, range.end_byte));
    regions.dedup();
    regions
}

/// A built-in query file doesn't compile against its grammar.
#[derive(Debug, thiserror::Error)]
#[error("the built-in {file} for {language} does not compile (supdoc bug): {error}")]
pub struct QueryFileError {
    /// The cartridge.
    pub language: LanguageId,
    /// `tags.scm` or `injections.scm`.
    pub file: &'static str,
    /// tree-sitter's reason, with row and column.
    pub error: tree_sitter::QueryError,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::path::AbsolutePath;

    fn rust_file(text: &str) -> SourceFile {
        let rust =
            crate::cartridges::rust::cartridge().expect("the built-in Rust queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), rust).expect("tree-sitter should return a tree")
    }

    fn region_texts(file: &SourceFile) -> Vec<&str> {
        file.language()
            .markdown_regions(file)
            .iter()
            .map(|range| {
                file.text()
                    .get(range.start_byte..range.end_byte)
                    .expect("a region should be a valid range of the file")
            })
            .collect()
    }

    #[test]
    fn rust_comments_are_markdown_regions_in_file_order() {
        let file =
            rust_file("/// See [add](../docs/a.md).\nfn add() {\n    // inline\n}\n/* block */\n");

        let texts = region_texts(&file);

        assert_eq!(texts.len(), 3, "one region per comment, got {texts:?}");
        assert!(
            texts
                .first()
                .is_some_and(|text| text.contains("[add](../docs/a.md)")),
            "the doc comment comes first: {texts:?}",
        );
        assert!(
            texts.get(1).is_some_and(|text| text.contains("inline")),
            "{texts:?}"
        );
        assert_eq!(texts.get(2), Some(&"/* block */"));
    }

    #[test]
    fn string_literal_is_not_a_markdown_region() {
        let file = rust_file("fn f() { let s = \"[a](b.rs#c)\"; }\n");

        assert_eq!(region_texts(&file), Vec::<&str>::new());
    }

    #[test]
    fn query_error_names_the_file_and_language() {
        let error = QueryCartridge::new(
            LanguageId::new("rust"),
            &["rs"],
            tree_sitter_rust::LANGUAGE.into(),
            "(no_such_node) @definition.function",
            "",
        )
        .expect_err("a query naming an unknown node kind should not compile");

        assert_eq!(error.file, "tags.scm");
        assert!(
            error
                .to_string()
                .starts_with("the built-in tags.scm for rust does not compile (supdoc bug): "),
            "got {error}",
        );
    }
}
