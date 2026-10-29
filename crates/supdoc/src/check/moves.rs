//! Moved-link detection: carrying a lock entry's history to the key it moved to (05 C6).
//!
//! The rule is one sentence: **carry the old hash only when there is exactly one candidate.** A
//! new link and a removed lock entry are candidates for each other when they point at the same
//! target and symbol. The pairing must be unique from both sides, so detection never guesses.

use std::collections::BTreeMap;

use crate::link::LinkKey;

/// What detection decided about the new links it was given.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Moves {
    /// New key to the old key whose hash it inherits. The old entry is used up.
    pub(super) carried: BTreeMap<LinkKey, LinkKey>,
    /// New keys that had candidates but no unique pairing, with every candidate they matched.
    /// Nothing is carried for these, so they stay plain `new`.
    pub(super) ambiguous: BTreeMap<LinkKey, Vec<LinkKey>>,
}

/// Pairs each of `new` (links with no lock entry) with the one entry of `obsolete` (lock entries
/// with no link) it clearly moved from, if there is exactly one such entry and no other new link
/// also matches it.
pub(super) fn detect_moves(new: &[&LinkKey], obsolete: &[&LinkKey]) -> Moves {
    let candidates: Vec<(&LinkKey, Vec<&LinkKey>)> = new
        .iter()
        .map(|&fresh| {
            let matching = obsolete
                .iter()
                .copied()
                .filter(|old| old.target == fresh.target && old.symbol == fresh.symbol)
                .collect();
            (fresh, matching)
        })
        .collect();

    let mut claims: BTreeMap<&LinkKey, usize> = BTreeMap::new();
    for old in candidates.iter().flat_map(|(_, matching)| matching) {
        *claims.entry(old).or_default() += 1;
    }

    let mut moves = Moves::default();
    for (fresh, matching) in candidates {
        match matching.as_slice() {
            [] => {}
            [only] if claims.get(only) == Some(&1) => {
                let _previous = moves.carried.insert(fresh.clone(), (*only).clone());
            }
            _ => {
                let _previous = moves
                    .ambiguous
                    .insert(fresh.clone(), matching.into_iter().cloned().collect());
            }
        }
    }
    moves
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

        assert_eq!(moves.carried.get(&new), Some(&old));
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
