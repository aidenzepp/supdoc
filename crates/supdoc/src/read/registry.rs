//! The built-in language registry: [`Languages::builtin`].
//!
//! Each query-driven cartridge is registered as a [`LazyCartridge`]: its id, extensions and
//! grammar are known up front, and its queries compile the first time a file of its language is
//! read, at most once per run. Choosing a cartridge for a path compiles nothing.

use std::fmt;
use std::sync::LazyLock;

use tree_sitter::{LanguageError, Parser};

use crate::cartridges::{bash, go, js, python, rust, ts, tsx};
use crate::read::cartridge::{
    Cartridge, LanguageId, Languages, MarkdownRegion, SourceFile, SymbolTable,
};
use crate::read::error::{CartridgeLoadError, ReadError};
use crate::read::query::QueryFileError;

/// The registry, built the first time it is asked for. `Err` only through a supdoc bug.
static BUILTIN: LazyLock<Result<Languages, CartridgeLoadError>> = LazyLock::new(|| {
    let _loaded = INLINE_GRAMMAR
        .as_ref()
        .map_err(CartridgeLoadError::InlineGrammar)?;
    Ok(Languages::new(vec![
        &RUST,
        crate::cartridges::markdown::cartridge::cartridge(),
        &BASH,
        &GO,
        &JAVASCRIPT,
        &TYPESCRIPT,
        &TSX,
        &PYTHON,
        crate::cartridges::toml::cartridge(),
        crate::cartridges::json::cartridge(),
        crate::cartridges::yaml::cartridge(),
    ]))
});

/// Whether the Markdown inline grammar loads into a parser, checked once. Reading links needs it.
static INLINE_GRAMMAR: LazyLock<Result<(), LanguageError>> =
    LazyLock::new(|| Parser::new().set_language(&tree_sitter_md::INLINE_LANGUAGE.into()));

/// Rust, compiled when the first `.rs` file is read.
static RUST: LazyCartridge =
    LazyCartridge::new(rust::ID, rust::EXTENSIONS, rust::grammar, true, || {
        compiled(rust::cartridge())
    });

/// Bash, compiled when the first shell script is read.
static BASH: LazyCartridge =
    LazyCartridge::new(bash::ID, bash::EXTENSIONS, bash::grammar, true, || {
        compiled(bash::cartridge())
    });

/// Go, compiled when the first `.go` file is read.
static GO: LazyCartridge = LazyCartridge::new(go::ID, go::EXTENSIONS, go::grammar, true, || {
    compiled(go::cartridge())
});

/// JavaScript, compiled when the first JavaScript file is read.
static JAVASCRIPT: LazyCartridge =
    LazyCartridge::new(js::ID, js::EXTENSIONS, js::grammar, true, || {
        compiled(js::cartridge())
    });

/// TypeScript, compiled when the first TypeScript file is read.
static TYPESCRIPT: LazyCartridge =
    LazyCartridge::new(ts::ID, ts::EXTENSIONS, ts::grammar, true, || {
        compiled(ts::cartridge())
    });

/// TSX, compiled when the first `.tsx` file is read.
static TSX: LazyCartridge =
    LazyCartridge::new(tsx::ID, tsx::EXTENSIONS, tsx::grammar, true, || {
        compiled(tsx::cartridge())
    });

/// Python, compiled when the first Python file is read.
static PYTHON: LazyCartridge = LazyCartridge::new(
    python::ID,
    python::EXTENSIONS,
    python::grammar,
    true,
    || compiled(python::cartridge()),
);

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
    /// Every cartridge compiled into supdoc, registered once per process. No query is compiled
    /// here: each cartridge compiles its own the first time a file of its language is read.
    ///
    /// # Errors
    ///
    /// [`CartridgeLoadError`] when the Markdown inline grammar doesn't load, which every read
    /// needs. That is a supdoc bug, so the run fails with exit code 70. A built-in query that
    /// doesn't compile fails the run the same way, as [`ReadError::CartridgeLoad`] when a file
    /// of its language is read. The registry's tests load every built-in.
    pub(crate) fn builtin() -> Result<&'static Self, &'static CartridgeLoadError> {
        BUILTIN.as_ref()
    }
}

/// A cartridge known by its id, extensions and grammar until a file of its language is read,
/// which compiles the cartridge it stands for.
pub(crate) struct LazyCartridge {
    /// Stable id.
    id: LanguageId,
    /// Lowercase extensions without the dot.
    extensions: &'static [&'static str],
    /// The grammar, loaded on first use. Loading it compiles nothing.
    grammar: LazyLock<tree_sitter::Language>,
    /// The cartridge it stands for. Compiles it on the first call and returns the same result
    /// after that.
    compiled: fn() -> Result<&'static dyn Cartridge, CartridgeLoadError>,
    /// Whether files of this language are walked as link sources. A static property of the
    /// language, set at registration, so asking never compiles anything.
    reads_links: bool,
}

