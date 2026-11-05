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
//! A language is on exactly when its table, `[features.code-blocks.rust]`, `.shell`, `.python`,
//! `.typescript`, `.javascript` or `.markdown`, is written in some file of the chain; an empty
//! table turns it on. The language
//! table selects tools (`formatter`, `linter`) and holds the language's own settings; each
//! tool's options, its step's mode, live in the tool's own table,
//! `[features.code-blocks.<language>.<tool>]`, validated by its cartridge.

mod javascript;
mod markdown;
mod python;
mod rust;
mod shell;
mod typescript;

use crate::code_blocks::tools::{self, CARTRIDGES, Step, Tier};
use std::ops::Range;

use crate::path::AbsolutePath;

use self::javascript::RawJavaScript;
use self::markdown::RawMarkdown;
use self::python::RawPython;
pub(crate) use self::rust::Edition;
use self::rust::RawRust;
pub(crate) use self::shell::Dialect;
use self::shell::RawShell;
use self::typescript::RawTypeScript;
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
    /// Markdown: tagged `md` or `markdown`.
    Markdown,
    /// TypeScript: tagged `ts`, `typescript`, `mts`, `cts` or `tsx`.
    TypeScript,
    /// JavaScript: tagged `js`, `javascript`, `mjs`, `cjs` or `jsx`.
    JavaScript,
}

impl Language {
    /// The language a block's tag names, when it is one of the aliases above.
    #[must_use]
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "rust" | "rs" => Some(Self::Rust),
            "sh" | "bash" | "shell" => Some(Self::Shell),
            "python" | "py" | "python3" => Some(Self::Python),
            "md" | "markdown" => Some(Self::Markdown),
            "ts" | "typescript" | "mts" | "cts" | "tsx" => Some(Self::TypeScript),
            "js" | "javascript" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            _ => None,
        }
    }

    /// The name of its table: `rust`, `shell`, `python`, `typescript`, `javascript` or `markdown`.
    #[must_use]
    pub(crate) const fn table(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Shell => "shell",
            Self::Python => "python",
            Self::Markdown => "markdown",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
        }
    }
}

/// A step's mode, written in its tool's table as `format = "..."` or `lint = "..."`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `"full"`, the default: the step runs.
    #[default]
    Full,
    /// `"off"`: the step doesn't run, and its state is `off`.
    Off,
}

/// The key a step's mode is written under in its tool's table.
const fn mode_key(step: Step) -> &'static str {
    match step {
        Step::Format => "format",
        Step::Lint => "lint",
    }
}

/// Reads a `formatter` or `linter` value, named by `key`, for `language`: a name from `known`. A
/// `{ command = [...] }` table is refused until custom commands have a trust model, and `false`,
/// removed, names the tool table that turns the step off.
///
/// # Errors
///
/// The deserializer's error when the value is a boolean, a table, an unknown name or any other
/// type.
fn deserialize_choice<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    language: Language,
    step: Step,
    known: &'static [&'static str],
) -> Result<Option<String>, D::Error> {
    use std::fmt;

    use serde::de::{Error, MapAccess, Visitor};

    struct Reader {
        language: Language,
        step: Step,
        known: &'static [&'static str],
    }
    impl Reader {
        /// `formatter` or `linter`.
        const fn key(&self) -> &'static str {
            match self.step {
                Step::Format => "formatter",
                Step::Lint => "linter",
            }
        }
    }
    impl<'de> Visitor<'de> for Reader {
        type Value = String;

        fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(out, "a {} {} name", self.language.table(), self.key())
        }

        fn visit_bool<E: Error>(self, value: bool) -> Result<String, E> {
            let default =
                tools::default_for(self.language, self.step).map_or("<tool>", |tool| tool.name);
            Err(E::custom(format!(
                "`{} = {value}` is removed: the {} key only selects a tool, one of {}; turn the \
                 step off in the tool's own table with `[features.code-blocks.{}.{default}] {} = \"off\"`",
                self.key(),
                self.key(),
                tool_list(self.known),
                self.language.table(),
                mode_key(self.step),
            )))
        }

        fn visit_str<E: Error>(self, value: &str) -> Result<String, E> {
            if self.known.contains(&value) {
                Ok(value.to_owned())
            } else {
                Err(E::custom(format!(
                    "`{value}` isn't a known {} {}; the known tools are {}",
                    self.language.table(),
                    self.key(),
                    tool_list(self.known)
                )))
            }
        }

        fn visit_map<A: MapAccess<'de>>(self, _map: A) -> Result<String, A::Error> {
            Err(A::Error::custom(format!(
                "a custom {} command needs trust and arrives in a later release; the known {} tools are {}",
                self.key(),
                self.language.table(),
                tool_list(self.known)
            )))
        }
    }

    deserializer
        .deserialize_any(Reader {
            language,
            step,
            known,
        })
        .map(Some)
}

