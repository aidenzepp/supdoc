//! The `Report` trait: what every piece of output data implements (07 "Rust design").

use std::io::{self, Write};

use serde_json::Value;

use crate::finding::Finding;
use crate::output::format::TextContext;
use crate::output::summary::Summary;

/// Every piece of output data implements this. JSON comes from `Serialize`, the schema from
/// `JsonSchema`, and the other formats from the methods here.
pub trait Report: serde::Serialize + schemars::JsonSchema {
    /// The command that produced this report: `check`, `update`, `list`...
    fn command(&self) -> &'static str;

    /// The link counts, when the report has them.
    ///
    /// They end the JSONL stream and the GitHub log.
    fn summary(&self) -> Option<&Summary> {
        None
    }

    /// Findings that aren't about one link. They go in the envelope's top-level `findings`.
    ///
    /// A link's own finding is nested in the link's serialized form instead (07 G8).
    fn findings(&self) -> &[Finding] {
        &[]
    }

    /// Every finding the report carries, nested ones included, for GitHub annotations.
    fn annotated_findings(&self) -> Vec<&Finding> {
        self.findings().iter().collect()
    }

    /// Whether the report leaves out results, for the envelope's `truncated`.
    fn truncated(&self) -> bool {
        false
    }

    /// Writes the report as text.
    ///
    /// # Errors
    ///
    /// Fails if writing to `out` fails.
    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()>;

    /// The report's records for JSONL, without the final summary line.
    ///
    /// The default splits the serialized report: each array under `links`, `changes`, `fixes`,
    /// `symbols` or `namespaces` becomes one record per element, tagged with its `type`.
    fn jsonl_records(&self) -> Vec<Value> {
        crate::output::jsonl::split_records(self)
    }
}
