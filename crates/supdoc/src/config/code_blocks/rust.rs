//! `[features.code-blocks.rust]`: the table that turns checking Rust code blocks on.

use super::{Choice, Language, Selection, ToolTable, deserialize_choice, deserialize_tool, shown};
use crate::code_blocks::tools::Step;

/// The Rust formatters supdoc knows.
const KNOWN: &[&str] = &["rustfmt"];

/// The Rust linters supdoc knows.
const LINTERS: &[&str] = &["clippy"];

/// A Rust edition rustfmt accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edition {
    /// `2015`.
    E2015,
    /// `2018`.
    E2018,
    /// `2021`.
    E2021,
    /// `2024`.
    E2024,
}

impl Edition {
    /// The edition `year` names, when it is `2015`, `2018`, `2021` or `2024`.
    #[must_use]
    pub(crate) fn from_year(year: &str) -> Option<Self> {
        match year {
            "2015" => Some(Self::E2015),
            "2018" => Some(Self::E2018),
            "2021" => Some(Self::E2021),
            "2024" => Some(Self::E2024),
            _ => None,
        }
    }

    /// The year, as `--edition` takes it.
    #[must_use]
    pub(crate) const fn year(self) -> &'static str {
        match self {
            Self::E2015 => "2015",
            Self::E2018 => "2018",
            Self::E2021 => "2021",
            Self::E2024 => "2024",
        }
    }
}

/// The `[features.code-blocks.rust]` table, exactly as written. Its keys are `formatter`,
/// `linter` and `edition`, plus one options table per tool; any other key is refused as
/// `config-parse`, and an empty table turns the language on.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRust {
    /// `"rustfmt"`, or a custom `{ command = [...] }`; unset takes the default.
    #[serde(default, deserialize_with = "formatter")]
    pub(super) formatter: Option<Choice>,
    /// `"clippy"`, or a custom `{ command = [...], diagnostics = "gnu" }`; unset takes the default.
    #[serde(default, deserialize_with = "linter")]
    pub(super) linter: Option<Choice>,
    /// `"2015"`, `"2018"`, `"2021"` or `"2024"`; unset reads the root's `Cargo.toml`.
    #[serde(default, deserialize_with = "edition")]
    pub(super) edition: Option<Edition>,
    /// `[features.code-blocks.rust.rustfmt]`, when written.
    #[serde(default, deserialize_with = "rustfmt")]
    pub(super) rustfmt: Option<ToolTable>,
    /// `[features.code-blocks.rust.clippy]`, when written.
    #[serde(default, deserialize_with = "clippy")]
    pub(super) clippy: Option<ToolTable>,
}

/// Reads `edition`: one of the four editions rustfmt takes, written as a string.
///
/// # Errors
///
/// The deserializer's error for any other value.
fn edition<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Edition>, D::Error> {
    use serde::Deserialize as _;
    use serde::de::Error;

    let year = String::deserialize(d).map_err(|_| {
        D::Error::custom(
            "`edition` must be quoted, as a string: write \"2015\", \"2018\", \"2021\" or \"2024\"",
        )
    })?;
    Edition::from_year(&year).map(Some).ok_or_else(|| {
        D::Error::custom(format!(
            "`{}` isn't a Rust edition supdoc knows; write \"2015\", \"2018\", \"2021\" or \"2024\"",
            shown(&year)
        ))
    })
}

/// Reads `formatter` against Rust's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"rustfmt"` or a valid custom command.
fn formatter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Rust, Step::Format, KNOWN)
}

/// Reads `linter` against Rust's known tools.
///
/// # Errors
///
/// The deserializer's error for any value but `"clippy"` or a valid custom command.
fn linter<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Choice>, D::Error> {
    deserialize_choice(d, Language::Rust, Step::Lint, LINTERS)
}

/// Reads `[features.code-blocks.rust.rustfmt]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn rustfmt<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "rustfmt")
}

/// Reads `[features.code-blocks.rust.clippy]` against its cartridge.
///
/// # Errors
///
/// The deserializer's error for a key or value the cartridge doesn't accept.
fn clippy<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ToolTable>, D::Error> {
    deserialize_tool(d, "clippy")
}

impl RawRust {
    /// What this table selects and the tool tables it writes.
    pub(super) fn selection(&self) -> Selection<'_> {
        Selection {
            formatter: self.formatter.as_ref(),
            linter: self.linter.as_ref(),
            tools: [("rustfmt", &self.rustfmt), ("clippy", &self.clippy)]
                .into_iter()
                .filter_map(|(name, table)| Some((name, table.as_ref()?)))
                .collect(),
        }
    }
}
