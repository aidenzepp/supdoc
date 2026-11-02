//! The check report: every link in scope with its state, plus findings not tied to one link.

use crate::finding::{Finding, Location};
use crate::link::LinkKey;
use crate::state::LinkState;

/// The result of comparing a scan with the lockfile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// One per [`LinkKey`] in scope, sorted.
    pub(crate) links: Vec<LinkReport>,
    /// Findings not tied to one link: from the scan, plus moved-link notes and syntax-error
    /// warnings.
    pub(crate) findings: Vec<Finding>,
}

/// One link and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkReport {
    /// Which link.
    pub(crate) key: LinkKey,
    /// Its state.
    pub(crate) state: LinkState,
    /// Every place it appears. Empty only for `obsolete`, since that link no longer exists.
    pub(crate) locations: Vec<Location>,
    /// The finding that explains this state (none for `passed`).
    pub(crate) finding: Option<Finding>,
    /// Set when a hash was carried from an old key.
    pub(crate) moved_from: Option<LinkKey>,
}
