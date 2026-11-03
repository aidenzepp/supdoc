//! `[features.code-blocks]`: the table that turns code-block checking on, and the one place its
//! keys live.
//!
//! The table is on when any file in the `extends` chain writes it. Each key comes from the nearest
//! file that sets it, as everywhere else in the chain.
//!
//! `include` and `exclude` choose which files code-blocks reads, independently of linking: each
//! defaults to the top-level key of the same name, and setting one never changes which files
//! linking reads.
//!
//! A language is on exactly when its table, `[features.code-blocks.rust]`, `.shell` or
//! `.python`, is written in some file of the chain; an empty table turns it on.

mod python;
mod rust;
mod shell;

use crate::path::AbsolutePath;

use self::python::RawPython;
use self::rust::RawRust;
pub(crate) use self::shell::Dialect;
use self::shell::RawShell;
use super::pattern::{AnchoredPatterns, PatternList};

/// A language supdoc has a `[features.code-blocks]` table for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    /// Rust: tagged `rust` or `rs`.
    Rust,
    /// Shell: tagged `sh`, `bash` or `shell`.
    Shell,
    /// Python: tagged `python`, `py` or `python3`.
    Python,
}

impl Language {
    /// The language a block's tag names, when it is one of the aliases above.
    #[must_use]
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "rust" | "rs" => Some(Self::Rust),
            "sh" | "bash" | "shell" => Some(Self::Shell),
            "python" | "py" | "python3" => Some(Self::Python),
            _ => None,
        }
    }
}

/// A language's `formatter` key, validated against the formatters known for that language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Formatter {
    /// `formatter = false`: the format step is off for the language.
    Off,
    /// `formatter = "<name>"`: one of the language's known formatters.
    Named(String),
}

/// Reads a `formatter` value: a name from `known`, or `false`. A `{ command = [...] }` table is
/// refused until custom commands have a trust model.
///
/// # Errors
///
/// The deserializer's error when the value is `true`, a table, an unknown name or any other type.
fn deserialize_formatter<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    language: &'static str,
    known: &'static [&'static str],
) -> Result<Option<Formatter>, D::Error> {
    use std::fmt;

    use serde::de::{Error, MapAccess, Visitor};

    struct Reader {
        language: &'static str,
        known: &'static [&'static str],
    }
    impl<'de> Visitor<'de> for Reader {
        type Value = Formatter;

        fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(out, "a {} formatter name or `false`", self.language)
        }

        fn visit_bool<E: Error>(self, value: bool) -> Result<Formatter, E> {
            if value {
                Err(E::custom(format!(
                    "`formatter = true` names no tool; write one of {} or `false`",
                    tool_list(self.known)
                )))
            } else {
                Ok(Formatter::Off)
            }
        }

        fn visit_str<E: Error>(self, value: &str) -> Result<Formatter, E> {
            if self.known.contains(&value) {
                Ok(Formatter::Named(value.to_owned()))
            } else {
                Err(E::custom(format!(
                    "`{value}` isn't a known {} formatter; the known tools are {}",
                    self.language,
                    tool_list(self.known)
                )))
            }
        }

        fn visit_map<A: MapAccess<'de>>(self, _map: A) -> Result<Formatter, A::Error> {
            Err(A::Error::custom(format!(
                "a custom formatter command needs trust and arrives in a later release; the known {} tools are {}",
                self.language,
                tool_list(self.known)
            )))
        }
    }

    deserializer
        .deserialize_any(Reader { language, known })
        .map(Some)
}

