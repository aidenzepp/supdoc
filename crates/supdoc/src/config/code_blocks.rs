//! `[features.code-blocks]`: the table that turns code-block checking on, and the one place its
//! keys live.
//!
//! The table is on when any file in the `extends` chain writes it. Each key comes from the nearest
//! file that sets it, as everywhere else in the chain.
//!
//! `include` and `exclude` choose which files code-blocks reads, independently of linking: each
//! defaults to the top-level key of the same name, and setting one never changes which files
//! linking reads.

use crate::path::AbsolutePath;

use super::pattern::{AnchoredPatterns, PatternList};

/// The `[features.code-blocks]` table, exactly as written. Any other key inside it is refused as
/// `config-parse`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCodeBlocks {
    /// The files code-blocks reads, with the top-level `include` rules. `None` inherits.
    include: Option<PatternList>,
    /// Files removed from code-blocks' `include`, with the top-level `exclude` rules. `None`
    /// inherits; `Some(empty)` deliberately excludes nothing.
    exclude: Option<PatternList>,
}

/// The table's keys merged across the chain, before the top-level defaults fill the gaps.
#[derive(Debug, Default)]
pub(super) struct Written {
    /// The nearest `include` written inside the table, anchored at the file that wrote it.
    include: Option<AnchoredPatterns>,
    /// The nearest `exclude` written inside the table, anchored at the file that wrote it.
    exclude: Option<AnchoredPatterns>,
}

impl Written {
    /// The resolved settings, each unset key taken from the top-level `include` or `exclude`.
    #[must_use]
    pub(super) fn resolve(
        self,
        include: &AnchoredPatterns,
        exclude: Option<&AnchoredPatterns>,
    ) -> CodeBlocks {
        CodeBlocks {
            include: self.include.unwrap_or_else(|| include.clone()),
            exclude: self.exclude.or_else(|| exclude.cloned()),
        }
    }
}

/// The resolved code-block settings, present only when some file in the chain wrote the table.
#[derive(Debug, Clone)]
pub(crate) struct CodeBlocks {
    /// The files code-blocks reads.
    include: AnchoredPatterns,
    /// Files removed from `include`, if any.
    exclude: Option<AnchoredPatterns>,
}

impl CodeBlocks {
    /// Whether code-blocks reads `path`: included and not excluded.
    #[must_use]
    pub(crate) fn scans(&self, path: &AbsolutePath) -> bool {
        self.include.matches(path) && !self.excludes(path)
    }

    /// Whether code-blocks' `exclude` names `path`, a file or a directory.
    #[must_use]
    pub(crate) fn excludes(&self, path: &AbsolutePath) -> bool {
        self.exclude
            .as_ref()
            .is_some_and(|exclude| exclude.matches(path))
    }
}

/// Folds one layer's table into what the nearer layers already resolved, child first: the
/// feature is on once any layer writes the table, and each key comes from the nearest layer that
/// sets it, anchored at `base`, the directory of `file`.
#[must_use]
pub(super) fn merge(
    nearer: Option<Written>,
    raw: Option<RawCodeBlocks>,
    file: &AbsolutePath,
    base: &AbsolutePath,
) -> Option<Written> {
    let Some(RawCodeBlocks { include, exclude }) = raw else {
        return nearer;
    };
    let anchor = |list| AnchoredPatterns::written_by(file.clone(), base.clone(), list);
    let nearer = nearer.unwrap_or_default();
    Some(Written {
        include: nearer.include.or_else(|| include.map(anchor)),
        exclude: nearer.exclude.or_else(|| exclude.map(anchor)),
    })
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
    fn an_unknown_key_inside_the_table_is_refused_as_a_parse_error() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks]\nlint = \"all\"\n",
        )]);

        let error = Config::load_file(&at(&dir, "supdoc.config.toml"))
            .expect_err("an unknown key inside the table should be refused");

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

    #[test]
    fn unset_keys_default_to_the_top_level_patterns() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "include = [\"docs\"]\nexclude = [\"docs/old\"]\n[features.code-blocks]\n",
        )]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");
        let code_blocks = config
            .code_blocks()
            .expect("the table turns the feature on");

        assert!(code_blocks.scans(&at(&dir, "docs/a.md")));
        assert!(!code_blocks.scans(&at(&dir, "docs/old/a.md")));
        assert!(!code_blocks.scans(&at(&dir, "skills/a.md")));
    }

    #[test]
    fn set_keys_replace_the_top_level_for_code_blocks_only() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "include = [\"docs\"]\nexclude = [\"docs/old\"]\n\
             [features.code-blocks]\ninclude = [\"skills\"]\nexclude = []\n",
        )]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");
        let code_blocks = config
            .code_blocks()
            .expect("the table turns the feature on");

        assert!(code_blocks.scans(&at(&dir, "skills/a.md")));
        assert!(!code_blocks.scans(&at(&dir, "docs/a.md")));
        assert!(
            config.scans(&at(&dir, "docs/a.md")) && !config.scans(&at(&dir, "skills/a.md")),
            "linking's scope should not change"
        );
    }

    #[test]
    fn keys_are_anchored_at_the_file_that_wrote_them() {
        let dir = tree(&[
            (
                "supdoc.config.toml",
                "[features.code-blocks]\ninclude = [\"skills\"]\n",
            ),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n",
            ),
        ]);

        let config = Config::load(&at(&dir, "apps/web")).expect("the fixture config should load");
        let code_blocks = config.code_blocks().expect("the table is inherited");

        assert!(code_blocks.scans(&at(&dir, "skills/a.md")));
        assert!(!code_blocks.scans(&at(&dir, "apps/web/skills/a.md")));
    }

    #[test]
    fn a_dot_segment_in_a_pattern_is_refused() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks]\ninclude = [\"./skills\"]\n",
        )]);

        let error = Config::load_file(&at(&dir, "supdoc.config.toml"))
            .expect_err("a `.` segment should be refused as at the top level");

        assert_eq!(error.code(), "config-parse", "got {error:?}");
        assert!(
            error
                .to_string()
                .contains("pattern `./skills` has a `.` segment; write `skills`"),
            "got {error}"
        );
    }
}
