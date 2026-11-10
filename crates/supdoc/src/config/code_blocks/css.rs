//! `[features.code-blocks.css]`: the table that turns checking CSS code blocks on, tagged `css`
//! or `scss`. biome reads an `scss` block as SCSS under the same `css` section of its config.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The CSS formatters supdoc knows. prettier isn't one yet: its CSS parser isn't wired.
const KNOWN: &[&str] = &["biome-format"];

/// The CSS linters supdoc knows.
const LINTERS: &[&str] = &["biome-lint"];

/// The `[features.code-blocks.css]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCss {
    /// `"biome-format"` or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"biome-lint"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.css.biome-format]`, when written.
    #[serde(default, rename = "biome-format", deserialize_with = "biome_format")]
    pub(super) biome_format: Option<ToolTable>,
    /// `[features.code-blocks.css.biome-lint]`, when written.
    #[serde(default, rename = "biome-lint", deserialize_with = "biome_lint")]
    pub(super) biome_lint: Option<ToolTable>,
}

/// Reads `formatter` against CSS's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-format"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Css, Step::Format, KNOWN)
}

/// Reads `linter` against CSS's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"biome-lint"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Css, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.css.biome-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-format")
}

/// Reads `[features.code-blocks.css.biome-lint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn biome_lint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "biome-lint")
}

impl RawCss {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
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
