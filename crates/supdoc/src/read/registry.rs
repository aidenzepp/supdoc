//! The built-in language registry: [`Languages::builtin`] (03 R1, The registry).

use crate::read::cartridge::{Cartridge, Languages};
use crate::read::query::QueryFileError;

impl Languages {
    /// Every cartridge compiled into supdoc.
    ///
    /// A built-in cartridge whose queries don't compile is left out, so its files read as an
    /// unsupported language instead of crashing the run. The registry's tests compile every
    /// built-in, so that only happens through a supdoc bug.
    #[must_use]
    pub fn builtin() -> Self {
        Self::new(builtins().into_iter().filter_map(Result::ok).collect())
    }
}

/// Every built-in cartridge, or the error that kept it from compiling.
fn builtins() -> Vec<Result<&'static dyn Cartridge, &'static QueryFileError>> {
    vec![crate::cartridges::rust::cartridge().map(|rust| rust as &'static dyn Cartridge)]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;
    use crate::read::query::MARKDOWN_INLINE;

    #[test]
    fn every_builtin_query_compiles_against_its_grammar() {
        for built in builtins() {
            assert!(
                built.is_ok(),
                "{}",
                built.err().map(ToString::to_string).unwrap_or_default()
            );
        }
    }

    #[test]
    fn every_extension_maps_to_exactly_one_cartridge() {
        let mut owners: HashMap<&str, Vec<&str>> = HashMap::new();
        for cartridge in Languages::builtin().iter() {
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
        for cartridge in Languages::builtin().iter() {
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
        for cartridge in Languages::builtin()
            .iter()
            .filter(|c| c.id().as_str() != "markdown")
        {
            let query = cartridge.queries().injections();
            let marks = (0..query.pattern_count()).any(|pattern| {
                query.property_settings(pattern).iter().any(|property| {
                    &*property.key == "injection.language"
                        && property.value.as_deref() == Some(MARKDOWN_INLINE)
                })
            });
            assert!(
                marks,
                "{}'s injections.scm marks no region as {MARKDOWN_INLINE}",
                cartridge.id()
            );
        }
    }

    #[test]
    fn rust_files_use_the_rust_cartridge() {
        let languages = Languages::builtin();

        assert_eq!(
            languages
                .for_path(Path::new("src/lib.rs"))
                .map(|c| c.id().as_str()),
            Some("rust"),
        );
    }
}
