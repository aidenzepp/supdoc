//! `[features.code-blocks.markdown]`: the table that turns checking Markdown code blocks on.

use super::{Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The Markdown formatters supdoc knows.
const FORMATTERS: &[&str] = &["rumdl-format"];

/// The Markdown linters supdoc knows.
const LINTERS: &[&str] = &["rumdl"];

/// The `[features.code-blocks.markdown]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMarkdown {
    /// `"rumdl-format"`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<String>,
    /// `"rumdl"`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<String>,
    /// `[features.code-blocks.markdown.rumdl-format]`, when written.
    #[serde(default, rename = "rumdl-format", deserialize_with = "rumdl_format")]
    pub(super) rumdl_format: Option<ToolTable>,
    /// `[features.code-blocks.markdown.rumdl]`, when written.
    #[serde(default, deserialize_with = "rumdl")]
    pub(super) rumdl: Option<ToolTable>,
}

/// Reads `formatter` against Markdown's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"rumdl-format"`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Markdown, Step::Format, FORMATTERS)
}

/// Reads `linter` against Markdown's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"rumdl"`.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Markdown, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.markdown.rumdl-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn rumdl_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "rumdl-format")
}

/// Reads `[features.code-blocks.markdown.rumdl]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn rumdl<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "rumdl")
}

impl RawMarkdown {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_deref(),
            linter: self.linter.as_deref(),
            tools: [("rumdl-format", &self.rumdl_format), ("rumdl", &self.rumdl)]
                .into_iter()
                .filter_map(|(name, table)| Some((name, table.as_ref()?)))
                .collect(),
        }
    }
}
