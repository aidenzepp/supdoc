//! The scan result: every link in the project and what its target looks like right now (04).
//!
//! A scan holds facts about now. It never says "failed"; only comparing it with the lockfile
//! (05) can say that.

use std::collections::{BTreeMap, BTreeSet};

use crate::finding::{Finding, Location};
use crate::hash::Hash;
use crate::link::{LinkKey, SourcePath};

/// Which part of the project to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every source under the root.
    All,
    /// Only these files and directories (04 S10), relative to the project root. Converting what
    /// the user typed into root-relative paths is the caller's job, not the scan's.
    Paths(Vec<SourcePath>),
}

impl Scope {
    /// Whether `source` is inside this scope: always for [`Scope::All`], otherwise when it is a
    /// listed file or lies under a listed directory. Matches whole path segments, so `docs`
    /// contains `docs/a.md` but not `docs2/a.md`.
    #[must_use]
    pub fn contains(&self, source: &SourcePath) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths.iter().any(|listed| {
                let (listed, source) = (listed.as_str(), source.as_str());
                source
                    .strip_prefix(listed)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            }),
        }
    }
}

/// Every link found in one run, plus problems not tied to a single link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// One per [`LinkKey`] (04 S11), sorted.
    pub links: BTreeMap<LinkKey, ScannedLink>,
    /// Problems not tied to one link (unreadable directories, excluded explicit paths, and links
    /// too malformed to have a key, like an invalid symbol), sorted (04 S8). A link's own problem
    /// lives in its [`TargetState::Broken`]. Never fatal.
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

#[cfg(test)]
mod tests {
    #![cfg_attr(
        test,
        allow(
            clippy::missing_errors_doc,
            clippy::missing_panics_doc,
            reason = "tests report failures rather than document callable contracts"
        )
    )]

    use super::*;

    fn source(raw: &str) -> SourcePath {
        raw.parse()
            .expect("the fixture source path should be valid")
    }

    fn paths(raw: &[&str]) -> Scope {
        Scope::Paths(raw.iter().map(|path| source(path)).collect())
    }

    #[test]
    fn all_contains_every_source() {
        assert!(Scope::All.contains(&source("docs/a.md")));
    }

    #[test]
    fn a_listed_file_contains_itself_only() {
        let scope = paths(&["docs/a.md"]);
        assert!(scope.contains(&source("docs/a.md")));
        assert!(!scope.contains(&source("docs/b.md")));
    }

    #[test]
    fn a_listed_directory_contains_what_lies_under_it() {
        assert!(paths(&["docs"]).contains(&source("docs/a.md")));
    }

    #[test]
    fn a_directory_does_not_contain_a_sibling_sharing_its_prefix() {
        let scope = paths(&["docs"]);
        assert!(!scope.contains(&source("docs2/a.md")));
        assert!(!scope.contains(&source("docs.md")));
    }

    #[test]
    fn a_listed_directory_contains_nested_directories() {
        assert!(paths(&["docs"]).contains(&source("docs/guide/deep/a.md")));
        assert!(!paths(&["docs/guide"]).contains(&source("docs/a.md")));
    }

    #[test]
    fn any_listed_path_can_match() {
        let scope = paths(&["src", "docs/a.md"]);
        assert!(scope.contains(&source("src/lib.rs")));
        assert!(scope.contains(&source("docs/a.md")));
    }

    #[test]
    fn an_empty_path_list_contains_nothing() {
        assert!(!paths(&[]).contains(&source("docs/a.md")));
    }
}
