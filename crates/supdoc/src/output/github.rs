//! GitHub Actions workflow commands, shown on the PR diff (07 "GitHub output").

use std::io::{self, Write};

use crate::finding::{Finding, Severity};
use crate::output::format::Verbosity;
use crate::output::paths::PathStyle;

/// Writes one `::error`, `::warning` or `::notice` line per finding the verbosity shows.
///
/// # Errors
///
/// Fails if writing to `out` fails.
pub fn render_annotations(
    paths: &PathStyle,
    verbosity: Verbosity,
    findings: &[&Finding],
    out: &mut dyn Write,
) -> io::Result<()> {
    for finding in findings {
        if verbosity.shows(finding.severity) {
            writeln!(out, "{}", annotation(paths, finding))?;
        }
    }
    Ok(())
}

/// One workflow command for `finding`.
#[must_use]
pub fn annotation(paths: &PathStyle, finding: &Finding) -> String {
    let command = match finding.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "notice",
    };
    let mut properties = Vec::new();
    if let Some(location) = &finding.location {
        properties.push(format!(
            "file={}",
            escape_property(&paths.github(&location.file))
        ));
        properties.push(format!("line={}", location.line));
        properties.push(format!("col={}", location.column));
    }
    properties.push(format!(
        "title={}",
        escape_property(&format!("supdoc {}", finding.code))
    ));
    format!(
        "::{command} {}::{}",
        properties.join(","),
        escape_data(&message(finding))
    )
}

/// The message and, after it, the help with its first letter capitalized.
fn message(finding: &Finding) -> String {
    let Some(help) = &finding.help else {
        return finding.message.clone();
    };
    let mut letters = help.chars();
    let help: String = letters
        .next()
        .map(|first| first.to_uppercase().chain(letters).collect())
        .unwrap_or_default();
    let separator = if finding.message.ends_with('.') {
        " "
    } else {
        ". "
    };
    format!("{}{separator}{help}", finding.message)
}

/// Escapes the message part of a workflow command: `%`, carriage return and newline.
#[must_use]
pub fn escape_data(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Escapes a property value: everything a message escapes, plus `:` and `,`.
#[must_use]
pub fn escape_property(text: &str) -> String {
    escape_data(text).replace(':', "%3A").replace(',', "%2C")
}
