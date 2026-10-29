//! `supdoc schema`: prints the JSON Schema of a command's `--json` output (06 X10, 07 G10).
//!
//! The schema is the one the command itself publishes through its `schema()` function, so the
//! JSON a command writes and the schema printed here come from the same types. Each envelope
//! schema pins its `command`, so one command's output validates against its own schema only. The
//! schema never loads the config (06 X14).

use std::io::{self, Write};

use clap::ValueEnum as _;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::output::json::FatalJson;
use crate::output::{Output, OutputError, Report, TextContext};

use super::args::{Inspect, SchemaArgs, SchemaTarget};
use super::{
    CliError, Context, Outcome, check, config, fix, init, list, rename, resolve, skill, update,
};

/// The JSON Schema document the run prints. Under `--json` it is the envelope's `data` itself.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(transparent)]
struct SchemaReport {
    /// A command's schema, or the document describing every built command and the error.
    schema: Value,
}

impl Report for SchemaReport {
    fn command(&self) -> &'static str {
        "schema"
    }

    fn render_text(&self, _context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(pretty(&self.schema).as_bytes())
    }

    /// The schema is the stream's one record, since the summary line carries only scalars.
    fn jsonl_records(&self) -> Vec<Value> {
        let mut record = Map::new();
        let _type = record.insert("type".to_owned(), Value::String("schema".to_owned()));
        let _schema = record.insert("schema".to_owned(), self.schema.clone());
        vec![Value::Object(record)]
    }
}

/// The JSON Schema dialect every printed document declares.
const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The key under `$defs` of the error schema, and the name of `schema/error.json`.
const ERROR: &str = "error";

/// Runs `supdoc schema`, printing the schema document through `output`.
///
/// With a `COMMAND` the document is that command's schema. Without one it is one 2020-12 document
/// holding the schema of every command this build has built, and of the error, under `$defs`,
/// with a top-level `oneOf` over them.
///
/// # Errors
///
/// Returns `cli/not-built` for a named command that is not built yet, and `cli/schema-write` when
/// standard output cannot be written for a reason other than a closed pipe.
pub fn run(
    args: &SchemaArgs,
    _context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let schema = match args.command {
        Some(target) => schema_value(target)?,
        None => combined_schema()?,
    };
    let report = SchemaReport { schema };
    output
        .artifact(&report, &pretty(&report.schema), true)
        .map_err(|error| write_failed(&error))?;
    Ok(Outcome::Clean)
}

/// The schema `target` publishes, as a JSON value.
///
/// # Errors
///
/// Returns `cli/not-built` while the command is not built, and `cli/schema-serialize` if the
/// schema cannot become JSON.
fn schema_value(target: SchemaTarget) -> Result<Value, CliError> {
    let mut schema = to_value(schema_of(target)?, &name(target))?;
    pin_command(&mut schema, &command_words(target));
    Ok(schema)
}

/// The schema of the fatal error `--json` writes to stderr.
///
/// # Errors
///
/// Returns `cli/schema-serialize` if the schema cannot become JSON.
fn error_value() -> Result<Value, CliError> {
    to_value(schemars::schema_for!(FatalJson), ERROR)
}

/// `schema` as JSON.
///
/// # Errors
///
/// Returns `cli/schema-serialize`, naming `what`, if the schema cannot become JSON.
fn to_value(schema: schemars::Schema, what: &str) -> Result<Value, CliError> {
    serde_json::to_value(schema).map_err(|error| {
        CliError::new(
            "cli/schema-serialize",
            format!("could not turn the schema of `{what}` into JSON: {error}"),
            "this is a supdoc bug; report it with the command you ran",
            CliError::BUG_EXIT_CODE,
        )
    })
}

/// The command `target` names as an envelope's `command` holds it: `config show`, not
/// `config-show`.
fn command_words(target: SchemaTarget) -> String {
    name(target).replace('-', " ")
}

/// Pins the envelope's `command` property to `command`, so only that command's output validates.
fn pin_command(schema: &mut Value, command: &str) {
    if let Some(property) = schema.pointer_mut("/properties/command")
        && let Some(property) = property.as_object_mut()
    {
        let _type = property.insert("type".to_owned(), Value::String("string".to_owned()));
        let _const = property.insert("const".to_owned(), Value::String(command.to_owned()));
    }
}