/// The options written in one tool's table, `[features.code-blocks.<language>.<tool>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ToolTable {
    /// Its step's mode, when written.
    mode: Option<Mode>,
    /// Where the table is, in the file that wrote it.
    span: Range<usize>,
}

/// Reads the table of the cartridge named `name`: its only key is its step's mode, `format` or
/// `lint`, whose value is one of the cartridge's `modes`.
///
/// # Errors
///
/// The deserializer's error for any other key, a value the cartridge doesn't accept, or a
/// value that isn't a table.
fn deserialize_tool<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    name: &'static str,
) -> Result<Option<ToolTable>, D::Error> {
    use serde::Deserialize as _;
    use serde::de::Error;

    let tool = CARTRIDGES
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| D::Error::custom(format!("no cartridge is named `{name}`")))?;
    let written = toml::Spanned::<toml::Table>::deserialize(deserializer)?;
    let key = mode_key(tool.step());
    let accepted = || {
        format!(
            "`{}` accepts only `{key}`, set to {}",
            tool.name,
            tool.modes
                .iter()
                .map(|mode| format!("\"{mode}\""))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    };
    let mut table = ToolTable {
        mode: None,
        span: written.span(),
    };
    for (option, value) in written.get_ref() {
        if option != key {
            return Err(D::Error::custom(format!(
                "`{option}` isn't an option of `{}`; {}",
                tool.name,
                accepted()
            )));
        }
        let Some(value) = value.as_str() else {
            return Err(D::Error::custom(format!(
                "`{key}` must be a string; {}",
                accepted()
            )));
        };
        if !tool.modes.contains(&value) {
            return Err(D::Error::custom(format!(
                "`{key} = \"{value}\"` isn't a mode of `{}`; {}",
                tool.name,
                accepted()
            )));
        }
        table.mode = Some(if value == "off" {
            Mode::Off
        } else {
            Mode::Full
        });
    }
    Ok(Some(table))
}

/// What one language table wrote: its `formatter`, its `linter`, and each tool table written,
/// by the tool's name.
pub(super) struct Selection<'raw> {
    /// `formatter`, when written.
    pub(super) formatter: Option<&'raw str>,
    /// `linter`, when written.
    pub(super) linter: Option<&'raw str>,
    /// Each tool table written.
    pub(super) tools: Vec<(&'static str, &'raw ToolTable)>,
}

/// A tool table the merged chain doesn't select its tool for: the language's merged `formatter`
/// or `linter` names another tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unselected {
    /// The file that wrote the table.
    pub(crate) file: AbsolutePath,
    /// Where the table is in `file`.
    pub(crate) span: Range<usize>,
    /// The language's table name, such as `rust` or `typescript`.
    pub(crate) language: &'static str,
    /// The tool the table is for.
    pub(crate) tool: &'static str,
    /// The selecting key, `formatter` or `linter`.
    pub(crate) key: &'static str,
    /// The tool the chain selects instead, when the language has one.
    pub(crate) selected: Option<&'static str>,
}

