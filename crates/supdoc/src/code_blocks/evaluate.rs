//! The per-block pipeline that `check` runs: judges each code block and returns its findings
//! and the count of blocks in each state.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::Path;

use crate::config::Config;
use crate::finding::{Finding, Location, Severity};
use crate::lock::model::Lockfile;
use crate::read::cartridge::Languages;
use crate::read::error::CartridgeLoadError;

use super::block::{CodeBlock, Info};
use super::excerpt;
use super::info::{
    InfoString, KNOWN_ATTRIBUTES, SourceKind, is_known_attribute, parenthesized_spelling, reasoned,
    source_attributes,
};
use super::source::{self, SourceText, Unresolved};
use super::states::{BlockState, Excerpt, state_of};
use super::uses::{self, Written};

pub(crate) use super::states::CodeBlockCounts;

/// The code for an info-string attribute supdoc doesn't know.
const UNKNOWN_ATTRIBUTE: &str = "code-block-unknown-attribute";

/// The code for an `ignore`, `no_format` or `no_lint` written without a reason.
const IGNORE_REASON: &str = "code-block-ignore-reason";

/// What judging the code blocks found.
#[derive(Debug, Default)]
pub(crate) struct Evaluation {
    /// The findings, in block order, then the `uses(...)` findings.
    pub(crate) findings: Vec<Finding>,
    /// How many blocks are in each state. `None` when `[features.code-blocks]` is off.
    pub(crate) counts: Option<CodeBlockCounts>,
}

/// The findings for `blocks`, in block order, reading each `from(...)` and `uses(...)` source
/// with `languages`, and one `code-block-stale` error for each `from(...)` block whose body
/// differs from its parts. Then the `uses(...)` findings against `lock`'s `[code-blocks]`
/// section, judging a doc's recorded targets obsolete only when `in_scope` holds for it and none
/// of its `uses(...)` values fails to resolve. With them, each block's [`BlockState`], counted.
/// Nothing when `config` leaves `[features.code-blocks]` off.
///
/// Effect: reads every source file the blocks name, contained in the project root, and each doc
/// holding a `from(...)` block.
///
/// # Errors
///
/// [`CartridgeLoadError`] when a source's built-in cartridge doesn't load: a supdoc bug that
/// fails the run (exit 70).
pub(crate) fn evaluate(
    config: &Config,
    languages: &Languages,
    blocks: &[CodeBlock],
    lock: &Lockfile,
    in_scope: impl Fn(&str) -> bool,
) -> Result<Evaluation, CartridgeLoadError> {
    let Some(code_blocks) = config.code_blocks() else {
        return Ok(Evaluation::default());
    };
    let mut findings = Vec::new();
    let mut states: Vec<BlockState> = Vec::with_capacity(blocks.len());
    let mut doc: Option<(&str, Option<String>)> = None;
    let mut written = Vec::new();
    let mut unresolved_uses = BTreeSet::new();
    for block in blocks {
        let mut outcome = None;
        if let Some(info) = block.info.as_ref() {
            findings.extend(unknown_attributes(info));
            findings.extend(missing_reasons(info));
            let sources = sources(config, languages, info)?;
            findings.extend(sources.missing);
            written.extend(sources.uses);
            unresolved_uses.extend(sources.use_missing.then_some(block.fence.file.as_str()));
            outcome = sources.from_missing.then_some(Excerpt::SourceMissing);
            if let Some(parts) = sources.parts {
                let file = block.fence.file.as_str();
                if doc.as_ref().is_none_or(|(read, _)| *read != file) {
                    doc = Some((file, read_doc(config, file)));
                }
                let stale = doc
                    .as_ref()
                    .and_then(|(_, text)| text.as_ref())
                    .and_then(|text| {
                        let line_endings = config.hash_settings().line_endings;
                        let expected = excerpt::expected(&parts, line_endings);
                        excerpt::stale(block, text, &expected, line_endings)
                    });
                outcome = Some(if stale.is_some() {
                    Excerpt::Stale
                } else {
                    Excerpt::Current
                });
                findings.extend(stale);
            }
        }
        states.push(state_of(block, outcome, |language| {
            code_blocks.checks(language)
        }));
    }
    findings.extend(uses::judge(&written, lock.recorded_uses(), |doc| {
        in_scope(doc) && !unresolved_uses.contains(doc)
    }));
    Ok(Evaluation {
        findings,
        counts: Some(CodeBlockCounts::tally(&states)),
    })
}

/// The text of `doc`, relative to the project root, as it is on disk now. `None` when it can't
/// be read; its blocks then get no stale finding.
///
/// Effect: reads the doc.
fn read_doc(config: &Config, doc: &str) -> Option<String> {
    std::fs::read_to_string(config.root().join_path(Path::new(doc))).ok()
}

