//! `[features.code-blocks.python]`: the table that turns checking Python code blocks on.

use super::{Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The Python formatters supdoc knows.
const KNOWN: &[&str] = &["ruff-format", "black"];

/// The Python linters supdoc knows.
const LINTERS: &[&str] = &["ruff"];

/// The `[features.code-blocks.python]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool,; any other key is refused as `config-parse`, and an empty table turns the language
/// on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPython {
    /// `"ruff-format"` or `"black"`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<String>,
    /// `"ruff"`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<String>,
    /// `[features.code-blocks.python.ruff-format]`, when written.
    #[serde(default, rename = "ruff-format", deserialize_with = "ruff_format")]
    pub(super) ruff_format: Option<ToolTable>,
    /// `[features.code-blocks.python.black]`, when written.
    #[serde(default, deserialize_with = "black")]
    pub(super) black: Option<ToolTable>,
    /// `[features.code-blocks.python.ruff]`, when written.
    #[serde(default, deserialize_with = "ruff")]
    pub(super) ruff: Option<ToolTable>,
}

/// Reads `formatter` against Python's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but a known name.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Python, Step::Format, KNOWN)
}

/// Reads `linter` against Python's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"ruff"`.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Python, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.python.ruff-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn ruff_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "ruff-format")
}

/// Reads `[features.code-blocks.python.black]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn black<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "black")
}

/// Reads `[features.code-blocks.python.ruff]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn ruff<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "ruff")
}

impl RawPython {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_deref(),
            linter: self.linter.as_deref(),
            tools: [
                ("ruff-format", &self.ruff_format),
                ("black", &self.black),
                ("ruff", &self.ruff),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
