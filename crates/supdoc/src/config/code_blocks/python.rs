//! `[features.code-blocks.python]`: the table that turns checking Python code blocks on.

use super::{Formatter, deserialize_formatter};

/// The Python formatters supdoc knows.
const KNOWN: &[&str] = &["ruff-format", "black"];

/// The `[features.code-blocks.python]` table, exactly as written. Its one key is `formatter`; any
/// other key is refused as `config-parse`, and an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPython {
    /// `"ruff-format"`, `"black"` or `false`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Formatter>,
}

/// Reads `formatter` against Python's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but a known name or `false`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Formatter>, D::Error> {
    deserialize_formatter(d, "python", KNOWN)
}
