//! Parsing lockfile text, leniently (02 L6, L7).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::Error as _;
use toml::Spanned;

use super::error::ParseLockError;
use super::model::{FORMAT_VERSION, Lockfile};
use crate::hash::{Hash, HashSettings, LineEndings};
use crate::link::{LinkKey, SourcePath, SymbolPath, Target};

/// What a merge leaves at the start of a line it could not resolve.
const CONFLICT_MARKERS: [&str; 3] = ["<<<<<<<", "=======", ">>>>>>>"];

/// Only the `version` field, read before anything else so a newer file is reported as newer.
#[derive(Deserialize)]
struct VersionProbe {
    /// The declared format version, if any.
    version: Option<u32>,
}

/// The whole v1 file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLockfile {
    /// Checked to be the current version; not needed afterwards.
    #[serde(rename = "version", deserialize_with = "current_version")]
    _version: u32,
    /// The settings the hashes were computed under. Required (02 L10).
    features: RawFeatures,
    /// The entries, each with the position it was written at.
    #[serde(default)]
    link: Vec<Spanned<RawLink>>,
}

/// The `[features]` table. Only the hash settings are recorded in the lock.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFeatures {
    /// `[features.hash]`.
    hash: RawHashFeatures,
}

/// The `[features.hash]` table, every key required.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHashFeatures {
    /// How line endings were treated before hashing.
    #[serde(rename = "line-endings")]
    line_endings: LineEndings,
}

impl From<RawHashFeatures> for HashSettings {
    fn from(raw: RawHashFeatures) -> Self {
        Self {
            line_endings: raw.line_endings,
        }
    }
}

/// One `[[link]]` table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLink {
    /// The file the link is written in.
    source: SourcePath,
    /// Where the link points, never above the project root or its namespace's directory.
    #[serde(deserialize_with = "contained_target")]
    target: Target,
    /// The symbol, or `None` for a whole-file link.
    symbol: Option<SymbolPath>,
    /// The recorded hash.
    hash: Hash,
}

/// Accepts only [`FORMAT_VERSION`]; newer files are caught earlier.
///
/// # Errors
///
/// Fails if the value is not a `u32` or is not the current version.
fn current_version<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version == FORMAT_VERSION {
        Ok(version)
    } else {
        Err(D::Error::custom(format_args!(
            "unsupported lockfile format {version}: this supdoc reads format {FORMAT_VERSION}"
        )))
    }
}

/// Reads a target, refusing one that climbs with `..`: supdoc reads nothing outside the project
/// root, so a hand-written lock can't name such a file either.
///
/// # Errors
///
/// Fails if the value is not a target, or if any of its segments is `..`.
fn contained_target<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Target, D::Error> {
    let target = Target::deserialize(deserializer)?;
    if target
        .path()
        .as_str()
        .split('/')
        .any(|segment| segment == "..")
    {
        return Err(D::Error::custom(format_args!(
            "target `{target}` climbs out of the project root with `..`, and supdoc tracks only \
             files inside it"
        )));
    }
    Ok(target)
}

/// The 1-based line containing byte `offset` of `text`.
fn line_at(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(0, |before| before.matches('\n').count())
        + 1
}

/// Wraps a TOML error with the line it points at.
fn toml_error(text: &str, error: toml::de::Error) -> ParseLockError {
    let line = error.span().map(|span| line_at(text, span.start));
    ParseLockError::Toml { line, error }
}

