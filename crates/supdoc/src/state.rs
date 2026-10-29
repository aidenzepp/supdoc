//! The state of a link compared with the lockfile (05 C1).

use std::fmt;

/// Where one link stands, in snapshot-testing terms (05 C1).
///
/// The same values name states everywhere: in reports, in `--state` filters on `update` and
/// `list`, and in JSON output.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    clap::ValueEnum,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum LinkState {
    /// The target's hash matches the lock entry.
    Passed,
    /// The target's hash differs from the lock entry: the code changed since the doc was confirmed.
    Failed,
    /// The target file, symbol or namespace can't be found.
    Broken,
    /// The link has no lock entry yet.
    New,
    /// A lock entry whose link no longer exists.
    Obsolete,
}

impl LinkState {
    /// Every state, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Passed,
        Self::Failed,
        Self::Broken,
        Self::New,
        Self::Obsolete,
    ];

    /// The lowercase name used in output and on the command line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Broken => "broken",
            Self::New => "new",
            Self::Obsolete => "obsolete",
        }
    }
}

impl fmt::Display for LinkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum;

    use super::*;

    #[test]
    fn display_serde_and_command_line_names_agree() {
        for state in LinkState::ALL {
            let json = serde_json::to_string(&state).expect("a state should serialize");
            let parsed = LinkState::from_str(state.as_str(), false).unwrap_or_else(|error| {
                panic!("`{state}` should parse on the command line: {error}")
            });

            assert_eq!(json, format!("\"{state}\""), "JSON name for {state:?}");
            assert_eq!(parsed, state, "command-line name for {state:?}");
        }
    }

    #[test]
    fn all_lists_every_value_clap_knows() {
        assert_eq!(LinkState::value_variants(), LinkState::ALL);
    }
}
