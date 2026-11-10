//! `[features.code-blocks.html]`: the table that turns checking HTML code blocks on, tagged
//! `html` or `htm`. An `xhtml` block is XML, not HTML, and stays unclaimed here.
//!
//! supdoc knows no HTML linter yet, so a block's lint step runs only when `linter` names a custom
//! command.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The HTML formatters supdoc knows.
const KNOWN: &[&str] = &["superhtml"];

/// The HTML linters supdoc knows: none yet, so only a custom command lints a block.
const LINTERS: &[&str] = &[];

/// The `[features.code-blocks.html]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus superhtml's options table; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHtml {
    /// `"superhtml"` or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// A custom `{ command = [...], diagnostics = "gnu" }`; unset leaves the lint step without a
    /// tool.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.html.superhtml]`, when written.
    #[serde(default, deserialize_with = "superhtml")]
    pub(super) superhtml: Option<ToolTable>,
}

/// Reads `formatter` against HTML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"superhtml"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Html, Step::Format, KNOWN)
}

/// Reads `linter` against HTML's known tools, of which there are none.
///
/// # Errors
///
/// The deserializer's error for any value but a valid custom command.
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

impl RawHtml {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: self
                .superhtml
                .as_ref()
                .map(|table| ("superhtml", table))
                .into_iter()
                .collect(),
        }
    }
}
