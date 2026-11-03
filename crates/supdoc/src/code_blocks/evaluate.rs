//! The per-block pipeline that `check` runs: judges each code block and returns its findings.

use std::ops::Range;

use crate::config::Config;
use crate::finding::{Finding, Location, Severity};
use crate::read::cartridge::Languages;
use crate::read::error::CartridgeLoadError;

use super::block::{CodeBlock, Info};
use super::info::{
    InfoString, KNOWN_ATTRIBUTES, is_known_attribute, parenthesized_spelling, reasoned,
    source_attributes,
};
use super::source::{self, Unresolved};

/// The code for an info-string attribute supdoc doesn't know.
const UNKNOWN_ATTRIBUTE: &str = "code-block-unknown-attribute";

/// The code for an `ignore`, `no_format` or `no_lint` written without a reason.
const IGNORE_REASON: &str = "code-block-ignore-reason";

/// The findings for `blocks`, in block order, reading each `from(...)` and `uses(...)` source
/// with `languages`. Nothing when `config` leaves `[features.code-blocks]` off.
///
/// Effect: reads every source file the blocks name, contained in the project root.
///
/// # Errors
///
/// [`CartridgeLoadError`] when a source's built-in cartridge doesn't load: a supdoc bug that
/// fails the run (exit 70).
pub(crate) fn evaluate(
    config: &Config,
    languages: &Languages,
    blocks: &[CodeBlock],
) -> Result<Vec<Finding>, CartridgeLoadError> {
    let mut findings = Vec::new();
    if config.code_blocks().is_none() {
        return Ok(findings);
    }
    for info in blocks.iter().filter_map(|block| block.info.as_ref()) {
        findings.extend(unknown_attributes(info));
        findings.extend(missing_reasons(info));
        findings.extend(missing_sources(config, languages, info)?);
    }
    Ok(findings)
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

/// One `code-block-source-missing` error at each `from(...)` or `uses(...)` value in `info` that
/// doesn't resolve to a contained, readable file or symbol.
///
/// # Errors
///
/// [`CartridgeLoadError`] when a source's built-in cartridge doesn't load.
fn missing_sources(
    config: &Config,
    languages: &Languages,
    info: &Info,
) -> Result<Vec<Finding>, CartridgeLoadError> {
    let doc = info.location.file.as_str();
    let mut findings = Vec::new();
    for attribute in source_attributes(&info.text) {
        let missing = match source::read(config, languages, doc, attribute.kind, attribute.value) {
            Ok(_text) => continue,
            Err(Unresolved::CartridgeLoad(error)) => return Err(error),
            Err(Unresolved::Missing(missing)) => missing,
        };
        findings.extend(value_location(info, attribute.range).map(|at| missing.finding(at)));
    }
    Ok(findings)
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
