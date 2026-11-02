//! The "did you mean" rule shared by the scanner, the reader, and the CLI: one cutoff, one metric
//! (optimal string alignment distance, where swapping neighbors is one edit), and a length
//! prefilter so that comparing against many candidates stays cheap.

/// The most single-character edits `wanted` may be from a candidate and still count as close:
/// `max(len, 3) / 3`.
fn cutoff(wanted: &str) -> usize {
    wanted.chars().count().max(3) / 3
}

/// The edit distance between `wanted` and `candidate`, counting a swap of neighbors as one edit.
///
/// Rank candidates by this so ranking agrees with [`is_close`].
#[must_use]
pub(crate) fn distance(wanted: &str, candidate: &str) -> usize {
    strsim::osa_distance(wanted, candidate)
}

/// Whether `candidate` is a likely misspelling of `wanted`: within `max(len, 3) / 3` single-character
/// edits, counting a swap of neighbors as one edit.
///
/// Strings whose lengths differ by more than the cutoff are rejected without computing the
/// distance, since the distance is at least the length difference.
#[must_use]
pub(crate) fn is_close(wanted: &str, candidate: &str) -> bool {
    let limit = cutoff(wanted);
    // Byte length bounds char length from above, so only a char-length gap can be trusted.
    if wanted.chars().count().abs_diff(candidate.chars().count()) > limit {
        return false;
    }
    distance(wanted, candidate) <= limit
}

/// The one candidate that is a likely misspelling of `wanted`, if exactly one is.
///
/// Close means within `max(len, 3) / 3` single-character edits of `wanted`, counting a swap of
/// neighbors as one edit, so `atuh` is close to `auth`. Repeated candidates count once.
#[must_use]
pub(crate) fn close_match<'c>(
    wanted: &str,
    candidates: impl IntoIterator<Item = &'c str>,
) -> Option<&'c str> {
    let mut close: Vec<&str> = candidates
        .into_iter()
        .filter(|candidate| *candidate != wanted)
        .filter(|candidate| is_close(wanted, candidate))
        .collect();
    close.sort_unstable();
    close.dedup();
    match close.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swap_of_neighbors_is_one_edit_for_filter_and_rank() {
        assert!(is_close("atuh", "auth"), "atuh should be close to auth");
        assert_eq!(
            distance("atuh", "auth"),
            1,
            "a swap should rank as one edit"
        );
    }

    #[test]
    fn a_much_longer_candidate_is_not_close() {
        assert!(
            !is_close("auth", "authentication_service"),
            "a candidate far longer than the query should be rejected"
        );
    }

    #[test]
    fn close_match_needs_exactly_one_distinct_candidate() {
        assert_eq!(close_match("atuh", ["auth", "auth"]), Some("auth"));
        assert_eq!(
            close_match("abc", ["abd", "abe"]),
            None,
            "two candidates are ambiguous"
        );
        assert_eq!(
            close_match("auth", ["auth"]),
            None,
            "the query itself is not a suggestion"
        );
    }
}
