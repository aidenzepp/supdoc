//! `[features.code-blocks.shell]`: the table that turns checking shell code blocks on.

/// The `[features.code-blocks.shell]` table, exactly as written. It has no keys yet, so any key
/// inside it is refused as `config-parse`; an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawShell {}
