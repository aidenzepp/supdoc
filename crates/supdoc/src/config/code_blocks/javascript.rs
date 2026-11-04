//! `[features.code-blocks.javascript]`: the table that turns checking JavaScript code blocks on.

use super::{Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The JavaScript formatters supdoc knows.
const KNOWN: &[&str] = &["biome-format"];

/// The `[features.code-blocks.javascript]` table, exactly as written. Its keys are `formatter`, plus one
/// options table per tool; any other key is refused as `config-parse`, and an empty table turns
/// the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawJavaScript {
    /// `"biome-format"`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<String>,
    /// `[features.code-blocks.javascript.biome-format]`, when written.
    #[serde(default, rename = "biome-format", deserialize_with = "biome_format")]
    pub(super) biome_format: Option<ToolTable>,
}

/// Reads `formatter` against JavaScript's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-format"`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::JavaScript, Step::Format, KNOWN)
}

/// Reads `[features.code-blocks.javascript.biome-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-format")
}

impl RawJavaScript {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_deref(),
            linter: None,
            tools: self
                .biome_format
                .as_ref()
                .map(|table| ("biome-format", table))
                .into_iter()
                .collect(),
        }
    }
}
