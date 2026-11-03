//! Tracking `uses(...)` targets: each written target's current hash compared with the one
//! `supdoc.lock` records for its doc, in any language.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::finding::{Finding, Location, Severity};
use crate::hash::Hash;
use crate::link::LinkKey;
use crate::lock::model::LOCK_FILE_NAME;
use crate::read::cartridge::Languages;
use crate::read::error::CartridgeLoadError;

use super::block::CodeBlock;
use super::info::{SourceKind, source_attributes};
use super::source::{self, Unresolved};

/// The code for a `uses(...)` target whose symbol changed since it was recorded.
const CHANGED: &str = "code-block-uses-changed";

/// The code for a `uses(...)` target the lockfile doesn't record for its doc.
const NEW: &str = "code-block-uses-new";

/// The code for a recorded `uses(...)` target its doc no longer writes.
const OBSOLETE: &str = "code-block-uses-obsolete";

/// One `uses(...)` value written in a doc, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    /// The value in the block's info string; its file is the doc.
    pub(crate) at: Location,
    /// The target as the lockfile keys it: `path#symbol` or `ns:path#symbol`.
    pub(crate) target: String,
    /// The symbol's current hash.
    pub(crate) hash: Hash,
}

/// What `supdoc update` records: the `uses(...)` values the docs write now.
#[derive(Debug, Default)]
pub(crate) struct Current {
    /// Every value that resolves, with its current hash.
    pub(crate) written: Vec<Written>,
    /// The docs with a value that doesn't resolve. Their stale records are kept.
    pub(crate) unresolved: BTreeSet<String>,
    /// One `code-block-source-missing` error for each value that doesn't resolve.
    pub(crate) missing: Vec<Finding>,
}

/// The current hash of each `uses(...)` value in `blocks`, whatever the block's language.
///
/// Effect: reads every source file the values name, contained in the project root.
///
/// # Errors
///
/// [`CartridgeLoadError`] when a source's built-in cartridge doesn't load.
pub(crate) fn current<'block>(
    config: &Config,
    languages: &Languages,
    blocks: impl IntoIterator<Item = &'block CodeBlock>,
) -> Result<Current, CartridgeLoadError> {
    let mut current = Current::default();
    for block in blocks {
        let Some(info) = block.info.as_ref() else {
            continue;
        };
        let doc = info.location.file.as_str();
        for attribute in source_attributes(&info.text) {
            if attribute.kind != SourceKind::Uses {
                continue;
            }
            match source::read_use(config, languages, doc, attribute.value) {
                Ok(found) => current.written.push(Written {
                    at: info.location.clone(),
                    target: found.target,
                    hash: found.hash,
                }),
                Err(Unresolved::CartridgeLoad(error)) => return Err(error),
                Err(Unresolved::Missing(missing)) => {
                    let _ = current.unresolved.insert(doc.to_owned());
                    current.missing.push(missing.finding(info.location.clone()));
                }
            }
        }
    }
    Ok(current)
}

/// The lockfile key of `target` (`path#symbol` or `ns:path#symbol`) written in `doc`, or `None`
/// when either doesn't parse.
pub(crate) fn key(doc: &str, target: &str) -> Option<LinkKey> {
    let (path, symbol) = target.split_once('#')?;
    Some(LinkKey {
        source: doc.parse().ok()?,
        target: path.parse().ok()?,
        symbol: Some(symbol.parse().ok()?),
    })
}

