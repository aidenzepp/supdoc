//! `[features.code-blocks.rust]`: the table that turns checking Rust code blocks on.

use super::{Formatter, deserialize_formatter};

/// The Rust formatters supdoc knows.
const KNOWN: &[&str] = &["rustfmt"];

/// The `[features.code-blocks.rust]` table, exactly as written. Its one key is `formatter`; any
/// other key is refused as `config-parse`, and an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRust {
    /// `"rustfmt"` or `false`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Formatter>,
}

/// Reads `formatter` against Rust's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"rustfmt"` or `false`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Formatter>, D::Error> {
    deserialize_formatter(d, "rust", KNOWN)
}
