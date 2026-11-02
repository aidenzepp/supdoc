//! The `Report` trait: what every piece of output data implements (07 "Rust design").

use std::io::{self, Write};

use serde_json::Value;

use crate::finding::Finding;
use crate::output::format::TextContext;
use crate::output::summary::Summary;

/// An array field of a report that JSONL writes as one record per element.
#[derive(Debug, Clone, Copy)]
pub struct RecordArray {
    /// The field's name in the serialized report.
    pub key: &'static str,
    /// The `type` each record is tagged with.
    pub record_type: &'static str,
}

impl RecordArray {
    /// A record array read from field `key`, tagged `record_type`.
    #[must_use]
    pub const fn new(key: &'static str, record_type: &'static str) -> Self {
        Self { key, record_type }
    }
}

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

    /// The array fields that JSONL writes as records, in stream order.
    ///
    /// Every other field of the report goes on the summary line.
    fn record_arrays(&self) -> &'static [RecordArray] {
        &[]
    }

    /// The report's records for JSONL, without the final summary line.
    ///
    /// The default splits the arrays named by [`Report::record_arrays`], one record per element,
    /// tagged with the array's `record_type`.
    fn jsonl_records(&self) -> Vec<Value> {
        crate::output::jsonl::split_records(self)
    }
}