/// `known` as backticked names joined by `, `.
fn tool_list(known: &[&str]) -> String {
    known
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What some file in the chain wrote for one language.
#[derive(Debug, Clone, Default)]
struct Table {
    /// The language's table is written.
    written: bool,
    /// The nearest `formatter` written.
    formatter: Option<Formatter>,
}

impl Table {
    /// This table with `raw`'s keys behind it: the nearer value of each key wins.
    fn behind(self, written: bool, formatter: Option<Formatter>) -> Self {
        Self {
            written: self.written || written,
            formatter: self.formatter.or(formatter),
        }
    }
}

/// Which language tables the chain wrote, with their keys.
#[derive(Debug, Clone, Default)]
struct Languages {
    /// `[features.code-blocks.rust]`.
    rust: Table,
    /// `[features.code-blocks.shell]`.
    shell: Table,
    /// `[features.code-blocks.python]`.
    python: Table,
    /// The nearest shell `dialect` written.
    dialect: Option<Dialect>,
}

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
    /// `[features.code-blocks.rust]`, when written.
    rust: Option<RawRust>,
    /// `[features.code-blocks.shell]`, when written.
    shell: Option<RawShell>,
    /// `[features.code-blocks.python]`, when written.
    python: Option<RawPython>,
}

/// The table's keys merged across the chain, before the top-level defaults fill the gaps.
#[derive(Debug, Default)]
pub(super) struct Written {
    /// The nearest `include` written inside the table, anchored at the file that wrote it.
    include: Option<AnchoredPatterns>,
    /// The nearest `exclude` written inside the table, anchored at the file that wrote it.
    exclude: Option<AnchoredPatterns>,
    /// The language tables written in any layer.
    languages: Languages,
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
            languages: self.languages,
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
    /// The language tables written.
    languages: Languages,
}

impl CodeBlocks {
    /// Whether `language`'s table is written, turning its blocks' checks on.
    #[must_use]
    pub(crate) const fn checks(&self, language: Language) -> bool {
        self.table(language).written
    }

    /// `language`'s `formatter` key: `None` selects its default formatter.
    #[must_use]
    pub(crate) const fn formatter(&self, language: Language) -> Option<&Formatter> {
        self.table(language).formatter.as_ref()
    }

    /// The shell dialect shfmt parses with: `bash` unless `dialect` says otherwise.
    #[must_use]
    pub(crate) fn dialect(&self) -> Dialect {
        self.languages.dialect.unwrap_or_default()
    }

    /// `language`'s table as written.
    const fn table(&self, language: Language) -> &Table {
        match language {
            Language::Rust => &self.languages.rust,
            Language::Shell => &self.languages.shell,
            Language::Python => &self.languages.python,
        }
    }

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
    let Some(RawCodeBlocks {
        include,
        exclude,
        rust,
        shell,
        python,
    }) = raw
    else {
        return nearer;
    };
    let anchor = |list| AnchoredPatterns::written_by(file.clone(), base.clone(), list);
    let nearer = nearer.unwrap_or_default();
    Some(Written {
        include: nearer.include.or_else(|| include.map(anchor)),
        exclude: nearer.exclude.or_else(|| exclude.map(anchor)),
        languages: Languages {
            rust: nearer
                .languages
                .rust
                .behind(rust.is_some(), rust.and_then(|raw| raw.formatter)),
            dialect: nearer
                .languages
                .dialect
                .or(shell.as_ref().and_then(|raw| raw.dialect)),
            shell: nearer
                .languages
                .shell
                .behind(shell.is_some(), shell.and_then(|raw| raw.formatter)),
            python: nearer
                .languages
                .python
                .behind(python.is_some(), python.and_then(|raw| raw.formatter)),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{Dialect, Formatter, Language};
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
    fn a_written_language_table_turns_that_language_on() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks.rust]\n[features.code-blocks.python]\n",
        )]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");
        let code_blocks = config
            .code_blocks()
            .expect("a language table turns the feature on");

