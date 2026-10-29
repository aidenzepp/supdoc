//! Reconciliation: the pure join of a scan and the lockfile that gives every link its state (05).

use std::collections::BTreeSet;

use super::model::{LinkReport, Report};
use super::moves::{Moves, detect_moves};
use crate::finding::{Finding, Severity};
use crate::hash::Hash;
use crate::link::{LinkKey, LinkRef, Target};
use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
use crate::scan::model::{Scan, ScannedLink, Scope, TargetState};
use crate::state::LinkState;

/// Whether `check` passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing needs attention.
    Clean,
    /// At least one link fails the check.
    Failing,
}

/// Gives every [`LinkKey`] in `scope` the state the 05 table says.
///
/// Pure: reads nothing but its arguments. Links outside `scope` are not judged, and neither are
/// lock entries written in sources that couldn't be read this run (05 C5).
#[must_use]
pub fn reconcile(scan: &Scan, lock: &Lockfile, scope: &Scope) -> Report {
    let scanned: Vec<(&LinkKey, &ScannedLink)> = scan
        .links
        .iter()
        .filter(|(key, _)| scope.contains(&key.source))
        .collect();

    let unlocked: Vec<&LinkKey> = scanned
        .iter()
        .filter(|(key, link)| {
            matches!(link.target, TargetState::Present { .. }) && lock.get(key).is_none()
        })
        .map(|&(key, _)| key)
        .collect();
    let removed: Vec<&LinkKey> = lock
        .iter()
        .map(|(key, _)| key)
        .filter(|key| scope.contains(&key.source))
        .filter(|key| !scan.links.contains_key(key))
        .filter(|key| !scan.unread_sources.contains(&key.source))
        .collect();
    let moves = detect_moves(&unlocked, &removed);

    let mut report = Report {
        links: Vec::new(),
        findings: scan.findings.clone(),
    };
    let mut syntax_error_targets: BTreeSet<&Target> = BTreeSet::new();

    for (key, link) in scanned {
        let (state, finding) = match &link.target {
            TargetState::Broken { finding } => (LinkState::Broken, Some(finding.clone())),
            TargetState::Present {
                hash,
                syntax_errors,
            } => {
                if *syntax_errors {
                    let _new = syntax_error_targets.insert(&key.target);
                }
                present_state(key, link, *hash, lock, &moves, &mut report.findings)
            }
        };
        report.links.push(LinkReport {
            key: key.clone(),
            state,
            locations: link.locations.clone(),
            finding,
            moved_from: moves.carried.get(key).cloned(),
        });
    }

    let used_up: BTreeSet<&LinkKey> = moves.carried.values().collect();
    for key in removed.into_iter().filter(|key| !used_up.contains(key)) {
        report.links.push(LinkReport {
            key: key.clone(),
            state: LinkState::Obsolete,
            locations: Vec::new(),
            finding: Some(obsolete_finding(key)),
            moved_from: None,
        });
    }
    report.links.sort_by(|a, b| a.key.cmp(&b.key));

    report
        .findings
        .extend(syntax_error_targets.into_iter().map(syntax_error_finding));
    report
}

/// Whether `check` passes (05 C3): `failed`, `broken` and `new` links fail it. `obsolete` links
/// only warn, and `deny_warnings` makes them, and every warning finding, fail it too.
#[must_use]
pub fn check_outcome(report: &Report, deny_warnings: bool) -> Outcome {
    let failing_link = report.links.iter().any(|link| match link.state {
        LinkState::Failed | LinkState::Broken | LinkState::New => true,
        LinkState::Obsolete => deny_warnings,
        LinkState::Passed => false,
    });
    let denied_warning = deny_warnings
        && report
            .findings
            .iter()
            .any(|finding| finding.severity == Severity::Warning);

    if failing_link || denied_warning {
        Outcome::Failing
    } else {
        Outcome::Clean
    }
}

/// The state of a link whose target exists, plus the finding that explains it. Pushes the notes
/// that go with a carried or refused move onto `notes`.
fn present_state(
    key: &LinkKey,
    link: &ScannedLink,
    hash: Hash,
    lock: &Lockfile,
    moves: &Moves,
    notes: &mut Vec<Finding>,
) -> (LinkState, Option<Finding>) {
    let carried_from = moves.carried.get(key);
    let recorded = lock
        .get(key)
        .or_else(|| carried_from.and_then(|old| lock.get(old)));

    if let Some(old) = carried_from {
        notes.push(moved_note(key, link, old));
    }
    match recorded {
        Some(recorded) if recorded == hash => (LinkState::Passed, None),
        Some(_) => (LinkState::Failed, Some(failed_finding(key, link))),
        None => {
            if let Some(candidates) = moves.ambiguous.get(key) {
                notes.push(move_ambiguous_note(key, link, candidates));
            }
            (LinkState::New, Some(new_finding(link)))
        }
    }
}

