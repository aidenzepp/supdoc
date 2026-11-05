//! `[features.code-blocks.yaml]`: the table that turns checking YAML code blocks on, tagged `yaml`
//! or `yml`.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The YAML formatters supdoc knows.
const KNOWN: &[&str] = &["yamlfmt", "prettier"];

/// The YAML linters supdoc knows.
const LINTERS: &[&str] = &["yamllint"];

/// The `[features.code-blocks.yaml]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawYaml {
    /// `"yamlfmt"`, `"prettier"` or a custom `{ command = [...] }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"yamllint"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.yaml.yamlfmt]`, when written.
    #[serde(default, deserialize_with = "yamlfmt")]
    pub(super) yamlfmt: Option<ToolTable>,
    /// `[features.code-blocks.yaml.yamllint]`, when written.
    #[serde(default, deserialize_with = "yamllint")]
    pub(super) yamllint: Option<ToolTable>,
    /// `[features.code-blocks.yaml.prettier]`, when written.
    #[serde(default, deserialize_with = "prettier")]
    pub(super) prettier: Option<ToolTable>,
}

/// Reads `formatter` against YAML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"yamlfmt"`, `"prettier"` or a valid custom
/// command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Yaml, Step::Format, KNOWN)
}

/// Reads `linter` against YAML's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"yamllint"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Yaml, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.yaml.yamlfmt]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn yamlfmt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "yamlfmt")
}

/// Reads `[features.code-blocks.yaml.yamllint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn yamllint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "yamllint")
}

/// Reads `[features.code-blocks.yaml.prettier]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn prettier<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "prettier")
}

impl RawYaml {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [
                ("yamlfmt", &self.yamlfmt),
                ("yamllint", &self.yamllint),
                ("prettier", &self.prettier),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
