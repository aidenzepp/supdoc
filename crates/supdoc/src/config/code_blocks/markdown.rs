//! `[features.code-blocks.markdown]`: the table that turns checking Markdown code blocks on.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The Markdown formatters supdoc knows.
const FORMATTERS: &[&str] = &["rumdl-format", "prettier"];

/// The Markdown linters supdoc knows.
const LINTERS: &[&str] = &["rumdl"];

/// The linter name people ask for that supdoc deliberately does not ship.
const MARKDOWNLINT_CLI2: &str = "markdownlint-cli2";

/// Why `linter = "markdownlint-cli2"` is refused, and what to write instead.
const MARKDOWNLINT_CLI2_REFUSED: &str = "`markdownlint-cli2` isn't a supported Markdown linter: \
    supdoc passes each code block on stdin, and markdownlint-cli2 can't be told that input's \
    file name, so its per-directory configs and path-scoped overrides or ignores would silently \
    not apply; use the built-in `linter = \"rumdl\"`, or a custom command, \
    `linter = { command = [\"markdownlint-cli2\", \"{path}\"] }`, if you accept that limit";

/// The `[features.code-blocks.markdown]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMarkdown {
    /// `"rumdl-format"`, `"prettier"` or a custom `{ command = [...] }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"rumdl"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.markdown.rumdl-format]`, when written.
    #[serde(default, rename = "rumdl-format", deserialize_with = "rumdl_format")]
    pub(super) rumdl_format: Option<ToolTable>,
    /// `[features.code-blocks.markdown.rumdl]`, when written.
    #[serde(default, deserialize_with = "rumdl")]
    pub(super) rumdl: Option<ToolTable>,
    /// `[features.code-blocks.markdown.prettier]`, when written.
    #[serde(default, deserialize_with = "prettier")]
    pub(super) prettier: Option<ToolTable>,
}

/// Reads `formatter` against Markdown's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"rumdl-format"`, `"prettier"` or a valid custom
/// command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Markdown, Step::Format, FORMATTERS)
}

/// Reads `linter` against Markdown's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"rumdl"` or a valid custom command;
/// `"markdownlint-cli2"` gets an error saying why it is not built in.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    use serde::Deserialize as _;
    use serde::de::Error as _;

    let value = ::toml::Value::deserialize(d)?;
    if value.as_str() == Some(MARKDOWNLINT_CLI2) {
        return Err(D::Error::custom(MARKDOWNLINT_CLI2_REFUSED));
    }
    deserialize_choice(value, Language::Markdown, Step::Lint, LINTERS)
        .map_err(|error| D::Error::custom(error.message()))
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

/// Reads `[features.code-blocks.markdown.prettier]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn prettier<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "prettier")
}

impl RawMarkdown {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [
                ("rumdl-format", &self.rumdl_format),
                ("rumdl", &self.rumdl),
                ("prettier", &self.prettier),
            ]
            .into_iter()
            .filter_map(|(name, table)| Some((name, table.as_ref()?)))
            .collect(),
        }
    }
}
