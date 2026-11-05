//! `[features.code-blocks.yaml]`: the table that turns checking YAML code blocks on, tagged `yaml`
//! or `yml`.

use super::{Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The YAML formatters supdoc knows.
const KNOWN: &[&str] = &["yamlfmt"];

/// The `[features.code-blocks.yaml]` table, exactly as written. Its keys are `formatter` plus one
/// options table per tool; any other key is refused as `config-parse`, and an empty table turns
/// the language on. YAML has no linter yet.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawYaml {
    /// `"yamlfmt"`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<String>,
    /// `[features.code-blocks.yaml.yamlfmt]`, when written.
    #[serde(default, deserialize_with = "yamlfmt")]
    pub(super) yamlfmt: Option<ToolTable>,
}

/// Reads `formatter` against YAML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"yamlfmt"`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Yaml, Step::Format, KNOWN)
}

/// Reads `[features.code-blocks.yaml.yamlfmt]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn yamlfmt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "yamlfmt")
}

impl RawYaml {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_deref(),
            linter: None,
            tools: self
                .yamlfmt
                .as_ref()
                .map(|table| ("yamlfmt", table))
                .into_iter()
                .collect(),
        }
    }
}