impl LazyCartridge {
    /// A cartridge named `id` for `extensions`, parsing with `grammar` and compiled by `compiled`.
    /// `reads_links` is `false` for a language that is a link target only.
    #[must_use]
    pub(crate) const fn new(
        id: LanguageId,
        extensions: &'static [&'static str],
        grammar: fn() -> tree_sitter::Language,
        reads_links: bool,
        compiled: fn() -> Result<&'static dyn Cartridge, CartridgeLoadError>,
    ) -> Self {
        Self {
            id,
            extensions,
            grammar: LazyLock::new(grammar),
            compiled,
            reads_links,
        }
    }

    /// The compiled cartridge, for a step of reading `file`.
    ///
    /// # Errors
    ///
    /// [`ReadError::CartridgeLoad`] when it doesn't compile. [`SourceFile::from_text`] loads the
    /// cartridge before any file exists, so a step of reading one never meets it.
    fn compiled_for(&self, file: &SourceFile) -> Result<&'static dyn Cartridge, ReadError> {
        (self.compiled)().map_err(|error| ReadError::CartridgeLoad {
            path: file.path().clone(),
            error,
        })
    }
}

impl fmt::Debug for LazyCartridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LazyCartridge")
            .field("id", &self.id)
            .field("extensions", &self.extensions)
            .finish_non_exhaustive()
    }
}

impl Cartridge for LazyCartridge {
    fn id(&self) -> LanguageId {
        self.id
    }

    fn extensions(&self) -> &'static [&'static str] {
        self.extensions
    }

    fn grammar(&self) -> &tree_sitter::Language {
        &self.grammar
    }

    fn reads_links(&self) -> bool {
        self.reads_links
    }

    fn load(&self) -> Result<(), CartridgeLoadError> {
        (self.compiled)().map(|_cartridge| ())
    }

    fn symbols(&self, file: &SourceFile) -> Result<SymbolTable, ReadError> {
        self.compiled_for(file)?.symbols(file)
    }

    fn markdown_regions(&self, file: &SourceFile) -> Result<Vec<MarkdownRegion>, ReadError> {
        self.compiled_for(file)?.markdown_regions(file)
    }
}

/// A built-in module's cartridge, as the registry hands it out.
///
/// # Errors
///
/// [`CartridgeLoadError::Query`] when one of its query files doesn't compile.
pub(crate) fn compiled<C: Cartridge>(
    cartridge: Result<&'static C, &'static QueryFileError>,
) -> Result<&'static dyn Cartridge, CartridgeLoadError> {
    match cartridge {
        Ok(cartridge) => Ok(cartridge),
        Err(error) => Err(CartridgeLoadError::Query(error)),
    }
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
        for cartridge in Languages::builtin()
            .expect("the built-in cartridges should register")
            .iter()
        {
            if let Err(error) = cartridge.load() {
                panic!("{error}");
            }
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

    /// A cartridge that is a link target only.
    #[derive(Debug)]
    struct TargetOnly {
        /// The grammar, created on first use.
        grammar: std::sync::OnceLock<tree_sitter::Language>,
    }

    impl Cartridge for TargetOnly {
        fn id(&self) -> LanguageId {
            LanguageId::new("target-only")
        }

        fn extensions(&self) -> &'static [&'static str] {
            &["tgt"]
        }

        fn grammar(&self) -> &tree_sitter::Language {
            self.grammar
                .get_or_init(|| tree_sitter_rust::LANGUAGE.into())
        }

        fn reads_links(&self) -> bool {
            false
        }

        fn symbols(&self, _file: &SourceFile) -> Result<SymbolTable, ReadError> {
            Ok(SymbolTable::default())
        }

        fn markdown_regions(&self, _file: &SourceFile) -> Result<Vec<MarkdownRegion>, ReadError> {
            Ok(Vec::new())
        }
    }

    static TARGET_ONLY: TargetOnly = TargetOnly {
        grammar: std::sync::OnceLock::new(),
    };

    /// Whether the test cartridge's compile closure ran.
    static COMPILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    #[test]
    fn a_target_only_lazy_cartridge_is_not_a_source_and_compiles_nothing_to_say_so() {
        let lazy = LazyCartridge::new(
            LanguageId::new("target-only"),
            &["tgt"],
            || tree_sitter_rust::LANGUAGE.into(),
            false,
            || {
                COMPILED.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(&TARGET_ONLY)
            },
        );

        assert!(
            !lazy.reads_links(),
            "a lazily registered target-only cartridge should not be walked as a source"
        );
        assert!(
            !COMPILED.load(std::sync::atomic::Ordering::SeqCst),
            "asking reads_links must not compile the cartridge"
        );
    }

    #[test]
    fn only_the_data_cartridges_are_target_only() {
        let target_only: Vec<&str> = Languages::builtin()
            .expect("the built-in cartridges should load")
            .iter()
            .filter(|cartridge| !cartridge.reads_links())
            .map(|cartridge| cartridge.id().as_str())
            .collect();

        assert_eq!(
            target_only,
            ["toml", "json", "yaml"],
            "data files are link targets, never link sources (D2.3)"
        );
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
