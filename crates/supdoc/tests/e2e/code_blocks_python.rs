//! Checking Python code blocks.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use crate::support::{Project, array_at, at};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// A recorded `basic` project with the python table (and `config` after it) and one `py` block
/// holding `body` in `docs/blocks.md`.
fn project_with_python_block(config: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.python]\n{config}"),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```py\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `name` running `script` in `dir`.
fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

#[test]
fn ruff_runs_with_no_cache_and_the_blocks_virtual_path_beside_the_doc() {
    let project = project_with_python_block("", "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "ruff",
        "[ \"$1\" = check ] && exit 0\n\
         [ \"$1 $2 $3 $5\" = 'format --no-cache --stdin-filename -' ] || exit 3\n\
         case \"$4\" in */docs/blocks.md.1.py) /bin/cat;; *) exit 4;; esac",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn black_when_selected_runs_quietly_and_names_itself_in_the_reason() {
    let project = project_with_python_block("formatter = \"black\"\nlinter = false\n", "x=1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "black",
        "[ \"$1 $2 $4\" = '--quiet --stdin-filename -' ] || exit 3\necho 'x = 1'",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`black` would format it differently",
        "{json}"
    );
}

#[test]
fn the_real_ruff_passes_a_formatted_block_and_fails_another() {
    let Some(dir) = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .find(|dir| dir.is_absolute() && dir.join("ruff").is_file())
    else {
        // Written to the process's stderr directly, which `cargo test` shows for a passing test.
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED the_real_ruff_passes_a_formatted_block_and_fails_another: ruff is not on PATH\n",
        );
        return;
    };
    let formatted = project_with_python_block("", "x = 1");
    let unformatted = project_with_python_block("", "x=1");

    let passes = formatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = unformatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
}

/// A `ruff` stub that formats by echoing its input and lints by running `check`, a script that
/// sees ruff's `check` arguments as `$@`.
fn ruff_stub(dir: &Path, check: &str) {
    stub(
        dir,
        "ruff",
        &format!("if [ \"$1\" = format ]; then /bin/cat; exit 0; fi\nshift\n{check}"),
    );
}

#[test]
fn lint_diagnostics_from_ruff_are_lint_errors_at_their_doc_line_and_column() {
    let project = project_with_python_block("", "x = 1\nif x == None:\n    pass");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    ruff_stub(
        outside.path(),
        "[ \"$1 $2 $3 $5 $6 $7\" = '--no-cache --output-format=json --stdin-filename --ignore F821,F401,F841 -' ] || exit 3\n\
         case \"$4\" in */docs/blocks.md.1.py) ;; *) exit 4;; esac\n\
         echo '[{\"code\":\"E711\",\"message\":\"Comparison to `None`\",\"location\":{\"row\":2,\"column\":6}}]'\n\
         exit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert_eq!(at(&json, "/findings/0/location/column"), 6, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn lint_exit_other_than_diagnostics_is_a_tool_failure() {
    let project = project_with_python_block("", "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    ruff_stub(outside.path(), "echo 'ruff failed: bad config' >&2\nexit 2");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
}

#[test]
fn lint_does_not_run_on_a_block_its_formatter_cannot_parse() {
    let project = project_with_python_block("", "x = (");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "ruff",
        "if [ \"$1\" = check ]; then echo '[{\"code\":\"E999\",\"message\":\"m\",\"location\":{\"row\":1,\"column\":1}}]'; exit 1; fi\n\
         echo \"error: Failed to parse $4:1:5: Expected an expression\" >&2\nexit 2",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
}

#[test]
fn lint_with_the_formatter_off_still_runs() {
    let project = project_with_python_block("formatter = false\n", "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    ruff_stub(outside.path(), "exit 0");

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "off", "{json}");
}

#[test]
fn lint_with_the_real_ruff_reports_a_rule_the_project_selects() {
    let Some(dir) = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .find(|dir| dir.is_absolute() && dir.join("ruff").is_file())
    else {
        // Written to the process's stderr directly, which `cargo test` shows for a passing test.
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED lint_with_the_real_ruff_reports_a_rule_the_project_selects: ruff is not on PATH\n",
        );
        return;
    };
    let config = "[lint]\nselect = [\"E711\", \"F401\"]\n";
    let clean = project_with_python_block("", "x = 1");
    let flagged = project_with_python_block("", "import os\n\nx = 1\nif x == None:\n    pass");
    clean.write("ruff.toml", config);
    flagged.write("ruff.toml", config);

    let passes = clean
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = flagged
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(at(&passes, "/data/code_blocks/0/lint"), "ran", "{passes}");
    assert_eq!(finding_kinds(&fails), ["code-block-lint"], "{fails}");
    assert_eq!(at(&fails, "/findings/0/location/line"), 7, "{fails}");
}