/// What a block's `from(...)` and `uses(...)` values resolved to.
struct Sources {
    /// One `code-block-source-missing` error for each value that doesn't resolve.
    missing: Vec<Finding>,
    /// Every `from(...)` part's text, in written order, when the block has one and each
    /// resolves.
    parts: Option<Vec<SourceText>>,
    /// Every `uses(...)` value that resolves, with its target and current hash.
    uses: Vec<Written>,
    /// Whether some `uses(...)` value doesn't resolve.
    use_missing: bool,
    /// Whether some `from(...)` value doesn't resolve.
    from_missing: bool,
}

/// One `code-block-unknown-attribute` warning at `info` for a language word holding commas, and
/// one for each attribute outside the closed list.
fn unknown_attributes(info: &Info) -> Vec<Finding> {
    let parsed = InfoString::parse(&info.text);
    let comma = parsed.spaced_language().map(|spaced| {
        Finding::new(
            UNKNOWN_ATTRIBUTE,
            Severity::Warning,
            format!(
                "the info string `{}` joins its attributes with commas",
                parsed.language
            ),
        )
        .with_location(info.location.clone())
        .with_help(format!(
            "separate the language and each attribute with spaces: write `{spaced}`"
        ))
    });
    let unknown = parsed
        .attributes
        .iter()
        .filter(|attribute| !is_known_attribute(attribute))
        .map(|attribute| {
            Finding::new(
                UNKNOWN_ATTRIBUTE,
                Severity::Warning,
                format!(
                    "the `{}` block's attribute `{attribute}` isn't one supdoc knows",
                    parsed.language
                ),
            )
            .with_location(info.location.clone())
            .with_help(match parenthesized_spelling(attribute) {
                Some(spelling) => format!("write `{attribute}` as `{spelling}`"),
                None => format!("remove `{attribute}` or correct it to one of {KNOWN_ATTRIBUTES}"),
            })
        });
    comma.into_iter().chain(unknown).collect()
}

/// One `code-block-ignore-reason` warning at `info` for each `ignore`, `no_format` or `no_lint`
/// without a reason. The attribute still takes effect.
fn missing_reasons(info: &Info) -> Vec<Finding> {
    let parsed = InfoString::parse(&info.text);
    parsed
        .attributes
        .iter()
        .filter_map(|attribute| reasoned(attribute))
        .filter(|found| found.reason.is_none())
        .map(|found| {
            let name = found.name;
            Finding::new(
                IGNORE_REASON,
                Severity::Warning,
                format!("the `{}` block's `{name}` doesn't say why", parsed.language),
            )
            .with_location(info.location.clone())
            .with_help(format!("write it as `{name}(<why>)`"))
        })
        .collect()
}

/// Reads every `from(...)` and `uses(...)` value in `info`: one `code-block-source-missing`
/// error at each that doesn't resolve to a contained, readable file or symbol, the `from(...)`
/// parts' text when all of them resolve, and each resolved `uses(...)` target with its hash.
///
/// # Errors
///
/// [`CartridgeLoadError`] when a source's built-in cartridge doesn't load.
fn sources(
    config: &Config,
    languages: &Languages,
    info: &Info,
) -> Result<Sources, CartridgeLoadError> {
    let doc = info.location.file.as_str();
    let mut missing = Vec::new();
    let mut parts = Vec::new();
    let mut uses = Vec::new();
    let mut part_missing = false;
    let mut use_missing = false;
    for attribute in source_attributes(&info.text) {
        let at = value_location(info, attribute.range);
        let result = match attribute.kind {
            SourceKind::From => {
                source::read(config, languages, doc, attribute.kind, attribute.value)
                    .map(|text| parts.push(text))
            }
            SourceKind::Uses => {
                source::read_use(config, languages, doc, attribute.value).map(|found| {
                    uses.extend(at.clone().map(|at| Written {
                        at,
                        target: found.target,
                        hash: found.hash,
                    }));
                })
            }
        };
        let unresolved = match result {
            Ok(()) => continue,
            Err(Unresolved::CartridgeLoad(error)) => return Err(error),
            Err(Unresolved::Missing(unresolved)) => unresolved,
        };
        part_missing |= attribute.kind == SourceKind::From;
        use_missing |= attribute.kind == SourceKind::Uses;
        missing.extend(at.map(|at| unresolved.finding(at)));
    }
    let parts = (!part_missing && !parts.is_empty()).then_some(parts);
    Ok(Sources {
        missing,
        parts,
        uses,
        use_missing,
        from_missing: part_missing,
    })
}

/// Where `range`, a byte range of `info`'s text, lies in the doc. An info string is one line, so
/// the value shares its line.
fn value_location(info: &Info, range: Range<usize>) -> Option<Location> {
    let before = info.text.get(..range.start)?.chars().count();
    let start = info.location.range.start + range.start;
    Some(Location {
        file: info.location.file.clone(),
        range: start..info.location.range.start + range.end,
        line: info.location.line,
        column: info.location.column + u32::try_from(before).ok()?,
    })
}
