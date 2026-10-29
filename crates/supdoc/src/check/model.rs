//! The check report: every link in scope with its state, plus findings not tied to one link (05).

use crate::finding::{Finding, Location};
use crate::link::LinkKey;
use crate::state::LinkState;

/// The result of comparing a scan with the lockfile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One per [`LinkKey`] in scope, sorted.
    pub links: Vec<LinkReport>,
    /// Findings not tied to one link: from the scan, plus moved-link notes and syntax-error
    /// warnings.
    pub findings: Vec<Finding>,
}

impl Report {
    /// How many links are in each state. Always computed from `links`, so it can't disagree with
    /// them.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.links.iter().map(|link| link.state).collect()
    }
}

/// One link and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReport {
    /// Which link.
    pub key: LinkKey,
    /// Its state.
    pub state: LinkState,
    /// Every place it appears. Empty only for `obsolete`, since that link no longer exists.
    pub locations: Vec<Location>,
    /// The finding that explains this state (none for `passed`).
    pub finding: Option<Finding>,
    /// Set when a hash was carried from an old key (05 C6).
    pub moved_from: Option<LinkKey>,
}

/// How many links are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Links whose hash matches.
    pub passed: usize,
    /// Links whose code changed.
    pub failed: usize,
    /// Links whose target can't be found.
    pub broken: usize,
    /// Links without a lock entry.
    pub new: usize,
    /// Lock entries without a link.
    pub obsolete: usize,
}

impl Counts {
    /// The count for one state.
    #[must_use]
    pub const fn get(&self, state: LinkState) -> usize {
        match state {
            LinkState::Passed => self.passed,
            LinkState::Failed => self.failed,
            LinkState::Broken => self.broken,
            LinkState::New => self.new,
            LinkState::Obsolete => self.obsolete,
        }
    }

    /// The number of links counted.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.passed + self.failed + self.broken + self.new + self.obsolete
    }
}

impl FromIterator<LinkState> for Counts {
    fn from_iter<I: IntoIterator<Item = LinkState>>(states: I) -> Self {
        let mut counts = Self::default();
        for state in states {
            let count = match state {
                LinkState::Passed => &mut counts.passed,
                LinkState::Failed => &mut counts.failed,
                LinkState::Broken => &mut counts.broken,
                LinkState::New => &mut counts.new,
                LinkState::Obsolete => &mut counts.obsolete,
            };
            *count += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(source: &str, state: LinkState) -> LinkReport {
        LinkReport {
            key: LinkKey {
                source: source.parse().expect("the fixture source should be valid"),
                target: "src/lib.rs"
                    .parse()
                    .expect("the fixture target should be valid"),
                symbol: None,
            },
            state,
            locations: Vec::new(),
            finding: None,
            moved_from: None,
        }
    }

    #[test]
    fn counts_tally_each_state_from_the_links() {
        let report = Report {
            links: vec![
                link("a.md", LinkState::Passed),
                link("b.md", LinkState::Passed),
                link("c.md", LinkState::Failed),
                link("d.md", LinkState::Obsolete),
            ],
            findings: Vec::new(),
        };

        let counts = report.counts();

        assert_eq!(
            LinkState::ALL.map(|state| counts.get(state)),
            [2, 1, 0, 0, 1],
            "counts in order passed, failed, broken, new, obsolete",
        );
        assert_eq!(counts.total(), report.links.len());
    }

    #[test]
    fn empty_report_counts_zero() {
        assert_eq!(Report::default().counts(), Counts::default());
    }
}
