//! `[features.code-blocks.dockerfile]`: the table that turns checking Dockerfile code blocks on,
//! tagged `dockerfile`, `Dockerfile`, `containerfile` or `Containerfile`.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The Dockerfile formatters supdoc knows.
const KNOWN: &[&str] = &["dockerfmt"];

/// The Dockerfile linters supdoc knows.
const LINTERS: &[&str] = &["hadolint"];

/// The `[features.code-blocks.dockerfile]` table, exactly as written. Its keys are `formatter`
/// and `linter`, plus one options table per tool; any other key is refused as `config-parse`, and
/// an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDockerfile {
    /// `"dockerfmt"` or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"hadolint"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.dockerfile.dockerfmt]`, when written.
    #[serde(default, deserialize_with = "dockerfmt")]
    pub(super) dockerfmt: Option<ToolTable>,
    /// `[features.code-blocks.dockerfile.hadolint]`, when written.
    #[serde(default, deserialize_with = "hadolint")]
    pub(super) hadolint: Option<ToolTable>,
}

/// Reads `formatter` against Dockerfile's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"dockerfmt"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Dockerfile, Step::Format, KNOWN)
}

/// Reads `linter` against Dockerfile's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"hadolint"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Dockerfile, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.dockerfile.dockerfmt]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn dockerfmt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "dockerfmt")
}

/// Reads `[features.code-blocks.dockerfile.hadolint]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn hadolint<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "hadolint")
}

impl RawDockerfile {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [("dockerfmt", &self.dockerfmt), ("hadolint", &self.hadolint)]
                .into_iter()
                .filter_map(|(name, table)| Some((name, table.as_ref()?)))
                .collect(),
        }
    }
}
