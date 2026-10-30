//! Planning `update`: exactly what would change in the lockfile, computed without writing (05 C2).
//!
//! The preview is this value rendered, and `--yes` writes [`UpdatePlan::next`].

use super::model::{LinkReport, Report};
use crate::hash::{Hash, HashSettings};
use crate::link::LinkKey;
use crate::lock::model::Lockfile;
use crate::scan::model::{Scan, TargetState};
use crate::state::LinkState;

/// What `update` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePlan {
    /// Every lock entry that would change, in report order.
    pub changes: Vec<LockChange>,
    /// The hash settings the lockfile is re-recorded under, when they differ from the ones it
    /// records (B.5). A change of settings alone is a change: `next` is then `Some`.
    pub settings: Option<SettingsChange>,
    /// The lockfile after the changes, or `None` when there is nothing to do, so the file isn't
    /// rewritten and its modification time and git status stay quiet.
    pub next: Option<Lockfile>,
}

/// The lockfile's recorded hash settings, and the different ones the config resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsChange {
    /// The settings the lockfile records.
    pub from: HashSettings,
    /// The settings the config resolves to, which the scan hashed with.
    pub to: HashSettings,
}

impl SettingsChange {
    /// The change from `from` to `to`, or `None` when they are the same.
    #[must_use]
    pub fn between(from: HashSettings, to: HashSettings) -> Option<Self> {
        (from != to).then_some(Self { from, to })
    }
}

/// One change to the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockChange {
    /// Add an entry for a link that had none: a `new` link, or a link that moved keys (05 C6).
    Record(LinkKey),
    /// Replace an entry's hash with the code's current hash: a `failed` link.
    Accept(LinkKey),
    /// Drop an entry: an `obsolete` one, or the old key of a link that moved.
    Remove(LinkKey),
}

/// Plans what `update` would do for the links in `states` (05 C9), re-recording the lockfile
/// under `settings`, the hash settings `scan` hashed with.
///
/// `broken` links are never recorded: there is no code to hash (05 C7). A link that moved keys
/// brings its old entry's hash along. That hash is replaced by the current one only when `failed`
/// is selected, so moving a link never accepts drift by itself. Moving needs `new` to be selected.
///
/// When `settings` differ from the lock's, every link whose hash they change looks `failed`, and
/// is accepted like any other. Entries the plan leaves alone keep their old hashes under the new
/// settings (B-Q4), so the caller plans a settings change only for a full update: no `[PATHS]`
/// and every state from `failed`, `new` and `obsolete` selected (B.5).
#[must_use]
pub fn plan_update(
    report: &Report,
    scan: &Scan,
    lock: &Lockfile,
    states: &[LinkState],
    settings: HashSettings,
) -> UpdatePlan {
    let selected = |state: LinkState| states.contains(&state);
    let settings_change = SettingsChange::between(lock.hash_settings(), settings);
    let mut next = lock.clone();
    next.set_hash_settings(settings);
    let mut changes = Vec::new();

    for link in &report.links {
        if link.state == LinkState::Obsolete {
            if selected(LinkState::Obsolete) && next.remove(&link.key).is_some() {
                changes.push(LockChange::Remove(link.key.clone()));
            }
            continue;
        }
        let Some((hash, accepting)) = hash_to_record(link, scan, lock, selected) else {
            continue;
        };
        if next.insert(link.key.clone(), hash) != Some(hash) {
            changes.push(if accepting {
                LockChange::Accept(link.key.clone())
            } else {
                LockChange::Record(link.key.clone())
            });
        }
        if let Some(old) = &link.moved_from
            && next.remove(old).is_some()
        {
            changes.push(LockChange::Remove(old.clone()));
        }
    }

    UpdatePlan {
        next: (!changes.is_empty() || settings_change.is_some()).then_some(next),
        changes,
        settings: settings_change,
    }
}

