//! The JSON envelope and the JSON form of findings (07 G8, G10).

use serde::Serialize;

use crate::finding::{Finding, Fix, Location, Severity};
use crate::output::report::Report;
use crate::path::AbsolutePath;

/// The version of the JSON shape. Adding optional fields keeps it; removing or renaming bumps it
/// (07 G10). New values of an existing enum (states, actions, kinds, JSONL record types) also keep
/// it, so consumers must tolerate values they do not know.
pub const SCHEMA_VERSION: u32 = 1;

/// The one object every command writes under `--json`, with the same top-level keys each time.
///
/// Its schema is the one `supdoc schema` prints, so the JSON written and the schema published
/// come from this one type.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Envelope<'a, R: Report> {
    /// [`SCHEMA_VERSION`].
    schema_version: u32,
    /// The command that produced `data`.
    command: &'static str,
    /// `true` exactly when the exit code is 0.
    ok: bool,
    /// The absolute project root, which every other path is relative to.
    root: String,
    /// The command's own payload.
    data: &'a R,
    /// Findings that aren't about one link.
    findings: Vec<FindingJson>,
    /// Whether results were left out.
    truncated: bool,
}

impl<'a, R: Report> Envelope<'a, R> {
    /// Wraps `report`. `ok` must be true exactly when the process will exit 0.
    #[must_use]
    pub fn new(report: &'a R, ok: bool, root: &AbsolutePath) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            command: report.command(),
            ok,
            root: root.display().to_string(),
            data: report,
            findings: report.findings().iter().map(FindingJson::from).collect(),
            truncated: report.truncated(),
        }
    }
}

/// A finding as JSON shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct FindingJson {
    /// The stable code, such as `link-failed`.
    pub kind: String,
    /// How bad it is.
    pub severity: SeverityJson,
    /// What happened, in one line.
    pub message: String,
    /// What to do next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Where it is. Left out of a link's nested finding, which shares the link's location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationJson>,
    /// The one correct edit, when there is exactly one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixJson>,
}

impl FindingJson {
    /// The finding as nested inside the link it explains: no `location`, since the link has them.
    #[must_use]
    pub fn nested(finding: &Finding) -> Self {
        Self {
            location: None,
            ..Self::from(finding)
        }
    }
}

impl From<&Finding> for FindingJson {
    fn from(finding: &Finding) -> Self {
        Self {
            kind: finding.code.to_owned(),
            severity: finding.severity.into(),
            message: finding.message.clone(),
            help: finding.help.clone(),
            location: finding.location.as_ref().map(LocationJson::from),
            fix: finding.fix.as_ref().map(FixJson::from),
        }
    }
}

/// A severity as JSON shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SeverityJson {
    /// Fails `check`.
    Error,
    /// Reported; fails `check` only under `--deny-warnings`.
    Warning,
    /// Information only.
    Note,
}

impl From<Severity> for SeverityJson {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Error => Self::Error,
            Severity::Warning => Self::Warning,
            Severity::Note => Self::Note,
        }
    }
}

/// A place in a file as JSON shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct LocationJson {
    /// The file, relative to the project root.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
    /// Start byte offset.
    pub start: usize,
    /// End byte offset.
    pub end: usize,
}

impl From<&Location> for LocationJson {
    fn from(location: &Location) -> Self {
        Self {
            file: location.file.as_str().to_owned(),
            line: location.line,
            column: location.column,
            start: location.range.start,
            end: location.range.end,
        }
    }
}

/// An exact edit as JSON shows it, with byte offsets so a script can apply it without recounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct FixJson {
    /// The file to edit, relative to the project root.
    pub file: String,
    /// Start byte offset.
    pub start: usize,
    /// End byte offset.
    pub end: usize,
    /// The new text.
    pub replacement: String,
}

impl From<&Fix> for FixJson {
    fn from(fix: &Fix) -> Self {
        Self {
            file: fix.file.as_str().to_owned(),
            start: fix.range.start,
            end: fix.range.end,
            replacement: fix.replacement.clone(),
        }
    }
}

/// A fatal error as JSON shows it, written to stderr (06 X8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct FatalJson {
    /// Always `true`, so a reader can tell this object from a report.
    pub error: bool,
    /// The stable code, such as `config-extends-missing`.
    pub kind: String,
    /// What happened, in one line.
    pub message: String,
    /// What to do next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// The process exit code.
    pub exit_code: u8,
    /// Where it happened, when it is about one place in a file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<FatalLocationJson>,
}

/// Where a fatal error happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct FatalLocationJson {
    /// The file, relative to the project root.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
}

impl FatalJson {
    /// A fatal error made from `finding`, exiting with `exit_code`.
    #[must_use]
    pub fn new(finding: &Finding, exit_code: u8) -> Self {
        Self {
            error: true,
            kind: finding.code.to_owned(),
            message: finding.message.clone(),
            help: finding.help.clone(),
            exit_code,
            location: finding.location.as_ref().map(|location| FatalLocationJson {
                file: location.file.as_str().to_owned(),
                line: location.line,
                column: location.column,
            }),
        }
    }
}
