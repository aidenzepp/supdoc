//! `[features.code-blocks.typescript]`: the table that turns checking TypeScript code blocks on.

use super::{Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The TypeScript formatters supdoc knows.
const KNOWN: &[&str] = &["biome-format"];

/// The TypeScript linters supdoc knows.
const LINTERS: &[&str] = &["biome-lint"];

/// The `[features.code-blocks.typescript]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawTypeScript {
    /// `"biome-format"`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<String>,
    /// `"biome-lint"`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<String>,
    /// `[features.code-blocks.typescript.biome-format]`, when written.
    #[serde(default, rename = "biome-format", deserialize_with = "biome_format")]
    pub(super) biome_format: Option<ToolTable>,
    /// `[features.code-blocks.typescript.biome-lint]`, when written.
    #[serde(default, rename = "biome-lint", deserialize_with = "biome_lint")]
    pub(super) biome_lint: Option<ToolTable>,
}

/// Reads `formatter` against TypeScript's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-format"`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::TypeScript, Step::Format, KNOWN)
}

/// Reads `linter` against TypeScript's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-lint"`.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::TypeScript, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.typescript.biome-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-format")
}

/// Reads `[features.code-blocks.typescript.biome-lint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_lint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-lint")
}

impl RawTypeScript {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_deref(),
            linter: self.linter.as_deref(),
            tools: [
                ("biome-format", &self.biome_format),
                ("biome-lint", &self.biome_lint),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