/// The hash `update` would record for `link`, and whether that accepts changed code. `None` when
/// the selected states leave the link alone.
fn hash_to_record(
    link: &LinkReport,
    scan: &Scan,
    lock: &Lockfile,
    selected: impl Fn(LinkState) -> bool,
) -> Option<(Hash, bool)> {
    let current = match &scan.links.get(&link.key)?.target {
        TargetState::Present { hash, .. } => *hash,
        TargetState::Broken { .. } => return None,
    };
    let carried = || link.moved_from.as_ref().and_then(|old| lock.get(old));

    match link.state {
        LinkState::Failed if selected(LinkState::Failed) => Some((current, true)),
        LinkState::Failed if selected(LinkState::New) => carried().map(|hash| (hash, false)),
        LinkState::New | LinkState::Passed if selected(LinkState::New) => {
            // A moved link that still passes records the same hash it carried.
            let is_moved_or_new = link.state == LinkState::New || link.moved_from.is_some();
            is_moved_or_new.then_some((current, false))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::reconcile::reconcile;
    use crate::finding::{Finding, Severity};
    use crate::hash::LineEndings;
    use crate::scan::model::{ScannedLink, Scope};

    fn key(source: &str, symbol: &str) -> LinkKey {
        LinkKey {
            source: source.parse().expect("the fixture source should be valid"),
            target: "src/lib.rs"
                .parse()
                .expect("the fixture target should be valid"),
            symbol: Some(symbol.parse().expect("the fixture symbol should be valid")),
        }
    }

    fn present(code: &[u8]) -> TargetState {
        TargetState::Present {
            hash: Hash::digest(code),
            syntax_errors: false,
        }
    }

    fn scan_of(links: Vec<(LinkKey, TargetState)>) -> Scan {
        Scan {
            links: links
                .into_iter()
                .map(|(key, target)| {
                    let link = ScannedLink {
                        locations: Vec::new(),
                        snippet: None,
                        target,
                    };
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

    fn plan(scan: &Scan, lock: &Lockfile, states: &[LinkState]) -> UpdatePlan {
        let report = reconcile(scan, lock, &Scope::All);
        plan_update(&report, scan, lock, states, lock.hash_settings())
    }

    const DEFAULT_STATES: [LinkState; 3] = [LinkState::Failed, LinkState::New, LinkState::Obsolete];

    /// One link in each state: `passed`, `failed`, `broken`, `new`, `obsolete`.
    fn one_of_each() -> (Scan, Lockfile) {
        let broken = TargetState::Broken {
            finding: Finding::new("resolve-not-found", Severity::Error, "no symbol"),
        };
        let scan = scan_of(vec![
            (key("docs/a.md", "passed"), present(b"same")),
            (key("docs/a.md", "failed"), present(b"after")),
            (key("docs/a.md", "broken"), broken),
            (key("docs/a.md", "new"), present(b"fresh")),
        ]);
        let lock = lock_of(&[
            (&key("docs/a.md", "passed"), b"same"),
            (&key("docs/a.md", "failed"), b"before"),
            (&key("docs/a.md", "broken"), b"old"),
            (&key("docs/gone.md", "obsolete"), b"old"),
        ]);
        (scan, lock)
    }

    #[test]
    fn update_plan_accepts_failed_records_new_removes_obsolete() {
        let (scan, lock) = one_of_each();

        let plan = plan(&scan, &lock, &DEFAULT_STATES);

        assert_eq!(
            plan.changes,
            [
                LockChange::Accept(key("docs/a.md", "failed")),
                LockChange::Record(key("docs/a.md", "new")),
                LockChange::Remove(key("docs/gone.md", "obsolete")),
            ],
        );
        let next = plan.next.expect("changes should produce a next lockfile");
        assert_eq!(
            next.get(&key("docs/a.md", "failed")),
            Some(Hash::digest(b"after"))
        );
        assert_eq!(
            next.get(&key("docs/a.md", "new")),
            Some(Hash::digest(b"fresh"))
        );
        assert_eq!(next.get(&key("docs/gone.md", "obsolete")), None);
    }

    #[test]
    fn update_plan_with_state_new_never_accepts_failed() {
        let (scan, lock) = one_of_each();

        let plan = plan(&scan, &lock, &[LinkState::New]);

        assert_eq!(
            plan.changes,
            [LockChange::Record(key("docs/a.md", "new"))],
            "`--state new` is bookkeeping only",
        );
        let next = plan.next.expect("changes should produce a next lockfile");
        assert_eq!(
            next.get(&key("docs/a.md", "failed")),
            Some(Hash::digest(b"before")),
            "the failed link's old hash must stay",
        );
    }

    #[test]
    fn update_plan_cannot_accept_broken() {
        let (scan, lock) = one_of_each();

        let plan = plan(&scan, &lock, &LinkState::ALL);

        let touched_broken = plan.changes.iter().any(|change| match change {
            LockChange::Record(key) | LockChange::Accept(key) | LockChange::Remove(key) => {
                *key == self::key("docs/a.md", "broken")
            }
        });
        assert!(
            !touched_broken,
            "a broken link has no code to hash: {:?}",
            plan.changes
        );
        let next = plan.next.expect("other changes should still be planned");
        assert_eq!(
            next.get(&key("docs/a.md", "broken")),
            Some(Hash::digest(b"old"))
        );
    }

    #[test]
    fn update_plan_is_empty_when_nothing_changes() {
        let add = key("docs/a.md", "add");
        let scan = scan_of(vec![(add.clone(), present(b"v1"))]);
        let lock = lock_of(&[(&add, b"v1")]);

        let plan = plan(&scan, &lock, &LinkState::ALL);

        assert_eq!(plan.changes, []);
        assert_eq!(plan.next, None, "nothing to do means no rewrite");
    }

    #[test]
    fn update_plan_under_new_settings_rewrites_the_header_without_entry_changes() {
        let add = key("docs/a.md", "add");
        let scan = scan_of(vec![(add.clone(), present(b"v1"))]);
        let lock = lock_of(&[(&add, b"v1")]);
        let exact = HashSettings {
            line_endings: LineEndings::Exact,
        };
        let report = reconcile(&scan, &lock, &Scope::All);

        let plan = plan_update(&report, &scan, &lock, &DEFAULT_STATES, exact);

        assert_eq!(plan.changes, [], "the one link still passes");
        assert_eq!(
            plan.settings,
            Some(SettingsChange {
                from: HashSettings::default(),
                to: exact
            })
        );
        let next = plan
            .next
            .expect("a settings change alone should rewrite the lockfile");
        assert_eq!(next.hash_settings(), exact);
        assert_eq!(next.get(&add), Some(Hash::digest(b"v1")));
    }

    #[test]
    fn update_plan_without_states_changes_nothing() {
        let (scan, lock) = one_of_each();

        assert_eq!(plan(&scan, &lock, &[]).next, None);
    }

    #[test]
    fn update_plan_moves_a_passing_renamed_link_and_drops_its_old_entry() {
        let old = key("docs/guide.md", "add");
        let moved = key("docs/guide/intro.md", "add");
        let scan = scan_of(vec![(moved.clone(), present(b"v1"))]);
        let lock = lock_of(&[(&old, b"v1")]);

        let plan = plan(&scan, &lock, &DEFAULT_STATES);

        assert_eq!(
            plan.changes,
            [
                LockChange::Record(moved.clone()),
                LockChange::Remove(old.clone())
            ],
        );
        let next = plan.next.expect("a move changes the lockfile");
        assert_eq!(next.get(&moved), Some(Hash::digest(b"v1")));
        assert_eq!(next.get(&old), None);
    }

    #[test]
    fn update_plan_moving_a_failed_link_accepts_only_when_failed_is_selected() {
        let old = key("docs/guide.md", "add");
        let moved = key("docs/guide/intro.md", "add");
        let scan = scan_of(vec![(moved.clone(), present(b"v2"))]);
        let lock = lock_of(&[(&old, b"v1")]);

        let bookkeeping = plan(&scan, &lock, &[LinkState::New, LinkState::Obsolete]);
        let accepting = plan(&scan, &lock, &DEFAULT_STATES);

        assert_eq!(
            bookkeeping.next.and_then(|next| next.get(&moved)),
            Some(Hash::digest(b"v1")),
            "moving alone must keep the old hash, so the link still reports failed",
        );
        assert_eq!(
            accepting.changes,
            [LockChange::Accept(moved.clone()), LockChange::Remove(old)],
        );
        assert_eq!(
            accepting.next.and_then(|next| next.get(&moved)),
            Some(Hash::digest(b"v2")),
        );
    }
}
