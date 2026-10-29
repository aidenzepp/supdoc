//! `supdoc schema`: prints the JSON Schema of a command's `--json` output (06 X10, 07 G10).
//!
//! The schema is the one the command itself publishes through its `schema()` function, so the
//! JSON a command writes and the schema printed here come from the same types. The schema never
//! loads the config (06 X14).

use std::io::{self, Write};

use clap::ValueEnum as _;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::output::{Output, OutputError, Report, TextContext};

use super::args::{Inspect, SchemaArgs, SchemaTarget};
use super::{
    CliError, Context, Outcome, check, config, fix, init, list, rename, resolve, skill, update,
};

/// The schema document as output data.
#[derive(Debug, Serialize, JsonSchema)]
struct SchemaReport {
    /// The JSON Schema, or an object of schemas keyed by command name when no command is named.
    schema: Value,
}

impl Report for SchemaReport {
    fn command(&self) -> &'static str {
        "schema"
    }

    fn render_text(&self, _context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(pretty(&self.schema).as_bytes())
    }
}

/// Runs `supdoc schema`, printing the schema document through `output`.
///
/// With a `COMMAND` the document is that command's schema. Without one it is an object keyed by
/// command name holding the schema of every command this build has built.
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
        None => every_built_schema()?,
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
    let schema = schema_of(target)?;
    serde_json::to_value(schema).map_err(|error| {
        CliError::new(
            "cli/schema-serialize",
            format!(
                "could not turn the schema of `{}` into JSON: {error}",
                name(target)
            ),
            "this is a supdoc bug; report it with the command you ran",
            CliError::BUG_EXIT_CODE,
        )
    })
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

/// Every built command's schema keyed by name. A command that is not built has no JSON output to
/// describe yet, so it is left out rather than failing the rest.
///
/// # Errors
///
/// Returns `cli/schema-serialize` if a schema cannot become JSON.
fn every_built_schema() -> Result<Value, CliError> {
    let mut all = Map::new();
    for target in SchemaTarget::value_variants() {
        match schema_value(*target) {
            Ok(schema) => {
                // Names are unique per target, so nothing is replaced.
                let _unreplaced = all.insert(name(*target), schema);
            }
            Err(error) if error.code() == CliError::NOT_BUILT => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Value::Object(all))
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

    #[test]
    fn shipped_schema_files_match_what_each_built_command_publishes() {
        let mut checked = 0;
        for target in SchemaTarget::value_variants() {
            let Ok(schema) = schema_value(*target) else {
                continue;
            };
            let file = schema_dir().join(format!("{}.json", name(*target)));
            let shipped = std::fs::read_to_string(&file).unwrap_or_else(|error| {
                panic!(
                    "{} should exist for a built command: {error}",
                    file.display()
                )
            });
            assert_eq!(
                shipped,
                pretty(&schema),
                "{} is stale; regenerate it with `supdoc schema {}`",
                file.display(),
                name(*target)
            );
            checked += 1;
        }
        assert!(checked > 0, "at least one command should be built");
    }

    #[test]
    fn every_schema_file_belongs_to_a_built_command() {
        let entries = std::fs::read_dir(schema_dir()).expect("schema/ should exist");
        for entry in entries {
            let path = entry.expect("schema/ should be readable").path();
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("schema file names should be UTF-8");
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
        let error = schema_value(SchemaTarget::Update).expect_err("update is not built");

        assert_eq!(error.code(), CliError::NOT_BUILT);
        assert!(
            error.message().contains("supdoc update"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn the_all_commands_document_has_one_entry_per_built_command() {
        let all = every_built_schema().expect("unbuilt commands are skipped, not fatal");
        let object = all.as_object().expect("the document is an object");

        for target in SchemaTarget::value_variants() {
            assert_eq!(
                object.contains_key(&name(*target)),
                schema_value(*target).is_ok(),
                "`{}` should appear exactly when it is built",
                name(*target)
            );
        }
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
}
