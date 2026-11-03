//! The per-block pipeline that `check` runs: judges each code block and returns its findings.

use crate::config::Config;
use crate::finding::{Finding, Severity};

use super::block::{CodeBlock, Info};
use super::info::{InfoString, KNOWN_ATTRIBUTES, is_known_attribute};

/// The code for an info-string attribute supdoc doesn't know.
const UNKNOWN_ATTRIBUTE: &str = "code-block-unknown-attribute";

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
        .flat_map(unknown_attributes)
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
            .with_help(format!(
                "remove `{attribute}` or correct it to one of {KNOWN_ATTRIBUTES}"
            ))
        });
    comma.into_iter().chain(unknown).collect()
}