        assert!(code_blocks.checks(Language::Rust));
        assert!(code_blocks.checks(Language::Python));
        assert!(
            !code_blocks.checks(Language::Shell),
            "an absent table should leave its language off"
        );
    }

    #[test]
    fn a_language_table_is_inherited_through_extends() {
        let dir = tree(&[
            ("supdoc.config.toml", "[features.code-blocks.shell]\n"),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n[features.code-blocks]\n",
            ),
        ]);

        let config = Config::load(&at(&dir, "apps/web")).expect("the fixture config should load");
        let code_blocks = config.code_blocks().expect("the table is written");

        assert!(code_blocks.checks(Language::Shell));
    }

    #[test]
    fn an_unknown_key_inside_a_language_table_is_refused_as_a_parse_error() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks.shell]\nflavor = \"zsh\"\n",
        )]);

        let error = Config::load_file(&at(&dir, "supdoc.config.toml"))
            .expect_err("an unknown key inside a language table should be refused");

        assert_eq!(error.code(), "config-parse", "got {error:?}");
        assert_eq!(error.status(), ExitStatus::Config);
    }

    #[test]
    fn each_alias_names_its_language() {
        let cases = [
            ("rust", Some(Language::Rust)),
            ("rs", Some(Language::Rust)),
            ("sh", Some(Language::Shell)),
            ("bash", Some(Language::Shell)),
            ("shell", Some(Language::Shell)),
            ("python", Some(Language::Python)),
            ("py", Some(Language::Python)),
            ("python3", Some(Language::Python)),
            ("zsh", None),
        ];
        for (tag, expected) in cases {
            assert_eq!(Language::from_tag(tag), expected, "tag `{tag}`");
        }
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

    /// The error from loading `[features.code-blocks.<table>]` holding `line`.
    fn refused(table: &str, line: &str) -> String {
        let dir = tree(&[(
            "supdoc.config.toml",
            &format!("[features.code-blocks.{table}]\n{line}\n"),
        )]);
        let error = Config::load_file(&at(&dir, "supdoc.config.toml"))
            .expect_err("the value should be refused");
        assert_eq!(error.code(), "config-parse", "got {error:?}");
        assert_eq!(error.status(), ExitStatus::Config);
        error.to_string()
    }

    #[test]
    fn a_known_formatter_name_or_false_is_selected() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks.python]\nformatter = \"black\"\n\
             [features.code-blocks.rust]\nformatter = false\n",
        )]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");
        let code_blocks = config.code_blocks().expect("the table is written");

        assert_eq!(
            code_blocks.formatter(Language::Python),
            Some(&Formatter::Named("black".to_owned()))
        );
        assert_eq!(code_blocks.formatter(Language::Rust), Some(&Formatter::Off));
    }

    #[test]
    fn an_unknown_formatter_name_is_refused_naming_the_known_tools() {
        let message = refused("python", "formatter = \"autopep8\"");

        assert!(message.contains("`ruff-format`, `black`"), "got {message}");
    }

    #[test]
    fn a_formatter_from_another_language_is_refused() {
        let message = refused("rust", "formatter = \"shfmt\"");

        assert!(message.contains("`rustfmt`"), "got {message}");
    }

    #[test]
    fn a_custom_command_table_is_refused_until_trust_exists() {
        let message = refused("shell", "formatter = { command = [\"shfmt\"] }");

        assert!(message.contains("needs trust"), "got {message}");
    }

    #[test]
    fn the_shell_dialect_defaults_to_bash_and_takes_posix_and_mksh() {
        for (line, expected) in [
            ("", Dialect::Bash),
            ("dialect = \"posix\"", Dialect::Posix),
            ("dialect = \"mksh\"", Dialect::Mksh),
        ] {
            let dir = tree(&[(
                "supdoc.config.toml",
                &format!("[features.code-blocks.shell]\n{line}\n"),
            )]);
            let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");

            let found = config
                .code_blocks()
                .expect("the table is written")
                .dialect();

            assert_eq!(found, expected, "`{line}`");
        }
    }

    #[test]
    fn an_unknown_dialect_is_refused() {
        let _message = refused("shell", "dialect = \"zsh\"");
    }
}