/// The findings for `written` against `recorded`, the lockfile's `(doc, target, hash)` entries:
/// a `code-block-uses-changed` error for each value whose hash differs from its doc's record, a
/// `code-block-uses-new` error for each with no record, and a `code-block-uses-obsolete` warning
/// for each record its doc no longer writes, when `judged` says that doc's records may be judged.
pub(crate) fn judge<'lock>(
    written: &[Written],
    recorded: impl IntoIterator<Item = (&'lock str, String, Hash)>,
    judged: impl Fn(&str) -> bool,
) -> Vec<Finding> {
    let recorded: BTreeMap<(&str, String), Hash> = recorded
        .into_iter()
        .map(|(doc, target, hash)| ((doc, target), hash))
        .collect();
    let seen: BTreeSet<(&str, &str)> = written
        .iter()
        .map(|value| (value.at.file.as_str(), value.target.as_str()))
        .collect();
    let mut findings = Vec::new();
    for value in written {
        let doc = value.at.file.as_str();
        let finding = match recorded.get(&(doc, value.target.clone())) {
            Some(hash) if *hash == value.hash => None,
            Some(_) => Some(Finding::new(
                CHANGED,
                Severity::Error,
                format!(
                    "`{}`, which this block uses, changed since it was recorded",
                    value.target
                ),
            )),
            None => Some(Finding::new(
                NEW,
                Severity::Error,
                format!(
                    "{LOCK_FILE_NAME} doesn't record `{}`, which this block uses",
                    value.target
                ),
            )),
        };
        findings.extend(finding.map(|finding| {
            finding
                .with_location(value.at.clone())
                .with_help(review_help(&value.target, doc))
        }));
    }
    for (doc, target) in recorded.keys() {
        if seen.contains(&(*doc, target.as_str())) || !judged(doc) {
            continue;
        }
        findings.push(
            Finding::new(
                OBSOLETE,
                Severity::Warning,
                format!(
                    "{LOCK_FILE_NAME} records `uses({target})` for {doc}, which no longer writes it"
                ),
            )
            .with_help(format!("run `supdoc update {doc} --yes`")),
        );
    }
    findings
}

/// What to do about a changed or new `target` in `doc`.
fn review_help(target: &str, doc: &str) -> String {
    format!("review the block against `{target}`, then run `supdoc update {doc} --yes`")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(doc: &str, target: &str, hash: &[u8]) -> Written {
        Written {
            at: Location {
                file: doc
                    .parse()
                    .expect("the fixture doc should be a valid source"),
                range: 0..1,
                line: 1,
                column: 1,
            },
            target: target.to_owned(),
            hash: Hash::digest(hash),
        }
    }

    fn codes(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|finding| finding.code).collect()
    }

    const TARGET: &str = "src/lib.rs#Counter.bump";

    #[test]
    fn a_recorded_equal_hash_passes() {
        let findings = judge(
            &[written("docs/a.md", TARGET, b"v1")],
            [("docs/a.md", TARGET.to_owned(), Hash::digest(b"v1"))],
            |_| true,
        );

        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_recorded_different_hash_is_changed() {
        let findings = judge(
            &[written("docs/a.md", TARGET, b"v2")],
            [("docs/a.md", TARGET.to_owned(), Hash::digest(b"v1"))],
            |_| true,
        );

        assert_eq!(codes(&findings), [CHANGED]);
        assert_eq!(
            findings.first().map(|finding| finding.severity),
            Some(Severity::Error)
        );
    }

    #[test]
    fn an_unrecorded_target_is_new_even_when_another_doc_records_it() {
        let findings = judge(
            &[written("docs/a.md", TARGET, b"v1")],
            [("docs/b.md", TARGET.to_owned(), Hash::digest(b"v1"))],
            |doc| doc == "docs/a.md",
        );

        assert_eq!(codes(&findings), [NEW]);
    }

    #[test]
    fn a_record_no_longer_written_is_obsolete_only_when_its_doc_is_judged() {
        let recorded = [
            ("docs/a.md", TARGET.to_owned(), Hash::digest(b"v1")),
            ("docs/out.md", TARGET.to_owned(), Hash::digest(b"v1")),
        ];

        let findings = judge(&[], recorded, |doc| doc == "docs/a.md");

        assert_eq!(codes(&findings), [OBSOLETE]);
        assert_eq!(
            findings.first().map(|finding| finding.severity),
            Some(Severity::Warning)
        );
    }
}
