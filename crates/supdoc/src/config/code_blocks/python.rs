//! `[features.code-blocks.python]`: the table that turns checking Python code blocks on.

/// The `[features.code-blocks.python]` table, exactly as written. It has no keys yet, so any key
/// inside it is refused as `config-parse`; an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPython {}
