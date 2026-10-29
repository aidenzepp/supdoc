//! The scan result: every link in the project and what its target looks like right now (04).
//!
//! A scan holds facts about now. It never says "failed"; only comparing it with the lockfile
//! (05) can say that.

use std::collections::{BTreeMap, BTreeSet};

use crate::finding::{Finding, Location};
use crate::hash::Hash;
use crate::link::{LinkKey, SourcePath};
use crate::path::AbsolutePath;

/// Which part of the project to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every source under the root.
    All,
    /// Only these files and directories (04 S10). Absolute and normalized.
    Paths(Vec<AbsolutePath>),
}

/// Every link found in one run, plus problems not tied to a single link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// One per [`LinkKey`] (04 S11), sorted.
    pub links: BTreeMap<LinkKey, ScannedLink>,
    /// Problems not tied to one link (unreadable directories, excluded explicit paths), sorted
    /// (04 S8). A link's own problem lives in its [`TargetState::Broken`]. Never fatal.
    pub findings: Vec<Finding>,
    /// Sources that were walked but couldn't be read. Their lock entries are never judged
    /// obsolete (05 C5).
    pub unread_sources: BTreeSet<SourcePath>,
}

/// One link, wherever it appears in its source, and the current state of its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedLink {
    /// Every place this link appears in its source. Never empty.
    pub locations: Vec<Location>,
    /// What the target looks like right now.
    pub target: TargetState,
}

/// What a link's target looks like right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetState {
    /// Resolved and hashed. Compared with the lockfile by 05.
    Present {
        /// The hash of the symbol's bytes, or of the whole file.
        hash: Hash,
        /// Whether the target file has syntax errors, which can make its hash unstable (03 R8).
        syntax_errors: bool,
    },
    /// The file, the symbol or the namespace can't be found. The finding explains.
    Broken {
        /// Why the target can't be found.
        finding: Finding,
    },
}
