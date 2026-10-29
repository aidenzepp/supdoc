//! Rendering reports as text, JSON, JSONL and GitHub annotations (07).
//!
//! [`Output`] is the one place supdoc writes to stdout and stderr. Reports are plain values that
//! implement [`Report`], so every format of every report can be tested without files or a terminal.

/// JSON Schema for every report, and the `supdoc schema` export (07 G10).
pub mod schema;

/// How output is shown: format, verbosity, and the context text rendering needs.
pub mod format;

/// How paths are shown in each format (07 G7).
pub mod paths;

/// The summary line and its counts.
pub mod summary;

/// The `Report` trait every piece of output data implements.
pub mod report;

/// Text rendering of findings with `annotate-snippets`.
pub mod text;

/// The JSON envelope and the wire form of findings.
pub mod json;

/// JSONL records.
pub mod jsonl;

/// GitHub Actions workflow commands.
pub mod github;

/// The `Output` value that owns stdout and stderr.
pub mod sink;

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod tests;

pub use format::{OutputFormat, TextContext, Verbosity};
pub use json::{Envelope, FindingJson, SCHEMA_VERSION};
pub use paths::PathStyle;
pub use report::Report;
pub use sink::{Output, OutputError, OutputSettings};
pub use summary::Summary;
