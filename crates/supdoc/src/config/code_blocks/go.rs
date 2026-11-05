//! `[features.code-blocks.go]`: the table that turns checking Go code blocks on, tagged `go` or
//! `golang`.

use super::{Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The Go formatters supdoc knows.
const KNOWN: &[&str] = &["gofmt"];

/// The `[features.code-blocks.go]` table, exactly as written. Its keys are `formatter` and the
/// `gofmt` options table; any other key is refused as `config-parse`, and an empty table turns the
/// language on. gofmt reads no config of its own, so nothing here can point it at a file.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGo {
    /// `"gofmt"`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<String>,
    /// `[features.code-blocks.go.gofmt]`, when written.
    #[serde(default, deserialize_with = "gofmt")]
    pub(super) gofmt: Option<ToolTable>,
}

/// Reads `formatter` against Go's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"gofmt"`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    deserialize_choice(d, Language::Go, Step::Format, KNOWN)
}

/// Reads `[features.code-blocks.go.gofmt]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn gofmt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "gofmt")
}

impl RawGo {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_deref(),
            linter: None,
            tools: self
                .gofmt
                .as_ref()
                .map(|table| ("gofmt", table))
                .into_iter()
                .collect(),
        }
    }
}
