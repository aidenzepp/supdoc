//! Moved-link detection: carrying a lock entry's history to the key it moved to (05 C6).
//!
//! The rule is one sentence: **carry the old hash only when there is exactly one candidate.** A
//! new link and a removed lock entry are candidates for each other when they point at the same
//! target and symbol. The pairing must be unique from both sides, so detection never guesses.

use std::collections::BTreeMap;

use crate::link::{LinkKey, SymbolPath, Target};

/// A lock entry moving from one key to another: the `from` entry's hash becomes `to`'s.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Move {
    /// The key the entry is under now, which is used up by the move.
    pub(crate) from: LinkKey,
    /// The key the entry moves to.
    pub(crate) to: LinkKey,
}

/// What detection decided about the new links it was given.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Moves {
    /// New key to the old key whose hash it inherits, so a new key inherits at most once. The old
    /// entry is used up. Read it through [`Moves::carried`] and [`Moves::carried_from`].
    carried: BTreeMap<LinkKey, LinkKey>,
    /// New keys that had candidates but no unique pairing, with every candidate they matched.
    /// Nothing is carried for these, so they stay plain `new`.
    pub(super) ambiguous: BTreeMap<LinkKey, Vec<LinkKey>>,
}

impl Moves {
    /// The old key whose hash `to` inherits, if it inherits one.
    pub(super) fn carried_from(&self, to: &LinkKey) -> Option<&LinkKey> {
        self.carried.get(to)
    }

    /// Every carried move, ordered by its new key.
    pub(super) fn carried(&self) -> impl Iterator<Item = Move> {
        self.carried.iter().map(|(to, from)| Move {
            from: from.clone(),
            to: to.clone(),
        })
    }
}

/// Pairs each of `new` (links with no lock entry) with the one entry of `obsolete` (lock entries
/// with no link) it clearly moved from, if there is exactly one such entry and no other new link
/// also matches it.
///
/// The removed entries are grouped by what they point at, so each new link costs one lookup, and
/// the number of new links pointing at a group is how many claim each of its entries.
pub(super) fn detect_moves(new: &[&LinkKey], obsolete: &[&LinkKey]) -> Moves {
    let mut by_reference: BTreeMap<Reference<'_>, Vec<&LinkKey>> = BTreeMap::new();
    for &old in obsolete {
        by_reference.entry(reference(old)).or_default().push(old);
    }
    let mut claims: BTreeMap<Reference<'_>, usize> = BTreeMap::new();
    for &fresh in new {
        if by_reference.contains_key(&reference(fresh)) {
            *claims.entry(reference(fresh)).or_default() += 1;
        }
    }

    let mut moves = Moves::default();
    for &fresh in new {
        let pointed_at = reference(fresh);
        let Some(matching) = by_reference.get(&pointed_at) else {
            continue;
        };
        match matching.as_slice() {
            [only] if claims.get(&pointed_at) == Some(&1) => {
                let _previous = moves.carried.insert(fresh.clone(), (*only).clone());
            }
            _ => {
                let _previous = moves
                    .ambiguous
                    .insert(fresh.clone(), matching.iter().copied().cloned().collect());
            }
        }
    }
    moves
}

/// What a link points at: its target and symbol, without its source.
type Reference<'a> = (&'a Target, Option<&'a SymbolPath>);

/// The [`Reference`] of `key`.
fn reference(key: &LinkKey) -> Reference<'_> {
    (&key.target, key.symbol.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn one_new_and_one_removed_link_to_the_same_symbol_are_a_move() {
        let new = key("docs/guide/intro.md", Some("add"));
        let old = key("docs/guide.md", Some("add"));

        let moves = detect_moves(&[&new], &[&old]);

        assert_eq!(moves.carried_from(&new), Some(&old));
        assert!(moves.ambiguous.is_empty());
    }

    #[test]
    fn a_different_symbol_is_not_a_candidate() {
        let new = key("docs/b.md", Some("add"));
        let old = key("docs/a.md", Some("sub"));

        assert_eq!(detect_moves(&[&new], &[&old]), Moves::default());
    }

    #[test]
    fn a_whole_file_link_does_not_match_a_symbol_link() {
        let new = key("docs/b.md", None);
        let old = key("docs/a.md", Some("add"));

        assert_eq!(detect_moves(&[&new], &[&old]), Moves::default());
    }

    #[test]
    fn two_removed_candidates_for_one_new_link_carry_nothing() {
        let new = key("docs/c.md", Some("add"));
        let first = key("docs/a.md", Some("add"));
        let second = key("docs/b.md", Some("add"));

        let moves = detect_moves(&[&new], &[&first, &second]);

        assert!(moves.carried.is_empty(), "two candidates must not pick one");
        assert_eq!(moves.ambiguous.get(&new), Some(&vec![first, second]));
    }

    #[test]
    fn two_new_links_claiming_one_removed_entry_carry_nothing() {
        let first = key("docs/b.md", Some("add"));
        let second = key("docs/c.md", Some("add"));
        let old = key("docs/a.md", Some("add"));

        let moves = detect_moves(&[&first, &second], &[&old]);

        assert!(
            moves.carried.is_empty(),
            "one entry cannot be inherited twice"
        );
        assert_eq!(
            moves.ambiguous.keys().collect::<Vec<_>>(),
            [&first, &second]
        );
    }
}
