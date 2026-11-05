//! `[features.code-blocks.go]`: the table that turns checking Go code blocks on, tagged `go` or
//! `golang`.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool};
use crate::code_blocks::tools::Step;

/// The Go formatters supdoc knows.
const KNOWN: &[&str] = &["gofmt"];

/// The Go linters supdoc knows.
const LINTERS: &[&str] = &["go-vet"];

/// The `[features.code-blocks.go]` table, exactly as written. Its keys are `formatter` and
/// `linter`, plus one options table per tool; any other key is refused as `config-parse`, and an
/// empty table turns the language on. Neither tool reads a config of its own, so nothing here can
/// point one at a file.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGo {
    /// `"gofmt"`, or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"go-vet"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `[features.code-blocks.go.gofmt]`, when written.
    #[serde(default, deserialize_with = "gofmt")]
    pub(super) gofmt: Option<ToolTable>,
    /// `[features.code-blocks.go.go-vet]`, when written.
    #[serde(default, rename = "go-vet", deserialize_with = "go_vet")]
    pub(super) go_vet: Option<ToolTable>,
}

/// Reads `formatter` against Go's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"gofmt"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Go, Step::Format, KNOWN)
}

/// Reads `linter` against Go's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"go-vet"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Go, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.go.gofmt]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn gofmt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "gofmt")
}

/// Reads `[features.code-blocks.go.go-vet]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn go_vet<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "go-vet")
}

impl RawGo {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [("gofmt", &self.gofmt), ("go-vet", &self.go_vet)]
                .into_iter()
                .filter_map(|(name, table)| Some((name, table.as_ref()?)))
                .collect(),
        }
    }
}
