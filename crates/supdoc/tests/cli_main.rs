//! Runs the built `supdoc` binary and checks what a user sees: streams and exit codes (06, 07).

#![cfg_attr(
    test,
    allow(
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        reason = "tests report failures rather than document callable contracts"
    )
)]

#[cfg(test)]
mod support {
    use std::process::Command;

    /// What the binary printed and how it ended.
    pub(crate) struct Run {
        pub(crate) stdout: String,
        pub(crate) stderr: String,
        pub(crate) code: Option<i32>,
    }

    /// Runs `supdoc` with `args` in an empty environment.
    pub(crate) fn supdoc(args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_supdoc"))
            .args(args)
            .env_clear()
            .output()
            .expect("the supdoc binary should start");
        Run {
            stdout: String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
            code: output.status.code(),
        }
    }

    /// Parses `text` as exactly one JSON object on one line.
    pub(crate) fn single_json_object(text: &str) -> serde_json::Value {
        assert_eq!(
            text.lines().count(),
            1,
            "expected exactly one line: {text:?}"
        );
        let value: serde_json::Value =
            serde_json::from_str(text).expect("the stream should hold one JSON document");
        assert!(value.is_object(), "expected a JSON object: {text:?}");
        value
    }

    /// The value of `name` in a JSON object, or a failure naming the missing field.
    pub(crate) fn field<'a>(object: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        object
            .get(name)
            .unwrap_or_else(|| panic!("the error object should have a `{name}` field: {object}"))
    }
}

use support::{field, single_json_object, supdoc};

/// A config path that cannot exist, so any config-loading command fails before it runs.
const MISSING_CONFIG: &str = "/nonexistent/supdoc.config.toml";

#[test]
fn bare_supdoc_prints_help_to_stdout_and_exits_0() {
    let run = supdoc(&[]);

    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("Usage: supdoc") && run.stdout.contains("Commands:"),
        "stdout should be the help: {}",
        run.stdout
    );
    assert_eq!(run.stderr, "", "help should leave stderr empty");
}

#[test]
fn missing_config_exits_78_with_the_error_on_stderr_only() {
    let run = supdoc(&["check", "--config", MISSING_CONFIG]);

    assert_eq!(run.code, Some(78), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "", "an error should leave stdout empty");
    assert!(
        run.stderr.contains("config-not-found"),
        "stderr should carry the config-not-found code: {}",
        run.stderr
    );
}

#[test]
fn json_error_is_one_object_and_nothing_else() {
    let run = supdoc(&["--json", "check", "--config", MISSING_CONFIG]);

    assert_eq!(run.code, Some(78), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "", "--json errors belong on stderr only");
    let error = single_json_object(&run.stderr);
    assert_eq!(field(&error, "error"), &serde_json::json!(true));
    assert_eq!(field(&error, "kind"), "config-not-found");
    assert_eq!(field(&error, "exit_code"), 78);
}

#[test]
fn unknown_flag_exits_2_with_a_json_usage_error_under_json() {
    let run = supdoc(&["--json", "--no-such-flag", "check"]);

    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "", "usage errors belong on stderr only");
    let error = single_json_object(&run.stderr);
    assert_eq!(field(&error, "kind"), "cli-usage");
    assert_eq!(field(&error, "exit_code"), 2);
}

#[test]
fn only_one_output_format_type_exists_in_the_crate() {
    let mut definitions = Vec::new();
    let mut pending = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("the source directory should be readable") {
            let path = entry.expect("a directory entry should be readable").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text =
                    std::fs::read_to_string(&path).expect("a source file should be readable");
                if text.contains("enum OutputFormat") {
                    definitions.push(path);
                }
            }
        }
    }

    assert_eq!(
        definitions.len(),
        1,
        "OutputFormat should be defined once: {definitions:?}"
    );
}