/// Refuses the removed `[features.code-blocks] lint` key, naming where a linter's mode lives now.
///
/// # Errors
///
/// Always, once the key is written.
fn removed_lint<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<()>, D::Error> {
    use serde::de::{Error, IgnoredAny};

    let _ignored: IgnoredAny = serde::Deserialize::deserialize(deserializer)?;
    Err(D::Error::custom(
        "`[features.code-blocks] lint` is removed: each linter's mode lives in its own table, \
         such as `[features.code-blocks.python.ruff] lint = \"off\"`",
    ))
}

/// Reads `tier`: `"static"` or `"project"`, written as a string.
///
/// # Errors
///
/// The deserializer's error for any other value; `"run"` is named as not supported yet.
fn tier<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Tier>, D::Error> {
    use serde::Deserialize as _;
    use serde::de::Error;

    let accepted = "write \"static\" or \"project\"";
    let word = String::deserialize(deserializer)
        .map_err(|_| D::Error::custom(format!("`tier` must be quoted, as a string: {accepted}")))?;
    match word.as_str() {
        "static" => Ok(Some(Tier::Static)),
        "project" => Ok(Some(Tier::Project)),
        "run" => Err(D::Error::custom(format!(
            "`tier = \"run\"` isn't supported yet; for now {accepted}"
        ))),
        other => Err(D::Error::custom(format!(
            "`{other}` isn't a tier supdoc knows; {accepted}"
        ))),
    }
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
    formatter: Option<String>,
    /// The nearest `linter` written.
    linter: Option<String>,
    /// The nearest mode written for each tool, by the tool's name.
    modes: Vec<(&'static str, Mode)>,
    /// Every tool table written in any layer, nearest first: the tool's name, the file that
    /// wrote it and where.
    tables: Vec<(&'static str, AbsolutePath, Range<usize>)>,
}

impl Table {
    /// This table with a farther layer's keys behind it: the nearer value of each key wins.
    /// `selection` is that layer's language table, written in `file`, when it writes one.
    fn behind(self, selection: Option<Selection<'_>>, file: &AbsolutePath) -> Self {
        let Some(selection) = selection else {
            return self;
        };
        let mut modes = self.modes;
        let mut tables = self.tables;
        for (name, table) in selection.tools {
            if let Some(mode) = table.mode
                && !modes.iter().any(|(nearer, _)| *nearer == name)
            {
                modes.push((name, mode));
            }
            tables.push((name, file.clone(), table.span.clone()));
        }
        Self {
            written: true,
            formatter: self.formatter.or(selection.formatter.map(str::to_owned)),
            linter: self.linter.or(selection.linter.map(str::to_owned)),
            modes,
            tables,
        }
    }

    /// The first tool table, nearest first, whose tool `language`'s merged `formatter` or
    /// `linter` (or its default) doesn't select.
    fn unselected(&self, language: Language) -> Option<Unselected> {
        self.tables.iter().find_map(|(name, file, span)| {
            let tool = CARTRIDGES.iter().find(|tool| tool.name == *name)?;
            let (key, written) = match tool.step() {
                Step::Format => ("formatter", self.formatter.as_deref()),
                Step::Lint => ("linter", self.linter.as_deref()),
            };
            let selected = written.map_or_else(
                || tools::default_for(language, tool.step()),
                |written| tools::named(language, tool.step(), written),
            );
            (selected.map(|tool| tool.name) != Some(tool.name)).then(|| Unselected {
                file: file.clone(),
                span: span.clone(),
                language: language.table(),
                tool: tool.name,
                key,
                selected: selected.map(|tool| tool.name),
            })
        })
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
    /// `[features.code-blocks.markdown]`.
    markdown: Table,
    /// `[features.code-blocks.typescript]`.
    typescript: Table,
    /// `[features.code-blocks.javascript]`.
    javascript: Table,
    /// The nearest shell `dialect` written.
    dialect: Option<Dialect>,
    /// The nearest Rust `edition` written.
    edition: Option<Edition>,
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
    /// The removed `lint` key, refused with a message naming the tool tables.
    #[serde(default, deserialize_with = "removed_lint")]
    #[expect(
        dead_code,
        reason = "a removed key kept only so writing it is refused with its replacement"
    )]
    lint: Option<()>,
    /// The most this project allows its code-block tools to load: `"static"` or `"project"`.
    #[serde(default, deserialize_with = "tier")]
    tier: Option<Tier>,
    /// `[features.code-blocks.rust]`, when written.
    rust: Option<RawRust>,
    /// `[features.code-blocks.shell]`, when written.
    shell: Option<RawShell>,
    /// `[features.code-blocks.python]`, when written.
    python: Option<RawPython>,
    /// `[features.code-blocks.markdown]`, when written.
    markdown: Option<RawMarkdown>,
    /// `[features.code-blocks.typescript]`, when written.
    typescript: Option<RawTypeScript>,
    /// `[features.code-blocks.javascript]`, when written.
    javascript: Option<RawJavaScript>,
}

/// The table's keys merged across the chain, before the top-level defaults fill the gaps.
#[derive(Debug, Default)]
pub(super) struct Written {
    /// The nearest `include` written inside the table, anchored at the file that wrote it.
    include: Option<AnchoredPatterns>,
    /// The nearest `exclude` written inside the table, anchored at the file that wrote it.
    exclude: Option<AnchoredPatterns>,
    /// The nearest `tier` written.
    tier: Option<Tier>,
    /// The language tables written in any layer.
    languages: Languages,
}

impl Written {
    /// The resolved settings, each unset key taken from the top-level `include` or `exclude`.
    ///
    /// # Errors
    ///
    /// The first tool table, Rust, shell, Python, TypeScript, JavaScript then Markdown, whose tool
    /// the merged chain doesn't select.
    pub(super) fn resolve(
        self,
        include: &AnchoredPatterns,
        exclude: Option<&AnchoredPatterns>,
    ) -> Result<CodeBlocks, Unselected> {
        let languages = &self.languages;
        let unselected = languages
            .rust
            .unselected(Language::Rust)
            .or_else(|| languages.shell.unselected(Language::Shell))
            .or_else(|| languages.python.unselected(Language::Python))
            .or_else(|| languages.typescript.unselected(Language::TypeScript))
            .or_else(|| languages.javascript.unselected(Language::JavaScript))
            .or_else(|| languages.markdown.unselected(Language::Markdown));
        if let Some(unselected) = unselected {
            return Err(unselected);
        }
        Ok(CodeBlocks {
            include: self.include.unwrap_or_else(|| include.clone()),
            exclude: self.exclude.or_else(|| exclude.cloned()),
            tier: self.tier.unwrap_or(Tier::Project),
            languages: self.languages,
        })
    }
}

/// The resolved code-block settings, present only when some file in the chain wrote the table.
#[derive(Debug, Clone)]
pub(crate) struct CodeBlocks {
    /// The files code-blocks reads.
    include: AnchoredPatterns,
    /// Files removed from `include`, if any.
    exclude: Option<AnchoredPatterns>,
    /// The most tools may load: the nearest `tier` written, else [`Tier::Project`].
    tier: Tier,
    /// The language tables written.
    languages: Languages,
}

impl CodeBlocks {
    /// Whether `language`'s table is written, turning its blocks' checks on.
    #[must_use]
    pub(crate) const fn checks(&self, language: Language) -> bool {
        self.table(language).written
    }

    /// The `tier` ceiling the project set, [`Tier::Project`] when it set none.
    #[must_use]
    pub(crate) const fn tier(&self) -> Tier {
        self.tier
    }

    /// `language`'s `formatter` key: `None` selects its default formatter.
    #[must_use]
    pub(crate) fn formatter(&self, language: Language) -> Option<&str> {
        self.table(language).formatter.as_deref()
    }

    /// `language`'s `linter` key: `None` selects its default linter.
    #[must_use]
    pub(crate) fn linter(&self, language: Language) -> Option<&str> {
        self.table(language).linter.as_deref()
    }

    /// The mode of `tool`'s step for `language`: the nearest one written in
    /// `[features.code-blocks.<language>.<tool>]`, else `full`.
    #[must_use]
    pub(crate) fn mode(&self, language: Language, tool: &str) -> Mode {
        self.table(language)
            .modes
            .iter()
            .find(|(name, _)| *name == tool)
            .map(|(_, mode)| *mode)
            .unwrap_or_default()
    }

    /// The Rust `edition` key, when written.
    #[must_use]
    pub(crate) const fn edition(&self) -> Option<Edition> {
        self.languages.edition
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
            Language::Markdown => &self.languages.markdown,
            Language::TypeScript => &self.languages.typescript,
            Language::JavaScript => &self.languages.javascript,
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
/// sets it, anchored at `base`, the directory of `file`. A tool's mode is a key of its own: a
/// nearer layer's mode for one tool leaves a farther layer's mode for another standing.
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
        tier,
        rust,
        shell,
        python,
        markdown,
        typescript,
        javascript,
        ..
    }) = raw
    else {
        return nearer;
    };
    let anchor = |list| AnchoredPatterns::written_by(file.clone(), base.clone(), list);
    let nearer = nearer.unwrap_or_default();
    Some(Written {
        include: nearer.include.or_else(|| include.map(anchor)),
        exclude: nearer.exclude.or_else(|| exclude.map(anchor)),
        tier: nearer.tier.or(tier),
        languages: Languages {
            edition: nearer
                .languages
                .edition
                .or(rust.as_ref().and_then(|raw| raw.edition)),
            rust: nearer
                .languages
                .rust
                .behind(rust.as_ref().map(RawRust::selection), file),
            dialect: nearer
                .languages
                .dialect
                .or(shell.as_ref().and_then(|raw| raw.dialect)),
            shell: nearer
                .languages
                .shell
                .behind(shell.as_ref().map(RawShell::selection), file),
            python: nearer
                .languages
                .python
                .behind(python.as_ref().map(RawPython::selection), file),
            markdown: nearer
                .languages
                .markdown
                .behind(markdown.as_ref().map(RawMarkdown::selection), file),
            typescript: nearer
                .languages
                .typescript
                .behind(typescript.as_ref().map(RawTypeScript::selection), file),
            javascript: nearer
                .languages
                .javascript
                .behind(javascript.as_ref().map(RawJavaScript::selection), file),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{Dialect, Edition, Language, Mode, Tier};
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
            "[features.code-blocks]\ncolour = \"all\"\n",
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
    fn the_child_edition_overrides_the_parent_and_the_parent_holds_when_the_child_is_silent() {
        let dir = tree(&[
            (
                "supdoc.config.toml",
                "[features.code-blocks.rust]\nedition = \"2018\"\n",
            ),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n[features.code-blocks.rust]\nedition = \"2021\"\n",
            ),
            (
                "apps/api/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n[features.code-blocks]\n",
            ),
        ]);

        let overridden = Config::load(&at(&dir, "apps/web")).expect("the child config should load");
        let inherited = Config::load(&at(&dir, "apps/api")).expect("the silent child should load");

        assert_eq!(
            overridden
                .code_blocks()
                .and_then(super::CodeBlocks::edition),
            Some(Edition::from_year("2021").expect("2021 is an edition")),
            "the child's edition should win"
        );
        assert_eq!(
            inherited.code_blocks().and_then(super::CodeBlocks::edition),
            Some(Edition::from_year("2018").expect("2018 is an edition")),
            "the parent's edition should hold"
        );
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
    fn a_known_formatter_name_is_selected() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks.python]\nformatter = \"black\"\n",
        )]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");
        let code_blocks = config.code_blocks().expect("the table is written");

        assert_eq!(code_blocks.formatter(Language::Python), Some("black"));
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
    fn each_language_selects_its_known_linter() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks.rust]\nlinter = \"clippy\"\n\
             [features.code-blocks.shell]\nlinter = \"shellcheck\"\n\
             [features.code-blocks.python]\nlinter = \"ruff\"\n",
        )]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");
        let code_blocks = config.code_blocks().expect("the table is written");

        assert_eq!(code_blocks.linter(Language::Rust), Some("clippy"));
        assert_eq!(code_blocks.linter(Language::Shell), Some("shellcheck"));
        assert_eq!(code_blocks.linter(Language::Python), Some("ruff"));
    }

    #[test]
    fn an_unset_linter_takes_the_default_and_the_child_overrides_the_parent() {
        let dir = tree(&[
            (
                "supdoc.config.toml",
                "[features.code-blocks.python]\nformatter = \"black\"\n",
            ),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n[features.code-blocks.python]\nformatter = \"ruff-format\"\n",
            ),
            (
                "apps/api/supdoc.config.toml",
                "[features.code-blocks.python]\n",
            ),
        ]);

        let child = Config::load(&at(&dir, "apps/web")).expect("the child config should load");
        let unset = Config::load(&at(&dir, "apps/api")).expect("the unset config should load");

        assert_eq!(
            child
                .code_blocks()
                .and_then(|blocks| blocks.formatter(Language::Python)),
            Some("ruff-format"),
            "the child's formatter should win"
        );
        assert_eq!(
            unset
                .code_blocks()
                .and_then(|blocks| blocks.linter(Language::Python)),
            None,
            "an unset linter should take the default"
        );
    }

    #[test]
    fn an_unknown_linter_name_is_refused_naming_the_known_tools() {
        let message = refused("python", "linter = \"pylint\"");

        assert!(message.contains("`ruff`"), "got {message}");
    }

    #[test]
    fn a_linter_from_another_language_is_refused() {
        let message = refused("shell", "linter = \"clippy\"");

        assert!(message.contains("`shellcheck`"), "got {message}");
    }

    #[test]
    fn a_custom_linter_command_table_or_true_is_refused() {
        let command = refused("rust", "linter = { command = [\"clippy\"] }");
        let _true = refused("rust", "linter = true");

        assert!(command.contains("needs trust"), "got {command}");
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

    #[test]
    fn a_tool_table_sets_its_step_mode_and_an_unwritten_mode_is_full() {
        let dir = tree(&[(
            "supdoc.config.toml",
            "[features.code-blocks.python.ruff]\nlint = \"off\"\n\
             [features.code-blocks.rust.rustfmt]\nformat = \"full\"\n\
             [features.code-blocks.shell]\n",
        )]);

        let config = Config::load(&at(&dir, ".")).expect("the fixture config should load");
        let code_blocks = config.code_blocks().expect("the table is written");

        assert!(
            code_blocks.checks(Language::Python),
            "a tool table turns its language on"
        );
        assert_eq!(code_blocks.mode(Language::Python, "ruff"), Mode::Off);
        assert_eq!(
            code_blocks.mode(Language::Python, "ruff-format"),
            Mode::Full
        );
        assert_eq!(code_blocks.mode(Language::Rust, "rustfmt"), Mode::Full);
        assert_eq!(code_blocks.mode(Language::Shell, "shellcheck"), Mode::Full);
    }

    #[test]
    fn a_table_for_a_tool_the_language_does_not_select_is_refused() {
        let unselected = refused("python.black", "format = \"off\"");
        let other = refused(
            "python",
            "formatter = \"black\"\n[features.code-blocks.python.ruff-format]\nformat = \"off\"",
        );

        assert!(
            unselected.contains("[features.code-blocks.python.black]"),
            "got {unselected}"
        );
        assert!(
            other.contains("[features.code-blocks.python.ruff-format]"),
            "got {other}"
        );
    }

    #[test]
    fn a_child_may_set_options_for_the_tool_its_parent_selects() {
        let dir = tree(&[
            (
                "supdoc.config.toml",
                "[features.code-blocks.python]\nformatter = \"black\"\n",
            ),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n\
                 [features.code-blocks.python.black]\nformat = \"off\"\n",
            ),
        ]);

        let config = Config::load(&at(&dir, "apps/web"))
            .expect("the parent's selection should admit the child's table");
        let code_blocks = config.code_blocks().expect("the table is written");

        assert_eq!(code_blocks.mode(Language::Python, "black"), Mode::Off);
    }

    #[test]
    fn a_child_that_selects_another_tool_cannot_keep_options_for_the_parents() {
        let dir = tree(&[
            (
                "supdoc.config.toml",
                "[features.code-blocks.python]\nformatter = \"black\"\n",
            ),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n\
                 [features.code-blocks.python]\nformatter = \"ruff-format\"\n\
                 [features.code-blocks.python.black]\nformat = \"off\"\n",
            ),
        ]);

        let error = Config::load(&at(&dir, "apps/web"))
            .expect_err("the merged selection doesn't select black");

        assert_eq!(error.code(), "config-parse", "got {error:?}");
        assert_eq!(error.status(), ExitStatus::Config);
        let message = error.to_string();
        assert!(
            message.contains("apps/web/supdoc.config.toml")
                && message.contains("[features.code-blocks.python.black]"),
            "the message should name the child's file and the table: {message}"
        );
    }

    #[test]
    fn a_key_or_value_the_cartridge_does_not_accept_is_refused_naming_what_it_accepts() {
        let wrong_step = refused("rust.rustfmt", "lint = \"off\"");
        let unknown_key = refused("shell.shellcheck", "severity = \"error\"");
        let partial = refused("rust.clippy", "lint = \"partial\"");

        assert!(
            wrong_step.contains("`lint` isn't an option of `rustfmt`")
                && wrong_step
                    .contains("`rustfmt` accepts only `format`, set to \"full\" or \"off\""),
            "got {wrong_step}"
        );
        assert!(
            unknown_key.contains("`shellcheck` accepts only `lint`, set to \"full\" or \"off\""),
            "got {unknown_key}"
        );
        assert!(
            partial.contains("`lint = \"partial\"` isn't a mode of `clippy`"),
            "got {partial}"
        );
    }

    /// The error from loading a config file holding `text`.
    fn load_error(text: &str) -> crate::config::ConfigError {
        let dir = tree(&[("supdoc.config.toml", text)]);
        Config::load_file(&at(&dir, "supdoc.config.toml"))
            .expect_err("the config should be refused")
    }

    #[test]
    fn the_removed_keys_are_parse_errors_whose_help_names_the_tool_table() {
        let cases = [
            (
                "[features.code-blocks.rust]\nformatter = false\n",
                "[features.code-blocks.rust.rustfmt]",
                "format = \"off\"",
            ),
            (
                "[features.code-blocks.python]\nlinter = false\n",
                "[features.code-blocks.python.ruff]",
                "lint = \"off\"",
            ),
            (
                "[features.code-blocks]\nlint = \"all\"\n",
                "[features.code-blocks.python.ruff]",
                "lint = \"off\"",
            ),
        ];
        for (text, table, key) in cases {
            let error = load_error(text);
            let help = error.help().unwrap_or_default();

            assert_eq!(error.code(), "config-parse", "`{text}`: {error:?}");
            assert_eq!(error.status(), ExitStatus::Config, "`{text}`");
            assert!(
                help.contains(table) && help.contains(key),
                "`{text}`: the help should name {table} and {key}: {help}"
            );
        }
    }

    #[test]
    fn extends_merges_tool_tables_and_the_child_mode_overrides_the_parent() {
        let dir = tree(&[
            (
                "supdoc.config.toml",
                "[features.code-blocks.python.ruff]\nlint = \"off\"\n\
                 [features.code-blocks.python.ruff-format]\nformat = \"off\"\n",
            ),
            (
                "apps/web/supdoc.config.toml",
                "extends = \"../../supdoc.config.toml\"\n\
                 [features.code-blocks.python.ruff]\nlint = \"full\"\n",
            ),
        ]);

        let config = Config::load(&at(&dir, "apps/web")).expect("the child config should load");
        let code_blocks = config.code_blocks().expect("the table is inherited");

        assert_eq!(
            code_blocks.mode(Language::Python, "ruff"),
            Mode::Full,
            "the child's mode should win"
        );
        assert_eq!(
            code_blocks.mode(Language::Python, "ruff-format"),
            Mode::Off,
            "the parent's mode for another tool should hold"
        );
    }

    /// The `tier` the config in `files` resolves for `apps/web`, or the root when absent.
    fn tier_of(files: &[(&str, &str)], at_dir: &str) -> Tier {
        let dir = tree(files);
        let config = Config::load(&at(&dir, at_dir)).expect("the fixture config should load");
        config.code_blocks().expect("the table is written").tier()
    }

    #[test]
    fn tier_defaults_to_project() {
        let tier = tier_of(&[("supdoc.config.toml", "[features.code-blocks]\n")], ".");

        assert_eq!(tier, Tier::Project);
    }

    #[test]
    fn each_tier_value_is_read() {
        for (word, expected) in [("static", Tier::Static), ("project", Tier::Project)] {
            let text = format!("[features.code-blocks]\ntier = \"{word}\"\n");

            assert_eq!(
                tier_of(&[("supdoc.config.toml", &text)], "."),
                expected,
                "tier = \"{word}\""
            );
        }
    }

    #[test]
    fn a_child_tier_overrides_the_parent_and_a_silent_child_inherits_it() {
        let parent = "[features.code-blocks]\ntier = \"static\"\n";
        let loud =
            "extends = \"../../supdoc.config.toml\"\n[features.code-blocks]\ntier = \"project\"\n";
        let silent = "extends = \"../../supdoc.config.toml\"\n[features.code-blocks]\n";

        assert_eq!(
            tier_of(
                &[
                    ("supdoc.config.toml", parent),
                    ("apps/web/supdoc.config.toml", loud)
                ],
                "apps/web"
            ),
            Tier::Project,
            "the child's tier should win"
        );
        assert_eq!(
            tier_of(
                &[
                    ("supdoc.config.toml", parent),
                    ("apps/web/supdoc.config.toml", silent)
                ],
                "apps/web"
            ),
            Tier::Static,
            "the parent's tier should hold"
        );
    }

    #[test]
    fn tier_run_is_refused_saying_running_is_not_supported_yet() {
        let message = refused_tier("\"run\"");

        assert!(
            message.contains("isn't supported yet")
                && message.contains("\"static\" or \"project\""),
            "got {message}"
        );
    }

    #[test]
    fn an_unknown_or_non_string_tier_is_refused_naming_the_accepted_values() {
        for value in ["\"trusted\"", "3", "true"] {
            let message = refused_tier(value);

            assert!(
                message.contains("\"static\" or \"project\""),
                "tier = {value}: got {message}"
            );
        }
    }

    /// The error from loading `tier = <value>`.
    fn refused_tier(value: &str) -> String {
        let dir = tree(&[(
            "supdoc.config.toml",
            &format!("[features.code-blocks]\ntier = {value}\n"),
        )]);
        let error = Config::load_file(&at(&dir, "supdoc.config.toml"))
            .expect_err("the value should be refused");
        assert_eq!(error.code(), "config-parse", "got {error:?}");
        assert_eq!(error.status(), ExitStatus::Config);
        error.to_string()
    }
}