/// What the link points at, without its source.
fn reference_of(key: &LinkKey) -> LinkRef {
    LinkRef {
        target: key.target.clone(),
        symbol: key.symbol.clone(),
    }
}

/// `docs/guide.md:3 → src/lib.rs#add`: where the link is written, then what it points at.
fn describe(key: &LinkKey, link: &ScannedLink) -> String {
    let reference = reference_of(key);
    let written_at = link.locations.first().map_or_else(
        || key.source.to_string(),
        |location| format!("{}:{}", location.file, location.line),
    );
    format!("{written_at} → {reference}")
}

/// Puts `finding` at the link's first location, if it has one.
fn at_first_location(finding: Finding, link: &ScannedLink) -> Finding {
    match link.locations.first().cloned() {
        Some(location) => finding.with_location(location),
        None => finding,
    }
}

/// The finding for a link whose code changed since it was confirmed.
fn failed_finding(key: &LinkKey, link: &ScannedLink) -> Finding {
    let reference = reference_of(key);
    at_first_location(
        Finding::new(
            "link/failed",
            Severity::Error,
            format!("`{reference}` changed since this doc was confirmed"),
        )
        .with_help(format!(
            "reread this, and if it's still accurate run `supdoc update {} --yes`",
            key.source
        )),
        link,
    )
}

/// The finding for a link with no lock entry.
fn new_finding(link: &ScannedLink) -> Finding {
    at_first_location(
        Finding::new(
            "link/new",
            Severity::Error,
            format!("this link isn't recorded in {LOCK_FILE_NAME} yet"),
        )
        .with_help("once it's accurate, run `supdoc update --state new --yes`"),
        link,
    )
}

/// The finding for a lock entry whose link is gone.
fn obsolete_finding(key: &LinkKey) -> Finding {
    Finding::new(
        "link/obsolete",
        Severity::Warning,
        format!("{LOCK_FILE_NAME} has an entry for `{key}`, which no longer exists"),
    )
    .with_help("run `supdoc update --state obsolete --yes`")
}

/// The note saying a link inherited its history from `old`.
fn moved_note(key: &LinkKey, link: &ScannedLink, old: &LinkKey) -> Finding {
    at_first_location(
        Finding::new(
            "check/moved",
            Severity::Note,
            format!(
                "{} carried its history from {}",
                describe(key, link),
                old.source
            ),
        ),
        link,
    )
}

/// The note saying why a link that looks moved was not given a history.
fn move_ambiguous_note(key: &LinkKey, link: &ScannedLink, candidates: &[LinkKey]) -> Finding {
    let reason = match candidates {
        [only] => format!(
            "the removed link `{only}` could have been its old location, but another new link matches it too"
        ),
        many => format!(
            "{} removed links could have been its old location",
            many.len()
        ),
    };
    at_first_location(
        Finding::new(
            "check/move-ambiguous",
            Severity::Note,
            format!("{} is new; {reason}", describe(key, link)),
        )
        .with_help("reread it, then run `supdoc update --state new --yes`"),
        link,
    )
}