impl Lockfile {
    /// Reads lockfile text. Entries may be in any order and identical duplicates collapse into
    /// one; [`render`](Self::render) then writes them canonically (02 L6).
    ///
    /// # Errors
    ///
    /// Fails, in this order of priority, on leftover merge-conflict markers, a `version` newer
    /// than [`FORMAT_VERSION`], invalid TOML or a malformed field (including a missing or
    /// unknown one, and a missing `[features.hash]` table), and two entries with the same key but different hashes.
    pub fn parse(text: &str) -> Result<Self, ParseLockError> {
        if let Some(index) = text.lines().position(|line| {
            CONFLICT_MARKERS
                .iter()
                .any(|marker| line.starts_with(marker))
        }) {
            return Err(ParseLockError::ConflictMarkers { line: index + 1 });
        }

        let probe: VersionProbe = toml::from_str(text).map_err(|error| toml_error(text, error))?;
        if let Some(found) = probe.version.filter(|found| *found > FORMAT_VERSION) {
            return Err(ParseLockError::NewerVersion {
                found,
                supported: FORMAT_VERSION,
            });
        }

        let raw: RawLockfile = toml::from_str(text).map_err(|error| toml_error(text, error))?;
        let mut entries: BTreeMap<LinkKey, (Hash, usize)> = BTreeMap::new();
        for link in raw.link {
            let line = line_at(text, link.span().start);
            let RawLink {
                source,
                target,
                symbol,
                hash,
            } = link.into_inner();
            let key = LinkKey {
                source,
                target,
                symbol,
            };
            if let Some(&(first_hash, first_line)) = entries.get(&key) {
                if first_hash != hash {
                    return Err(ParseLockError::ConflictingDuplicate {
                        key,
                        first_line,
                        second_line: line,
                    });
                }
            } else {
                let _ = entries.insert(key, (hash, line));
            }
        }
        let mut lock: Self = entries
            .into_iter()
            .map(|(key, (hash, _))| (key, hash))
            .collect();
        lock.set_hash_settings(raw.features.hash.into());
        Ok(lock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b";
    const HASH_B: &str = "41aa07d241aa07d241aa07d241aa07d241aa07d241aa07d241aa07d241aa07d2";

    /// One `[[link]]` table, with `symbol` omitted for whole-file links.
    fn entry(source: &str, target: &str, symbol: Option<&str>, hash: &str) -> String {
        let symbol = symbol.map_or_else(String::new, |symbol| format!("symbol = \"{symbol}\"\n"));
        format!(
            "[[link]]\nsource = \"{source}\"\ntarget = \"{target}\"\n{symbol}hash = \"{hash}\"\n"
        )
    }

    /// The `version` line and the default `[features.hash]` table every v1 file starts with.
    const PREAMBLE: &str = "version = 1\n\n[features.hash]\nline-endings = \"normalize\"\n";

    /// A v1 file made of `entries`, under the default hash settings.
    fn file(entries: &[String]) -> String {
        entries.iter().fold(String::from(PREAMBLE), |text, entry| {
            format!("{text}\n{entry}")
        })
    }

    fn parse(text: &str) -> Lockfile {
        Lockfile::parse(text).expect("the fixture lockfile should parse")
    }

    fn parse_error(text: &str) -> ParseLockError {
        Lockfile::parse(text).expect_err("the fixture lockfile should be rejected")
    }

    fn key(source: &str, target: &str, symbol: Option<&str>) -> LinkKey {
        LinkKey {
            source: source.parse().expect("the fixture source should be valid"),
            target: target.parse().expect("the fixture target should be valid"),
            symbol: symbol
                .map(|symbol| symbol.parse().expect("the fixture symbol should be valid")),
        }
    }

    fn hash(hex: &str) -> Hash {
        hex.parse().expect("the fixture hash should be valid")
    }

    const GOLDEN: &str = "\
# This file is generated by supdoc. Do not edit by hand.
version = 1

[features.hash]
line-endings = \"normalize\"

[[link]]
source = \"docs/guide.md\"
target = \"src/lib.rs\"
symbol = \"Config.validate\"
hash = \"9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b\"

[[link]]
source = \"docs/guide.md\"
target = \"auth:src/session.rs\"
hash = \"41aa07d241aa07d241aa07d241aa07d241aa07d241aa07d241aa07d241aa07d2\"
";

    #[test]
    fn render_matches_v1_golden_text() {
        let lock: Lockfile = [
            (
                key("docs/guide.md", "src/lib.rs", Some("Config.validate")),
                hash(HASH_A),
            ),
            (
                key("docs/guide.md", "auth:src/session.rs", None),
                hash(HASH_B),
            ),
        ]
        .into_iter()
        .collect();

        assert_eq!(lock.render(), GOLDEN);
    }

    #[test]
    fn empty_lock_renders_features_hash() {
        assert_eq!(
            Lockfile::default().render(),
            "# This file is generated by supdoc. Do not edit by hand.\nversion = 1\n\n\
             [features.hash]\nline-endings = \"normalize\"\n"
        );
    }

    #[test]
    fn features_hash_table_is_required() {
        let error = parse_error(&format!(
            "version = 1\n\n{}",
            entry("docs/a.md", "src/lib.rs", None, HASH_A)
        ));

        assert_eq!(error.code(), "lock/parse");
        assert!(
            error.to_string().contains("missing field `features`"),
            "the error should name the missing table, got: {error}"
        );
    }

    #[test]
    fn features_hash_round_trips_both_values() {
        for line_endings in [LineEndings::Normalize, LineEndings::Exact] {
            let mut lock: Lockfile = [(key("docs/a.md", "src/lib.rs", None), hash(HASH_A))]
                .into_iter()
                .collect();
            lock.set_hash_settings(HashSettings { line_endings });

            let rendered = lock.render();
            let reparsed = parse(&rendered);

            assert!(
                rendered.contains(&format!(
                    "[features.hash]\nline-endings = \"{line_endings}\"\n"
                )),
                "`{line_endings}` should be written out, got:\n{rendered}"
            );
            assert_eq!(reparsed.hash_settings().line_endings, line_endings);
            assert_eq!(reparsed, lock);
        }
    }

    #[test]
    fn unknown_line_endings_value_is_rejected_with_line() {
        let text = "version = 1\n\n[features.hash]\nline-endings = \"crlf\"\n";

        let error = parse_error(text);

        assert_eq!(error.code(), "lock/parse");
        assert_eq!(
            error.to_string(),
            "unknown variant `crlf`, expected `normalize` or `exact` (line 4)"
        );
    }

    #[test]
    fn unknown_features_key_is_rejected() {
        for text in [
            format!("{PREAMBLE}eol = \"lf\"\n"),
            format!("{PREAMBLE}\n[features.cache]\nenabled = true\n"),
        ] {
            let error = parse_error(&text);

            assert_eq!(error.code(), "lock/parse", "{text}");
            assert!(
                error.to_string().contains("unknown field"),
                "the error should name the unknown key, got: {error}"
            );
        }
    }

    #[test]
    fn inline_features_table_is_accepted_and_rendered_canonically() {
        let text = format!(
            "version = 1\nfeatures = {{ hash = {{ line-endings = \"exact\" }} }}\n\n{}",
            entry("docs/a.md", "src/lib.rs", None, HASH_A)
        );

        let lock = parse(&text);

        assert_eq!(lock.hash_settings().line_endings, LineEndings::Exact);
        assert_eq!(
            lock.render(),
            format!(
                "# This file is generated by supdoc. Do not edit by hand.\nversion = 1\n\n\
                 [features.hash]\nline-endings = \"exact\"\n\n{}",
                entry("docs/a.md", "src/lib.rs", None, HASH_A)
            )
        );
    }

    #[test]
    fn render_is_identical_regardless_of_insert_order() {
        let entries = [
            (key("docs/b.md", "src/lib.rs", None), hash(HASH_A)),
            (key("docs/a.md", "src/lib.rs", Some("add")), hash(HASH_B)),
            (key("docs/a.md", "src/lib.rs", None), hash(HASH_A)),
        ];
        let forward: Lockfile = entries.iter().cloned().collect();
        let backward: Lockfile = entries.iter().rev().cloned().collect();

        assert_eq!(forward.render(), backward.render());
    }

    #[test]
    fn parse_then_render_round_trips() {
        let lock = parse(GOLDEN);

        assert_eq!(lock.render(), GOLDEN);
        assert_eq!(parse(&lock.render()), lock);
    }

    #[test]
    fn whole_file_entry_renders_without_symbol_field() {
        let lock: Lockfile = [(key("docs/a.md", "src/lib.rs", None), hash(HASH_A))]
            .into_iter()
            .collect();

        assert!(
            !lock.render().contains("symbol"),
            "a whole-file entry should have no `symbol` field, got:\n{}",
            lock.render()
        );
    }

    #[test]
    fn whole_file_entries_sort_before_symbol_entries() {
        let lock: Lockfile = [
            (key("docs/a.md", "src/lib.rs", Some("add")), hash(HASH_A)),
            (key("docs/a.md", "src/lib.rs", None), hash(HASH_B)),
        ]
        .into_iter()
        .collect();
        let rendered = lock.render();

        let whole = rendered
            .find(HASH_B)
            .expect("the whole-file hash is rendered");
        let symbol = rendered.find(HASH_A).expect("the symbol hash is rendered");
        assert!(
            whole < symbol,
            "whole-file entry should come first:\n{rendered}"
        );
    }

    #[test]
    fn namespaced_target_round_trips_as_written() {
        let text = file(&[entry("docs/a.md", "auth:src/lib.rs", Some("login"), HASH_A)]);
        let lock = parse(&text);

        assert_eq!(
            lock.iter()
                .map(|(key, _)| key.to_string())
                .collect::<Vec<_>>(),
            ["docs/a.md → auth:src/lib.rs#login"]
        );
        assert!(
            lock.render().contains("target = \"auth:src/lib.rs\""),
            "the namespace should be kept in the target, got:\n{}",
            lock.render()
        );
    }

    #[test]
    fn out_of_order_entries_are_accepted_and_rendered_sorted() {
        let text = file(&[
            entry("docs/b.md", "src/lib.rs", None, HASH_A),
            entry("docs/a.md", "src/lib.rs", None, HASH_B),
        ]);

        let rendered = parse(&text).render();

        let a = rendered.find("docs/a.md").expect("docs/a.md is rendered");
        let b = rendered.find("docs/b.md").expect("docs/b.md is rendered");
        assert!(a < b, "entries should be re-sorted:\n{rendered}");
    }

    #[test]
    fn identical_duplicates_collapse_to_one_entry() {
        let one = entry("docs/a.md", "src/lib.rs", Some("add"), HASH_A);
        let text = file(&[one.clone(), one]);

        assert_eq!(parse(&text).len(), 1);
    }

    #[test]
    fn conflicting_duplicates_are_rejected_naming_both_lines() {
        let text = file(&[
            entry("docs/a.md", "src/lib.rs", Some("add"), HASH_A),
            entry("docs/a.md", "src/lib.rs", Some("add"), HASH_B),
        ]);

        let error = parse_error(&text);

        assert_eq!(error.code(), "lock/duplicate");
        assert!(
            matches!(
                error,
                ParseLockError::ConflictingDuplicate {
                    first_line: 6,
                    second_line: 12,
                    ..
                }
            ),
            "expected the entries at lines 6 and 12 to be named, got {error:?}"
        );
        assert_eq!(
            error.to_string(),
            "two entries for docs/a.md → src/lib.rs#add disagree (lines 6 and 12)"
        );
    }

    #[test]
    fn conflict_markers_are_reported_before_toml_errors() {
        let text = "version = 1\n<<<<<<< HEAD\nnot toml at all\n=======\n>>>>>>> branch\n";

        let error = parse_error(text);

        assert_eq!(error.code(), "lock/conflict");
        assert_eq!(error.to_string(), "unresolved merge conflict at line 2");
    }

    #[test]
    fn newer_version_asks_for_upgrade_not_unknown_field() {
        let text = "version = 2\n\n[[link]]\nfuture_field = true\n";

        let error = parse_error(text);

        assert_eq!(error.code(), "lock/newer-version");
        assert_eq!(
            error.to_string(),
            "lockfile format 2 is newer than this supdoc supports (1)"
        );
    }

    #[test]
    fn missing_version_is_rejected() {
        let error = parse_error(&entry("docs/a.md", "src/lib.rs", None, HASH_A));

        assert_eq!(error.code(), "lock/parse");
        assert!(
            error.to_string().contains("version"),
            "the error should name the missing field, got: {error}"
        );
    }

    #[test]
    fn version_zero_is_rejected() {
        let error = parse_error("version = 0\n");

        assert_eq!(error.code(), "lock/parse");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = "version = 1\nextra = true\n";

        let error = parse_error(text);

        assert_eq!(error.code(), "lock/parse");
        assert!(
            error.to_string().contains("extra"),
            "the error should name the unknown field, got: {error}"
        );
    }

    #[test]
    fn unknown_field_in_an_entry_is_rejected() {
        let text = file(&[format!(
            "{}extra = 1\n",
            entry("docs/a.md", "src/lib.rs", None, HASH_A)
        )]);

        assert_eq!(parse_error(&text).code(), "lock/parse");
    }

    #[test]
    fn uppercase_hash_is_accepted_and_rendered_lowercase() {
        let text = file(&[entry(
            "docs/a.md",
            "src/lib.rs",
            None,
            &HASH_A.to_uppercase(),
        )]);

        let rendered = parse(&text).render();

        assert!(
            rendered.contains(HASH_A) && !rendered.contains(&HASH_A.to_uppercase()),
            "the hash should render lowercase, got:\n{rendered}"
        );
    }

    #[test]
    fn short_or_non_hex_hash_is_rejected_with_line() {
        for bad in ["9f2c", &"z".repeat(64)] {
            let text = file(&[entry("docs/a.md", "src/lib.rs", None, bad)]);

            let error = parse_error(&text);

            assert_eq!(error.code(), "lock/parse", "hash `{bad}`");
            assert!(
                error.to_string().contains("expected 64 hex characters")
                    && error.to_string().ends_with("(line 9)"),
                "hash `{bad}` should be rejected with its line, got: {error}"
            );
        }
    }

    #[test]
    fn source_with_parent_segment_is_rejected() {
        let text = file(&[entry("../docs/a.md", "src/lib.rs", None, HASH_A)]);

        assert_eq!(parse_error(&text).code(), "lock/parse");
    }

    #[test]
    fn target_with_parent_segment_is_rejected() {
        for target in ["../shared/x.rs", "src/../../x.rs", "auth:../x.rs"] {
            let text = file(&[entry("docs/a.md", target, None, HASH_A)]);

            let error = parse_error(&text);
            assert_eq!(error.code(), "lock/parse", "`{target}` should be rejected");
            assert!(
                error.to_string().contains("outside") || error.to_string().contains("climbs"),
                "the error should say the target leaves the root: {error}",
            );
        }
    }

    #[test]
    fn backslash_paths_are_rejected() {
        for (source, target) in [
            ("docs\\\\a.md", "src/lib.rs"),
            ("docs/a.md", "src\\\\lib.rs"),
        ] {
            let text = file(&[entry(source, target, None, HASH_A)]);

            assert_eq!(
                parse_error(&text).code(),
                "lock/parse",
                "`{source}` -> `{target}` should be rejected"
            );
        }
    }

    #[test]
    fn empty_symbol_segment_is_rejected() {
        let text = file(&[entry(
            "docs/a.md",
            "src/lib.rs",
            Some("Config..validate"),
            HASH_A,
        )]);

        assert_eq!(parse_error(&text).code(), "lock/parse");
    }
}
