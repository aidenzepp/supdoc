//! `[features.code-blocks.makefile]`: the table that turns checking Makefile code blocks on,
//! tagged `make`, `makefile`, `Makefile` or `mk`. `mak`, `bsdmake` and `mf` (Metafont) stay
//! unclaimed.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The Makefile formatters supdoc knows.
const KNOWN: &[&str] = &["mbake"];

/// The Makefile linters supdoc knows.
const LINTERS: &[&str] = &["checkmake"];

/// The `[features.code-blocks.makefile]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMakefile {
    /// `"mbake"` or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"checkmake"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the
    /// default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.makefile.mbake]`, when written.
    #[serde(default, deserialize_with = "mbake")]
    pub(super) mbake: Option<ToolTable>,
    /// `[features.code-blocks.makefile.checkmake]`, when written.
    #[serde(default, deserialize_with = "checkmake")]
    pub(super) checkmake: Option<ToolTable>,
}

/// Reads `formatter` against the Makefile's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"mbake"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Makefile, Step::Format, KNOWN)
}

/// Reads `linter` against the Makefile's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"checkmake"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Makefile, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.makefile.mbake]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn mbake<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "mbake")
}

/// Reads `[features.code-blocks.makefile.checkmake]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn checkmake<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "checkmake")
}

impl RawMakefile {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [("mbake", &self.mbake), ("checkmake", &self.checkmake)]
                .into_iter()
                .filter_map(|(name, table)| Some((name, table.as_ref()?)))
                .collect(),
        }
    }
}
