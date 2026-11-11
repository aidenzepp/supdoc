//! `[features.code-blocks.xml]`: the table that turns checking XML code blocks on, tagged `xml`,
//! `svg`, `xsd`, `xsl`, `xslt` or `xhtml`.
//!
//! xmllint does both steps: `xmllint-format` formats a block, `xmllint` lints it.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The XML formatters supdoc knows.
const KNOWN: &[&str] = &["xmllint-format"];

/// The XML linters supdoc knows.
const LINTERS: &[&str] = &["xmllint"];

/// The `[features.code-blocks.xml]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus xmllint's two options tables; any other key is refused as `config-parse`, and
/// an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawXml {
    /// `"xmllint-format"`, or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"xmllint"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.xml.xmllint-format]`, when written.
    #[serde(
        default,
        rename = "xmllint-format",
        deserialize_with = "xmllint_format"
    )]
    pub(super) xmllint_format: Option<ToolTable>,
    /// `[features.code-blocks.xml.xmllint]`, when written.
    #[serde(default, deserialize_with = "xmllint")]
    pub(super) xmllint: Option<ToolTable>,
}

/// Reads `formatter` against XML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"xmllint-format"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Xml, Step::Format, KNOWN)
}

/// Reads `linter` against XML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"xmllint"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Xml, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.xml.xmllint-format]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn xmllint_format<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "xmllint-format")
}

/// Reads `[features.code-blocks.xml.xmllint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn xmllint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "xmllint")
}

impl RawXml {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [
                ("xmllint-format", &self.xmllint_format),
                ("xmllint", &self.xmllint),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
