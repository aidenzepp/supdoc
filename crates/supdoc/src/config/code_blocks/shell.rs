//! `[features.code-blocks.shell]`: the table that turns checking shell code blocks on.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The shell formatters supdoc knows.
const KNOWN: &[&str] = &["shfmt"];

/// The shell linters supdoc knows.
const LINTERS: &[&str] = &["shellcheck"];

/// The shell dialect shfmt parses blocks as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Dialect {
    /// Bash, the default.
    #[default]
    Bash,
    /// POSIX sh.
    Posix,
    /// The `MirBSD` Korn shell.
    Mksh,
}

impl Dialect {
    /// The value of shfmt's `-ln` flag.
    #[must_use]
    pub(crate) const fn language_name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Posix => "posix",
            Self::Mksh => "mksh",
        }
    }
}

/// The `[features.code-blocks.shell]` table, exactly as written. Its keys are `formatter`,
/// `linter` and `dialect`, plus one options table per tool; any other key is refused as
/// `config-parse`, and an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawShell {
    /// `"shfmt"`, or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"shellcheck"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `bash`, `posix` or `mksh`; unset is `bash`.
    pub(super) dialect: Option<Dialect>,
    /// `[features.code-blocks.shell.shfmt]`, when written.
    #[serde(default, deserialize_with = "shfmt")]
    pub(super) shfmt: Option<ToolTable>,
    /// `[features.code-blocks.shell.shellcheck]`, when written.
    #[serde(default, deserialize_with = "shellcheck")]
    pub(super) shellcheck: Option<ToolTable>,
}

/// Reads `formatter` against shell's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"shfmt"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Shell, Step::Format, KNOWN)
}

/// Reads `linter` against shell's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"shellcheck"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Shell, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.shell.shfmt]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn shfmt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "shfmt")
}

/// Reads `[features.code-blocks.shell.shellcheck]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn shellcheck<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "shellcheck")
}

impl RawShell {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [("shfmt", &self.shfmt), ("shellcheck", &self.shellcheck)]
                .into_iter()
                .filter_map(|(name, table)| Some((name, table.as_ref()?)))
                .collect(),
        }
    }
}
