//! `[features.code-blocks.shell]`: the table that turns checking shell code blocks on.

use super::{Choice, deserialize_choice};

/// The shell formatters supdoc knows.
const KNOWN: &[&str] = &["shfmt"];

/// The shell linters supdoc knows.
const LINTERS: &[&str] = &["shellcheck"];

/// The shell dialect shfmt parses blocks as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Dialect {
    /// Bash, the default.
    #[default]
    Bash,
    /// POSIX sh.
    Posix,
    /// The `MirBSD` Korn shell.
    Mksh,
}

impl Dialect {
    /// The value of shfmt's `-ln` flag.
    #[must_use]
    pub(crate) const fn language_name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Posix => "posix",
            Self::Mksh => "mksh",
        }
    }
}

/// The `[features.code-blocks.shell]` table, exactly as written. Its keys are `formatter`,
/// `linter` and `dialect`; any other key is refused as `config-parse`, and an empty table turns the language
/// on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawShell {
    /// `"shfmt"` or `false`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"shellcheck"` or `false`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `bash`, `posix` or `mksh`; unset is `bash`.
    pub(super) dialect: Option<Dialect>,
}

/// Reads `formatter` against shell's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"shfmt"` or `false`.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, "shell", "formatter", KNOWN)
}

/// Reads `linter` against shell's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"shellcheck"` or `false`.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, "shell", "linter", LINTERS)
}
