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
        "[ \"$1 $2 $3 $5\" = 'format --no-cache --stdin-filename -' ] || exit 3\n\
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
    let project = project_with_python_block("formatter = \"black\"\n", "x=1");
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
