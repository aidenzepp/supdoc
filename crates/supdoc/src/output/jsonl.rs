//! JSONL: one self-describing object per line, ending with a summary (07 G9).

use serde_json::{Map, Value};

use crate::output::json::{FindingJson, SCHEMA_VERSION};
use crate::output::report::Report;

/// The arrays of a report that become records, in the order they're written, and each record's
/// `type`.
const RECORD_ARRAYS: [(&str, &str); 5] = [
    ("links", "link"),
    ("changes", "change"),
    ("fixes", "fix"),
    ("symbols", "symbol"),
    ("namespaces", "namespace"),
];

/// Splits a serialized report into one tagged record per element of its record arrays.
///
/// A report that can't be serialized yields one `error` record saying so, so the stream never
/// silently loses data.
#[must_use]
pub fn split_records<R: serde::Serialize + ?Sized>(report: &R) -> Vec<Value> {
    let value = match serde_json::to_value(report) {
        Ok(value) => value,
        Err(error) => {
            return vec![tagged(
                "error",
                Value::String(format!("could not serialize the report: {error}")),
            )];
        }
    };
    let Value::Object(mut fields) = value else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for (key, record_type) in RECORD_ARRAYS {
        if let Some(Value::Array(items)) = fields.remove(key) {
            records.extend(items.into_iter().map(|item| tagged(record_type, item)));
        }
    }
    records
}

/// Every line of a report: its records, its loose findings, then the summary.
#[must_use]
pub fn lines<R: Report>(report: &R, ok: bool) -> Vec<Value> {
    let mut lines = report.jsonl_records();
    for finding in report.findings() {
        lines.push(tagged(
            "finding",
            serde_json::to_value(FindingJson::from(finding)).unwrap_or(Value::Null),
        ));
    }
    lines.push(summary_line(report, ok));
    lines
}

/// The closing line: the report's top-level scalar fields, the command, whether it succeeded, the
/// link counts, and the schema version.
///
/// The summary's own keys win over a report field of the same name.
fn summary_line<R: Report>(report: &R, ok: bool) -> Value {
    let mut fields = scalar_fields(report);
    let _type = fields.insert("type".to_owned(), Value::String("summary".to_owned()));
    let _command = fields.insert("command".to_owned(), Value::from(report.command()));
    let _ok = fields.insert("ok".to_owned(), Value::Bool(ok));
    if let Some(Ok(Value::Object(counts))) = report.summary().map(serde_json::to_value) {
        fields.extend(counts);
    }
    let _version = fields.insert("schema_version".to_owned(), Value::from(SCHEMA_VERSION));
    Value::Object(fields)
}

/// The top-level bool, string and number fields of the serialized report.
///
/// Arrays, objects and nulls are left out; arrays become records instead.
fn scalar_fields<R: serde::Serialize + ?Sized>(report: &R) -> Map<String, Value> {
    let Ok(Value::Object(fields)) = serde_json::to_value(report) else {
        return Map::new();
    };
    fields
        .into_iter()
        .filter(|(_, value)| matches!(value, Value::Bool(_) | Value::String(_) | Value::Number(_)))
        .collect()
}

/// `value` with a `type` field. A non-object value is kept under `value`.
fn tagged(record_type: &str, value: Value) -> Value {
    let mut fields = match value {
        Value::Object(fields) => fields,
        other => {
            let mut wrapped = Map::new();
            let _value = wrapped.insert("value".to_owned(), other);
            wrapped
        }
    };
    let _type = fields.insert("type".to_owned(), Value::String(record_type.to_owned()));
    Value::Object(fields)
}