/// The warning that a target file has syntax errors (03 R8, 05 C12).
fn syntax_error_finding(target: &Target) -> Finding {
    Finding::new(
        "read/syntax-errors",
        Severity::Warning,
        format!("{target} has syntax errors; its links may report stale until it parses cleanly"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Location;

    fn key(source: &str, symbol: Option<&str>) -> LinkKey {
        LinkKey {
            source: source.parse().expect("the fixture source should be valid"),
            target: "src/lib.rs"
                .parse()
                .expect("the fixture target should be valid"),
            symbol: symbol
                .map(|symbol| symbol.parse().expect("the fixture symbol should be valid")),
        }
    }

    fn written_in(key: &LinkKey, target: TargetState) -> ScannedLink {
        let location = Location::in_text(key.source.clone(), "[a](b)", 0..6)
            .expect("the fixture range should be locatable");
        ScannedLink {
            locations: vec![location],
            target,
        }
    }

    fn present(code: &[u8]) -> TargetState {
        TargetState::Present {
            hash: Hash::digest(code),
            syntax_errors: false,
        }
    }

    fn broken() -> TargetState {
        TargetState::Broken {
            finding: Finding::new("resolve/not-found", Severity::Error, "no symbol `add`"),
        }
    }

    fn scan_of(links: Vec<(LinkKey, TargetState)>) -> Scan {
        Scan {
            links: links
                .into_iter()
                .map(|(key, target)| {
                    let link = written_in(&key, target);
                    (key, link)
                })
                .collect(),
            ..Scan::default()
        }
    }

    fn lock_of(entries: &[(&LinkKey, &[u8])]) -> Lockfile {
        entries
            .iter()
            .map(|&(key, code)| (key.clone(), Hash::digest(code)))
            .collect()
    }

    fn state_of(report: &Report, key: &LinkKey) -> Option<LinkState> {
        report
            .links
            .iter()
            .find(|link| &link.key == key)
            .map(|link| link.state)
    }

    #[test]
    fn matching_hash_is_passed() {
        let add = key("docs/guide.md", Some("add"));
        let scan = scan_of(vec![(add.clone(), present(b"v1"))]);

        let report = reconcile(&scan, &lock_of(&[(&add, b"v1")]), &Scope::All);

        assert_eq!(state_of(&report, &add), Some(LinkState::Passed));
        assert_eq!(
            report.links.first().map(|link| link.finding.is_none()),
            Some(true)
        );
    }

    #[test]
    fn different_hash_is_failed() {
        let add = key("docs/guide.md", Some("add"));
        let scan = scan_of(vec![(add.clone(), present(b"v2"))]);

        let report = reconcile(&scan, &lock_of(&[(&add, b"v1")]), &Scope::All);

        assert_eq!(state_of(&report, &add), Some(LinkState::Failed));
        let finding = report.links.first().and_then(|link| link.finding.as_ref());
        assert_eq!(finding.map(|finding| finding.code), Some("link/failed"));
    }

    #[test]
    fn missing_target_is_broken_even_if_locked() {
        let add = key("docs/guide.md", Some("add"));
        let scan = scan_of(vec![(add.clone(), broken())]);

        let report = reconcile(&scan, &lock_of(&[(&add, b"v1")]), &Scope::All);

        assert_eq!(state_of(&report, &add), Some(LinkState::Broken));
        let finding = report.links.first().and_then(|link| link.finding.as_ref());
        assert_eq!(
            finding.map(|finding| finding.code),
            Some("resolve/not-found"),
            "a broken link carries the scan's own finding",
        );
    }

    #[test]
    fn unlocked_link_is_new() {
        let add = key("docs/guide.md", Some("add"));
        let scan = scan_of(vec![(add.clone(), present(b"v1"))]);

        let report = reconcile(&scan, &Lockfile::default(), &Scope::All);

        assert_eq!(state_of(&report, &add), Some(LinkState::New));
    }

    #[test]
    fn lock_entry_without_link_is_obsolete() {
        let gone = key("docs/old.md", Some("add"));

        let report = reconcile(&Scan::default(), &lock_of(&[(&gone, b"v1")]), &Scope::All);

        assert_eq!(state_of(&report, &gone), Some(LinkState::Obsolete));
        assert!(
            report.links.iter().all(|link| link.locations.is_empty()),
            "an obsolete link is written nowhere",
        );
    }

    #[test]
    fn entry_outside_scope_is_not_judged() {
        let other = key("docs/other.md", Some("add"));
        let scope = Scope::Paths(vec![
            "docs/a.md"
                .parse()
                .expect("the fixture source should be valid"),
        ]);

        let report = reconcile(&Scan::default(), &lock_of(&[(&other, b"v1")]), &scope);

        assert!(
            report.links.is_empty(),
            "`docs/other.md` is outside the scope, so its entry must not be obsolete: {:?}",
            report.links,
        );
    }

    #[test]
    fn entry_from_unreadable_source_is_not_obsolete() {
        let locked = key("docs/locked.md", Some("add"));
        let mut scan = Scan::default();
        let _new = scan.unread_sources.insert(locked.source.clone());

        let report = reconcile(&scan, &lock_of(&[(&locked, b"v1")]), &Scope::All);

        assert!(
            report.links.is_empty(),
            "an unreadable source must keep its entries: {:?}",
            report.links,
        );
    }

    #[test]
    fn renamed_source_carries_hash_and_can_still_fail() {
        let old = key("docs/guide.md", Some("add"));
        let unchanged = key("docs/guide/intro.md", Some("add"));
        let changed = key("docs/guide/intro.md", Some("sub"));
        let old_sub = key("docs/guide.md", Some("sub"));
        let scan = scan_of(vec![
            (unchanged.clone(), present(b"add v1")),
            (changed.clone(), present(b"sub v2")),
        ]);
        let lock = lock_of(&[(&old, b"add v1"), (&old_sub, b"sub v1")]);

        let report = reconcile(&scan, &lock, &Scope::All);

        assert_eq!(state_of(&report, &unchanged), Some(LinkState::Passed));
        assert_eq!(
            state_of(&report, &changed),
            Some(LinkState::Failed),
            "a moved link whose code also changed is failed, not new",
        );
        assert_eq!(
            report
                .links
                .iter()
                .map(|link| link.state)
                .collect::<Vec<_>>(),
            [LinkState::Passed, LinkState::Failed],
            "the used-up old entries must not also be reported obsolete",
        );
        assert_eq!(
            report
                .links
                .iter()
                .find(|link| link.key == unchanged)
                .and_then(|link| link.moved_from.clone()),
            Some(old),
        );
        assert_eq!(
            report.findings.iter().map(|f| f.code).collect::<Vec<_>>(),
            ["check/moved", "check/moved"],
        );
    }

    #[test]
    fn two_move_candidates_carry_nothing_and_explain() {
        let first = key("docs/a.md", Some("add"));
        let second = key("docs/b.md", Some("add"));
        let fresh = key("docs/intro.md", Some("add"));
        let scan = scan_of(vec![(fresh.clone(), present(b"v1"))]);
        let lock = lock_of(&[(&first, b"v1"), (&second, b"v1")]);

        let report = reconcile(&scan, &lock, &Scope::All);

        assert_eq!(state_of(&report, &fresh), Some(LinkState::New));
        assert_eq!(state_of(&report, &first), Some(LinkState::Obsolete));
        assert_eq!(state_of(&report, &second), Some(LinkState::Obsolete));
        let notes: Vec<_> = report
            .findings
            .iter()
            .map(|finding| (finding.code, finding.message.as_str()))
            .collect();
        assert_eq!(
            notes,
            [(
                "check/move-ambiguous",
                "docs/intro.md:1 → src/lib.rs#add is new; 2 removed links could have been its old location"
            )],
        );
    }

    #[test]
    fn syntax_errors_in_a_target_warn_once_per_file() {
        let add = key("docs/a.md", Some("add"));
        let sub = key("docs/a.md", Some("sub"));
        let flagged = TargetState::Present {
            hash: Hash::digest(b"x"),
            syntax_errors: true,
        };
        let scan = scan_of(vec![(add.clone(), flagged.clone()), (sub.clone(), flagged)]);

        let report = reconcile(&scan, &Lockfile::default(), &Scope::All);

        assert_eq!(
            report.findings.iter().map(|f| f.code).collect::<Vec<_>>(),
            ["read/syntax-errors"],
        );
    }

    #[test]
    fn links_are_sorted_by_key_across_states() {
        let late = key("docs/z.md", Some("add"));
        let early = key("docs/a.md", Some("sub"));
        let scan = scan_of(vec![(late.clone(), present(b"v1"))]);

        let report = reconcile(&scan, &lock_of(&[(&early, b"v1")]), &Scope::All);

        assert_eq!(
            report
                .links
                .iter()
                .map(|link| link.key.clone())
                .collect::<Vec<_>>(),
            [early, late],
        );
    }

    fn report_with(states: &[LinkState]) -> Report {
        Report {
            links: states
                .iter()
                .map(|&state| LinkReport {
                    key: key("docs/a.md", None),
                    state,
                    locations: Vec::new(),
                    finding: None,
                    moved_from: None,
                })
                .collect(),
            findings: Vec::new(),
        }
    }

    #[test]
    fn failed_broken_and_new_fail_check() {
        for state in [LinkState::Failed, LinkState::Broken, LinkState::New] {
            assert_eq!(
                check_outcome(&report_with(&[LinkState::Passed, state]), false),
                Outcome::Failing,
                "a {state} link should fail the check",
            );
        }
    }

    #[test]
    fn obsolete_only_warns() {
        assert_eq!(
            check_outcome(
                &report_with(&[LinkState::Passed, LinkState::Obsolete]),
                false
            ),
            Outcome::Clean,
        );
        assert_eq!(check_outcome(&Report::default(), false), Outcome::Clean);
    }

    #[test]
    fn deny_warnings_fails_on_obsolete() {
        assert_eq!(
            check_outcome(&report_with(&[LinkState::Obsolete]), true),
            Outcome::Failing,
        );
        let mut warned = report_with(&[LinkState::Passed]);
        warned
            .findings
            .push(Finding::new("read/syntax-errors", Severity::Warning, "x"));
        assert_eq!(check_outcome(&warned, false), Outcome::Clean);
        assert_eq!(check_outcome(&warned, true), Outcome::Failing);
    }
}
