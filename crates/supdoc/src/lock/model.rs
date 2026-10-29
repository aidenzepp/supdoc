//! The in-memory lockfile: one recorded hash per link (02).
//!
//! Parsing, rendering and file I/O live beside this module; this is the map they build and read.

use std::collections::BTreeMap;

use crate::hash::Hash;
use crate::link::LinkKey;

/// The lockfile's name, in the project root.
pub const LOCK_FILE_NAME: &str = "supdoc.lock";

/// The on-disk format version this supdoc reads and writes. Bumped by any change to the format
/// or to how hashes are computed (02 L7).
pub const FORMAT_VERSION: u32 = 1;

/// Every recorded link and the hash it was last confirmed against.
///
/// Keyed by [`LinkKey`], so entries are unique and sorted in canonical order by construction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lockfile {
    /// The entries, in canonical order.
    links: BTreeMap<LinkKey, Hash>,
}

impl Lockfile {
    /// The recorded hash for `key`, if any.
    #[must_use]
    pub fn get(&self, key: &LinkKey) -> Option<Hash> {
        self.links.get(key).copied()
    }

    /// Records `hash` for `key`, returning the previous hash, if any.
    pub fn insert(&mut self, key: LinkKey, hash: Hash) -> Option<Hash> {
        self.links.insert(key, hash)
    }

    /// Removes the entry for `key`, returning its hash, if any.
    pub fn remove(&mut self, key: &LinkKey) -> Option<Hash> {
        self.links.remove(key)
    }

    /// Every entry, in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = (&LinkKey, Hash)> {
        self.links.iter().map(|(key, hash)| (key, *hash))
    }

    /// Keeps only the entries for which `keep` returns `true`.
    pub fn retain(&mut self, mut keep: impl FnMut(&LinkKey, Hash) -> bool) {
        self.links.retain(|key, hash| keep(key, *hash));
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }
}

impl FromIterator<(LinkKey, Hash)> for Lockfile {
    /// Later entries for the same key replace earlier ones.
    fn from_iter<I: IntoIterator<Item = (LinkKey, Hash)>>(entries: I) -> Self {
        Self {
            links: entries.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str, target: &str, symbol: Option<&str>) -> LinkKey {
        LinkKey {
            source: source.parse().expect("the fixture source should be valid"),
            target: target.parse().expect("the fixture target should be valid"),
            symbol: symbol
                .map(|symbol| symbol.parse().expect("the fixture symbol should be valid")),
        }
    }

    #[test]
    fn insert_get_and_remove_track_one_hash_per_key() {
        let add = key("docs/guide.md", "src/lib.rs", Some("add"));
        let mut lock = Lockfile::default();

        assert_eq!(lock.insert(add.clone(), Hash::digest(b"v1")), None);
        assert_eq!(
            lock.insert(add.clone(), Hash::digest(b"v2")),
            Some(Hash::digest(b"v1"))
        );
        assert_eq!(lock.get(&add), Some(Hash::digest(b"v2")));
        assert_eq!(lock.remove(&add), Some(Hash::digest(b"v2")));
        assert_eq!(lock.get(&add), None);
        assert!(
            lock.is_empty(),
            "removing the only entry should leave the lockfile empty"
        );
    }

    #[test]
    fn entries_are_per_source_not_per_target() {
        let mut lock = Lockfile::default();
        let _ = lock.insert(
            key("docs/guide.md", "src/lib.rs", Some("add")),
            Hash::digest(b"old"),
        );
        let _ = lock.insert(
            key("docs/api.md", "src/lib.rs", Some("add")),
            Hash::digest(b"new"),
        );

        assert_eq!(
            lock.len(),
            2,
            "two docs linking one symbol should keep two entries"
        );
    }

    #[test]
    fn iter_is_in_canonical_order_regardless_of_insert_order() {
        let keys = [
            key("docs/b.md", "src/lib.rs", None),
            key("docs/a.md", "src/lib.rs", Some("add")),
            key("docs/a.md", "auth:src/lib.rs", None),
            key("docs/a.md", "src/lib.rs", None),
        ];
        let lock: Lockfile = keys
            .iter()
            .rev()
            .map(|key| (key.clone(), Hash::digest(b"x")))
            .collect();

        assert_eq!(
            lock.iter()
                .map(|(key, _)| key.to_string())
                .collect::<Vec<_>>(),
            [
                "docs/a.md → src/lib.rs",
                "docs/a.md → src/lib.rs#add",
                "docs/a.md → auth:src/lib.rs",
                "docs/b.md → src/lib.rs",
            ],
        );
    }

    #[test]
    fn retain_drops_entries_the_predicate_rejects() {
        let keep = key("docs/a.md", "src/lib.rs", None);
        let drop = key("docs/old.md", "src/lib.rs", None);
        let mut lock: Lockfile = [
            (keep.clone(), Hash::digest(b"a")),
            (drop, Hash::digest(b"b")),
        ]
        .into_iter()
        .collect();

        lock.retain(|key, _| key.source.as_str() != "docs/old.md");

        assert_eq!(
            lock.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
            [keep]
        );
    }
}
