//! `[features.code-blocks]`: the table that turns code-block checking on, and the one place its
//! keys live.
//!
//! The table is on when any file in the `extends` chain writes it. Each key it gains will come from
//! the nearest file that sets it, as everywhere else in the chain.

/// The `[features.code-blocks]` table, exactly as written. It has no keys yet, so any key inside
/// it is refused as `config-parse`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCodeBlocks {}

/// The resolved code-block settings, present only when some file in the chain wrote the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeBlocks {}

/// Folds one layer's table into what the nearer layers already resolved, child first: the
/// feature is on once any layer writes the table.
#[must_use]
pub(super) fn merge(nearer: Option<CodeBlocks>, raw: Option<RawCodeBlocks>) -> Option<CodeBlocks> {
    nearer.or_else(|| raw.map(|RawCodeBlocks {}| CodeBlocks {}))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::config::Config;
    use crate::exit::ExitStatus;
    use crate::path::AbsolutePath;

    /// Writes `files` (relative path, contents) under a fresh temp dir.
    fn tree(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().expect("a temp dir should be creatable");
        for (relative, contents) in files {
            let path = dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be creatable");
            }
            fs::write(&path, contents).expect("the fixture file should be writable");
        }
        dir
    }

    /// `relative` beneath the temp dir, as an absolute path.
    fn at(dir: &TempDir, relative: &str) -> AbsolutePath {
        AbsolutePath::new(&dir.path().join(relative)).expect("a temp dir path should be absolute")
    }

    #[test]
    fn a_key_inside_the_table_is_refused_as_a_parse_error() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks]\nlint = \"all\"\n",
        )]);

        let error = Config::load_file(&at(&dir, "supdoc.config.toml"))
            .expect_err("a key inside the empty table should be refused");

        assert_eq!(error.code(), "config-parse", "got {error:?}");
        assert_eq!(error.status(), ExitStatus::Config);
    }

    #[test]
    fn the_table_is_inherited_through_extends() {
        let dir = tree(&[
            ("supdoc.config.toml", "[features.code-blocks]\n"),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n",
            ),
        ]);

        let config = Config::load(&at(&dir, "apps/web")).expect("the fixture config should load");

        assert!(
            config.code_blocks().is_some(),
            "a child should inherit the parent's table"
        );
    }

    #[test]
    fn without_the_table_the_feature_is_off() {
        let dir = tree(&[("supdoc.config.toml", "include = [\"docs\"]\n")]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");

        assert!(config.code_blocks().is_none());
    }
}
