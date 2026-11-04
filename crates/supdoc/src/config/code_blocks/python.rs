//! `[features.code-blocks.python]`: the table that turns checking Python code blocks on.

use super::{Choice, deserialize_choice};

/// The Python formatters supdoc knows.
const KNOWN: &[&str] = &["ruff-format", "black"];

/// The Python linters supdoc knows.
const LINTERS: &[&str] = &["ruff"];

/// The `[features.code-blocks.python]` table, exactly as written. Its keys are `formatter` and
/// `linter`; any other key is refused as `config-parse`, and an empty table turns the language
/// on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPython {
    /// `"ruff-format"`, `"black"` or `false`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"ruff"` or `false`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
}

/// Reads `formatter` against Python's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but a known name or `false`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, "python", "formatter", KNOWN)
}

/// Reads `linter` against Python's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"ruff"` or `false`.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, "python", "linter", LINTERS)
}
