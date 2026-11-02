//! The scan result: every link in the project and what its target looks like right now.
//!
//! A scan holds facts about now. It never says "failed"; only comparing it with the lockfile
//! can say that.

use std::collections::{BTreeMap, BTreeSet};

use crate::finding::{Finding, Location, Snippet};
use crate::hash::Hash;
use crate::link::{LinkKey, SourcePath};

/// Which part of the project to scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Every source under the root.
    All,
    /// Only these files and directories, relative to the project root. Converting what
    /// the user typed into root-relative paths is the caller's job, not the scan's.
    Paths(Vec<SourcePath>),
}

impl Scope {
    /// Whether `source` is inside this scope: always for [`Scope::All`], otherwise when it is a
    /// listed file or lies under a listed directory. Matches whole path segments, so `docs`
    /// contains `docs/a.md` but not `docs2/a.md`.
    #[must_use]
    pub(crate) fn contains(&self, source: &SourcePath) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths.iter().any(|listed| lies_under(source, listed)),
        }
    }
}

/// Whether `source` is `path` itself or lies under it as a directory. Matches whole path
/// segments, so `docs` holds `docs/a.md` but not `docs2/a.md`.
fn lies_under(source: &SourcePath, path: &SourcePath) -> bool {
    source
        .as_str()
        .strip_prefix(path.as_str())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Every link found in one run, plus problems not tied to a single link.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scan {
    /// One per [`LinkKey`], sorted.
    pub(crate) links: BTreeMap<LinkKey, ScannedLink>,
    /// Problems not tied to one link (unreadable directories, excluded explicit paths, and links
    /// too malformed to have a key, like an invalid symbol), sorted. A link's own problem
    /// lives in its [`TargetState::Broken`]. Never fatal.
    pub(crate) findings: Vec<Finding>,
    /// Sources that were walked but couldn't be read. Their lock entries are never judged
    /// obsolete.
    pub(crate) unread_sources: BTreeSet<SourcePath>,
    /// Sources holding a link too malformed to have a key. Such a link may be an edit of a locked
    /// one, so none of the source's lock entries are judged obsolete while it remains.
    pub(crate) keyless_error_sources: BTreeSet<SourcePath>,
    /// Directories the walk couldn't read, relative to the root. Every source beneath one is
    /// unread too, so its lock entries are never judged obsolete either.
    pub(crate) unread_dirs: BTreeSet<SourcePath>,
}

impl Scan {
    /// Whether `source` couldn't be read: it failed to read itself, or it lies beneath a
    /// directory the walk couldn't read. Its links are unknown, so none of its lock entries can
    /// be judged.
    #[must_use]
    pub(crate) fn is_unread(&self, source: &SourcePath) -> bool {
        self.unread_sources.contains(source)
            || self.unread_dirs.iter().any(|dir| lies_under(source, dir))
    }
}

/// One link, wherever it appears in its source, and the current state of its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScannedLink {
    /// The first place this link appears in its source.
    pub(crate) first: Location,
    /// Every later place this link appears, in order.
    pub(crate) more: Vec<Location>,
    /// The source lines of the first location, captured at scan time, when they could be.
    pub(crate) snippet: Option<Snippet>,
    /// What the target looks like right now.
    pub(crate) target: TargetState,
}

impl ScannedLink {
    /// Every place this link appears in its source, first one first. Never empty.
    pub(crate) fn locations(&self) -> impl Iterator<Item = &Location> {
        std::iter::once(&self.first).chain(&self.more)
    }
}

/// What a link's target looks like right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetState {
    /// Resolved and hashed. Compared with the lockfile by 05.
    Present {
        /// The hash of the symbol's bytes, or of the whole file.
        hash: Hash,
        /// Whether the target file has syntax errors, which can make its hash unstable.
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