/// The schema `target`'s command publishes.
///
/// # Errors
///
/// Returns `cli/not-built` while the command is not built.
fn schema_of(target: SchemaTarget) -> Result<schemars::Schema, CliError> {
    match target {
        SchemaTarget::Check => check::schema(),
        SchemaTarget::Update => update::schema(),
        SchemaTarget::Fix => fix::schema(),
        SchemaTarget::List => list::schema(),
        SchemaTarget::Resolve => resolve::schema(),
        SchemaTarget::ConfigShow => config::schema(Inspect::ConfigShow),
        SchemaTarget::NamespaceList => config::schema(Inspect::NamespaceList),
        SchemaTarget::NamespaceRename => rename::schema(),
        SchemaTarget::Init => init::schema(),
        SchemaTarget::SkillInstall => skill::schema(),
    }
}

/// The name a user types for `target`, which is also its file name under `schema/`.
fn name(target: SchemaTarget) -> String {
    target
        .to_possible_value()
        .map_or_else(String::new, |value| value.get_name().to_owned())
}

/// The one document `supdoc schema` prints without a COMMAND: every built command's schema and
/// the error's under `$defs`, and a top-level `oneOf` over them.
///
/// Each entry is its own schema resource, with an `$id` and its own `$defs`, so the `#/$defs/...`
/// references inside it keep resolving. A command that is not built has no JSON output to
/// describe yet, so it is left out rather than failing the rest.
///
/// # Errors
///
/// Returns `cli/schema-serialize` if a schema cannot become JSON.
fn combined_schema() -> Result<Value, CliError> {
    let mut entries = Vec::new();
    for target in SchemaTarget::value_variants() {
        match schema_value(*target) {
            Ok(schema) => entries.push((name(*target), schema)),
            Err(error) if error.code() == CliError::NOT_BUILT => {}
            Err(error) => return Err(error),
        }
    }
    entries.push((ERROR.to_owned(), error_value()?));

    let mut defs = Map::new();
    let mut one_of = Vec::new();
    for (key, mut schema) in entries {
        if let Some(object) = schema.as_object_mut() {
            let _dialect = object.remove("$schema");
            let _id = object.insert("$id".to_owned(), Value::String(format!("urn:supdoc:{key}")));
        }
        let _unreplaced = defs.insert(key.clone(), schema);
        one_of.push(serde_json::json!({ "$ref": format!("#/$defs/{key}") }));
    }

    let mut document = Map::new();
    let _dialect = document.insert("$schema".to_owned(), Value::String(DIALECT.to_owned()));
    let _title = document.insert("title".to_owned(), Value::String("supdoc".to_owned()));
    let _defs = document.insert("$defs".to_owned(), Value::Object(defs));
    let _one_of = document.insert("oneOf".to_owned(), Value::Array(one_of));
    Ok(Value::Object(document))
}

/// `value` as indented JSON ending in a newline, the form of the files under `schema/`.
fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| unreachable!("a JSON value always serializes"));
    text.push('\n');
    text
}

