//! The built-in language registry: [`Languages::builtin`].

use std::sync::LazyLock;

use tree_sitter::{LanguageError, Parser};

use crate::read::cartridge::{Cartridge, Languages};
use crate::read::error::CartridgeLoadError;

/// The registry, built the first time it is asked for. `Err` only through a supdoc bug.
static BUILTIN: LazyLock<Result<Languages, CartridgeLoadError>> =
    LazyLock::new(|| Ok(Languages::new(load_builtins()?)));

/// Whether the Markdown inline grammar loads into a parser, checked once. Reading links needs it.
static INLINE_GRAMMAR: LazyLock<Result<(), LanguageError>> =
    LazyLock::new(|| Parser::new().set_language(&tree_sitter_md::INLINE_LANGUAGE.into()));

/// A parser for tree-sitter-md's inline grammar.
///
/// [`Languages::builtin`] fails the run when the grammar does not load, so every parser made
/// after it has the grammar.
pub(crate) fn inline_parser() -> Parser {
    let mut parser = Parser::new();
    let _loaded = parser.set_language(&tree_sitter_md::INLINE_LANGUAGE.into());
    parser
}

impl Languages {
    /// Every cartridge compiled into supdoc, built once per process.
    ///
    /// # Errors
    ///
    /// [`CartridgeLoadError`] when a built-in query or grammar doesn't load. That is a supdoc bug
    /// in every case, so the run fails with exit code 70 instead of reading files as an
    /// unsupported language. The registry's tests load every built-in.
    pub(crate) fn builtin() -> Result<&'static Self, &'static CartridgeLoadError> {
        BUILTIN.as_ref()
    }
}

/// Every built-in cartridge, or the first error that kept one from loading.
///
/// # Errors
///
/// [`CartridgeLoadError`] for the first query file or grammar that doesn't load.
fn load_builtins() -> Result<Vec<&'static dyn Cartridge>, CartridgeLoadError> {
    let _loaded = INLINE_GRAMMAR
        .as_ref()
        .map_err(CartridgeLoadError::InlineGrammar)?;
    let query = CartridgeLoadError::Query;
    Ok(vec![
        crate::cartridges::rust::cartridge().map_err(query)? as &'static dyn Cartridge,
        crate::cartridges::markdown::cartridge::cartridge(),
        crate::cartridges::bash::cartridge().map_err(query)?,
        crate::cartridges::go::cartridge().map_err(query)?,
        crate::cartridges::js::cartridge().map_err(query)?,
        crate::cartridges::ts::cartridge().map_err(query)?,
        crate::cartridges::tsx::cartridge().map_err(query)?,
        crate::cartridges::python::cartridge().map_err(query)?,
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;
    use crate::cartridges::python::PythonCartridge;
    use crate::read::query::{MARKDOWN_INLINE, QueryCartridge};

    #[test]
    fn every_builtin_query_compiles_against_its_grammar() {
        if let Err(error) = load_builtins() {
            panic!("{error}");
        }
    }

    #[test]
    fn every_extension_maps_to_exactly_one_cartridge() {
        let mut owners: HashMap<&str, Vec<&str>> = HashMap::new();
        for cartridge in Languages::builtin()
            .expect("the built-in cartridges should load")
            .iter()
        {
            for extension in cartridge.extensions() {
                owners
                    .entry(extension)
                    .or_default()
                    .push(cartridge.id().as_str());
            }
        }

        let shared: Vec<_> = owners.iter().filter(|(_, ids)| ids.len() > 1).collect();
        assert!(
            shared.is_empty(),
            "extensions claimed by several cartridges: {shared:?}"
        );
    }

    #[test]
    fn every_extension_is_lowercase_without_a_dot() {
        for cartridge in Languages::builtin()
            .expect("the built-in cartridges should load")
            .iter()
        {
            for extension in cartridge.extensions() {
                assert!(
                    !extension.is_empty()
                        && !extension.contains('.')
                        && *extension == extension.to_ascii_lowercase(),
                    "{} declares extension {extension:?}",
                    cartridge.id(),
                );
            }
        }
    }

    #[test]
    fn every_builtin_injection_query_marks_markdown_inline() {
        // Markdown finds its regions with its own code; every other cartridge is query-driven.
        let cartridges = [
            (
                "rust",
                crate::cartridges::rust::cartridge().map(QueryCartridge::queries),
            ),
            (
                "bash",
                crate::cartridges::bash::cartridge().map(QueryCartridge::queries),
            ),
            (
                "go",
                crate::cartridges::go::cartridge().map(QueryCartridge::queries),
            ),
            (
                "js",
                crate::cartridges::js::cartridge().map(QueryCartridge::queries),
            ),
            (
                "ts",
                crate::cartridges::ts::cartridge().map(QueryCartridge::queries),
            ),
            (
                "tsx",
                crate::cartridges::tsx::cartridge().map(QueryCartridge::queries),
            ),
            (
                "python",
                crate::cartridges::python::cartridge().map(PythonCartridge::queries),
            ),
        ];
        for (id, queries) in cartridges {
            let query = queries
                .expect("the built-in cartridges should load")
                .injections();
            let marks = (0..query.pattern_count()).any(|pattern| {
                query.property_settings(pattern).iter().any(|property| {
                    &*property.key == "injection.language"
                        && property.value.as_deref() == Some(MARKDOWN_INLINE)
                })
            });
            assert!(
                marks,
                "{id}'s injections.scm marks no region as {MARKDOWN_INLINE}"
            );
        }
    }

    #[test]
    fn rust_files_use_the_rust_cartridge() {
        let languages = Languages::builtin().expect("the built-in cartridges should load");

        assert_eq!(
            languages
                .for_path(Path::new("src/lib.rs"))
                .map(|c| c.id().as_str()),
            Some("rust"),
        );
    }
}
