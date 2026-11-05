//! `[features.code-blocks.javascript]`: the table that turns checking JavaScript code blocks on.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The JavaScript formatters supdoc knows.
const KNOWN: &[&str] = &["biome-format", "prettier"];

/// The JavaScript linters supdoc knows.
const LINTERS: &[&str] = &["biome-lint"];

/// The `[features.code-blocks.javascript]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawJavaScript {
    /// `"biome-format"`, `"prettier"` or a custom `{ command = [...] }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"biome-lint"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.javascript.biome-format]`, when written.
    #[serde(default, rename = "biome-format", deserialize_with = "biome_format")]
    pub(super) biome_format: Option<ToolTable>,
    /// `[features.code-blocks.javascript.biome-lint]`, when written.
    #[serde(default, rename = "biome-lint", deserialize_with = "biome_lint")]
    pub(super) biome_lint: Option<ToolTable>,
    /// `[features.code-blocks.javascript.prettier]`, when written.
    #[serde(default, deserialize_with = "prettier")]
    pub(super) prettier: Option<ToolTable>,
}

/// Reads `formatter` against JavaScript's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-format"`, `"prettier"` or a valid custom
/// command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::JavaScript, Step::Format, KNOWN)
}

/// Reads `linter` against JavaScript's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-lint"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::JavaScript, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.javascript.biome-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-format")
}

/// Reads `[features.code-blocks.javascript.biome-lint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_lint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-lint")
}

/// Reads `[features.code-blocks.javascript.prettier]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn prettier<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "prettier")
}

impl RawJavaScript {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [
                ("biome-format", &self.biome_format),
                ("biome-lint", &self.biome_lint),
                ("prettier", &self.prettier),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