/// The error for a schema that could not be written.
fn write_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/schema-write",
        format!("could not write the schema: {error}"),
        "check that standard output is open and writable, or redirect it to a file",
        CliError::IO_EXIT_CODE,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The `schema/` directory at the repository root, where releases ship the schemas.
    fn schema_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schema")
    }

    /// Asserts `shipped` is exactly what `supdoc schema` prints for `file`'s schema.
    fn assert_file_current(file: &std::path::Path, schema: &Value, regenerate: &str) {
        let shipped = std::fs::read_to_string(file).unwrap_or_else(|error| {
            panic!("{} should exist: {error}", file.display());
        });
        assert_eq!(
            shipped,
            pretty(schema),
            "{} is stale; regenerate it with `{regenerate}`",
            file.display()
        );
    }

    #[test]
    fn shipped_schema_files_match_what_each_built_command_publishes() {
        let mut checked = 0;
        for target in SchemaTarget::value_variants() {
            let Ok(schema) = schema_value(*target) else {
                continue;
            };
            let file = schema_dir().join(format!("{}.json", name(*target)));
            assert_file_current(&file, &schema, &format!("supdoc schema {}", name(*target)));
            checked += 1;
        }
        assert!(checked > 0, "at least one command should be built");
    }

    #[test]
    fn the_shipped_error_schema_matches_the_fatal_error_type() {
        let schema = error_value().expect("the error schema should serialize");
        let file = schema_dir().join(format!("{ERROR}.json"));
        assert_file_current(
            &file,
            &schema,
            "supdoc schema > schema/error.json (error entry)",
        );
    }

    #[test]
    fn every_schema_file_belongs_to_a_built_command_or_is_the_error() {
        let entries = std::fs::read_dir(schema_dir()).expect("schema/ should exist");
        for entry in entries {
            let path = entry.expect("schema/ should be readable").path();
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("schema file names should be UTF-8");
            if stem == ERROR {
                continue;
            }
            let target = SchemaTarget::value_variants()
                .iter()
                .find(|target| name(**target) == stem)
                .unwrap_or_else(|| panic!("{} names no command", path.display()));
            assert!(
                schema_value(*target).is_ok(),
                "{} ships a schema for a command that is not built",
                path.display()
            );
        }
    }

    #[test]
    fn a_named_command_that_is_not_built_says_so() {
        for target in SchemaTarget::value_variants() {
            let Err(error) = schema_value(*target) else {
                continue;
            };

            assert_eq!(error.code(), CliError::NOT_BUILT, "`{}`", name(*target));
            let spoken = format!("supdoc {}", name(*target).replace('-', " "));
            assert!(
                error.message().contains(&spoken),
                "the not-built error should name `{spoken}`: {}",
                error.message()
            );
        }
    }

    #[test]
    fn the_bare_document_is_a_2020_12_schema_with_defs_error_and_one_of() {
        let document = combined_schema().expect("unbuilt commands are skipped, not fatal");

        assert_eq!(document.pointer("/$schema"), Some(&Value::from(DIALECT)));
        let _validator = jsonschema::validator_for(&document)
            .unwrap_or_else(|error| panic!("the bare document is not a valid schema: {error}"));
        let defs = document
            .pointer("/$defs")
            .and_then(Value::as_object)
            .expect("$defs is an object");
        assert!(
            defs.contains_key(ERROR),
            "$defs should hold the error schema"
        );
        for target in SchemaTarget::value_variants() {
            assert_eq!(
                defs.contains_key(&name(*target)),
                schema_value(*target).is_ok(),
                "`{}` should appear in $defs exactly when it is built",
                name(*target)
            );
        }
        assert_eq!(
            document
                .pointer("/oneOf")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(defs.len()),
            "oneOf should have one branch per $defs entry"
        );
    }

    #[test]
    fn the_bare_document_accepts_the_error_and_rejects_other_json() {
        let document = combined_schema().expect("the document should build");
        let validator = jsonschema::validator_for(&document).expect("a valid schema");

        let fatal = serde_json::json!({
            "error": true, "kind": "config/missing", "message": "m", "exit_code": 2
        });
        let neither = serde_json::json!({"hello": "world"});

        assert!(validator.is_valid(&fatal), "an error object should match");
        assert!(
            !validator.is_valid(&neither),
            "arbitrary JSON should not match"
        );
    }

    #[test]
    fn check_output_validates_against_the_check_schema_but_not_the_list_schema() {
        let envelope = |command: &str| {
            serde_json::json!({
                "schema_version": 1, "command": command, "ok": true, "root": "/p",
                "data": {
                    "summary": {"passed": 1, "failed": 0, "broken": 0, "new": 0, "obsolete": 0},
                    "links": []
                },
                "findings": [], "truncated": false
            })
        };
        let validator = |target| {
            jsonschema::validator_for(&schema_value(target).expect("the command is built"))
                .expect("a valid schema")
        };

        let output = envelope("check");

        assert!(
            validator(SchemaTarget::Check).is_valid(&output),
            "check output should satisfy schema/check.json"
        );
        assert!(
            !validator(SchemaTarget::List).is_valid(&output),
            "check output must not satisfy schema/list.json"
        );
    }

    #[test]
    fn every_published_schema_is_a_valid_json_schema_with_a_versioned_envelope() {
        for target in SchemaTarget::value_variants() {
            let Ok(schema) = schema_value(*target) else {
                continue;
            };
            let _validator = jsonschema::validator_for(&schema).unwrap_or_else(|error| {
                panic!("`{}` publishes an invalid schema: {error}", name(*target))
            });
            assert!(
                schema.pointer("/properties/schema_version").is_some(),
                "`{}` should describe the envelope's schema_version",
                name(*target)
            );
        }
    }

    #[test]
    fn jsonl_carries_the_schema_as_a_record_before_the_summary() {
        let report = SchemaReport {
            schema: serde_json::json!({"type": "object"}),
        };

        let lines = crate::output::jsonl::lines(&report, true);

        let types: Vec<_> = lines
            .iter()
            .filter_map(|line| line.pointer("/type").and_then(Value::as_str))
            .collect();
        assert_eq!(types, ["schema", "summary"], "lines: {lines:?}");
        assert_eq!(
            lines.first().and_then(|line| line.pointer("/schema")),
            Some(&report.schema)
        );
    }
}
