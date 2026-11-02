//! Rendering reports as text, JSON, JSONL and GitHub annotations.
//!
//! [`Output`] is the one place supdoc writes to stdout and stderr. Reports are plain values that
//! implement [`Report`], so every format of every report can be tested without files or a terminal.

/// How output is shown: format, verbosity, and the context text rendering needs.
pub(crate) mod format;

/// How paths are shown in each format.
pub(crate) mod paths;

/// The closing note of a preview.
pub(crate) mod preview;

/// The summary line and its counts.
pub(crate) mod summary;

/// The `Report` trait every piece of output data implements.
pub(crate) mod report;

/// Text rendering of findings with `annotate-snippets`.
pub(crate) mod text;

/// The JSON envelope and the wire form of findings.
pub(crate) mod json;

/// JSONL records.
pub(crate) mod jsonl;

/// GitHub Actions workflow commands.
pub(crate) mod github;

/// The `Output` value that owns stdout and stderr.
pub(crate) mod sink;

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod tests;

pub(crate) use format::{OutputFormat, TextContext, Verbosity};
pub(crate) use paths::PathStyle;
pub(crate) use preview::preview_note;
pub(crate) use report::{RecordArray, Report};
pub(crate) use sink::{Output, OutputError, OutputSettings};
pub(crate) use summary::Summary;
