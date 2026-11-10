//! `[features.code-blocks.html]`: the table that turns checking HTML code blocks on, tagged
//! `html` or `htm`. An `xhtml` block is XML, not HTML, and stays unclaimed here. superhtml formats
//! a block and biome lints it.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The HTML formatters supdoc knows.
const KNOWN: &[&str] = &["superhtml"];

/// The HTML linters supdoc knows.
const LINTERS: &[&str] = &["biome-lint"];

/// The `[features.code-blocks.html]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHtml {
    /// `"superhtml"` or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"biome-lint"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.html.superhtml]`, when written.
    #[serde(default, deserialize_with = "superhtml")]
    pub(super) superhtml: Option<ToolTable>,
    /// `[features.code-blocks.html.biome-lint]`, when written.
    #[serde(default, rename = "biome-lint", deserialize_with = "biome_lint")]
    pub(super) biome_lint: Option<ToolTable>,
}

/// Reads `formatter` against HTML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"superhtml"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Html, Step::Format, KNOWN)
}

/// Reads `linter` against HTML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-lint"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Html, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.html.superhtml]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn superhtml<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "superhtml")
}

/// Reads `[features.code-blocks.html.biome-lint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_lint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-lint")
}

impl RawHtml {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [
                ("superhtml", &self.superhtml),
                ("biome-lint", &self.biome_lint),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
