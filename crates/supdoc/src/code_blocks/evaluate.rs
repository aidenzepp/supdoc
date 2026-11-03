//! The per-block pipeline that `check` runs: judges each code block and returns its findings.

use crate::config::Config;
use crate::finding::{Finding, Severity};

use super::block::{CodeBlock, Info};
use super::info::{
    InfoString, KNOWN_ATTRIBUTES, is_known_attribute, parenthesized_spelling, reasoned,
};

/// The code for an info-string attribute supdoc doesn't know.
const UNKNOWN_ATTRIBUTE: &str = "code-block-unknown-attribute";

/// The code for an `ignore`, `no_format` or `no_lint` written without a reason.
const IGNORE_REASON: &str = "code-block-ignore-reason";

/// The findings for `blocks`, in block order. Nothing when `config` leaves
/// `[features.code-blocks]` off.
#[must_use]
pub(crate) fn evaluate(config: &Config, blocks: &[CodeBlock]) -> Vec<Finding> {
    if config.code_blocks().is_none() {
        return Vec::new();
    }
    blocks
        .iter()
        .filter_map(|block| block.info.as_ref())
        .flat_map(|info| {
            let mut findings = unknown_attributes(info);
            findings.extend(missing_reasons(info));
            findings
        })
        .collect()
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
