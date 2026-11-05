//! `[features.code-blocks.toml]`: the table that turns checking TOML code blocks on, tagged
//! `toml`.

use super::{Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The TOML formatters supdoc knows.
const KNOWN: &[&str] = &["tombi-format"];

/// The TOML linters supdoc knows.
const LINTERS: &[&str] = &["tombi-lint"];

/// The `[features.code-blocks.toml]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawToml {
    /// `"tombi-format"`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<String>,
    /// `"tombi-lint"`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<String>,
    /// `[features.code-blocks.toml.tombi-format]`, when written.
    #[serde(default, rename = "tombi-format", deserialize_with = "tombi_format")]
    pub(super) tombi_format: Option<ToolTable>,
    /// `[features.code-blocks.toml.tombi-lint]`, when written.
    #[serde(default, rename = "tombi-lint", deserialize_with = "tombi_lint")]
    pub(super) tombi_lint: Option<ToolTable>,
}

/// Reads `formatter` against TOML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"tombi-format"`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Toml, Step::Format, KNOWN)
}

/// Reads `linter` against TOML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"tombi-lint"`.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Toml, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.toml.tombi-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn tombi_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "tombi-format")
}

/// Reads `[features.code-blocks.toml.tombi-lint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn tombi_lint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "tombi-lint")
}

impl RawToml {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_deref(),
            linter: self.linter.as_deref(),
            tools: [
                ("tombi-format", &self.tombi_format),
                ("tombi-lint", &self.tombi_lint),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
