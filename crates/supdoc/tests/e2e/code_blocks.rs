//! Checking the code blocks in docs: the `[features.code-blocks]` gate and the warnings `check`
//! gives for a block's info string.

use std::os::unix::fs::symlink;

use crate::support::{Project, array_at};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// The doc every test writes: a list item holding a block whose attribute is unknown.
const UNKNOWN_ATTRIBUTE_DOC: &str = "# Blocks\n\n- Run it:\n\n  ```sh hidden\n  echo hi\n  ```\n";

/// A recorded copy of the `basic` fixture with [`UNKNOWN_ATTRIBUTE_DOC`] at `docs/blocks.md`.
fn project_with_unknown_attribute() -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("docs/blocks.md", UNKNOWN_ATTRIBUTE_DOC);
    project
}

#[test]
fn without_the_table_a_doc_with_blocks_leaves_the_check_report_unchanged() {
    let project = Project::fixture("basic");
    project.record();
    let before = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    project.write("docs/blocks.md", UNKNOWN_ATTRIBUTE_DOC);
    let after = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert_eq!(
        before, after,
        "with the feature off, no block should be judged"
    );
}

#[test]
fn an_unknown_attribute_warns_and_fails_only_under_deny_warnings() {
    let project = project_with_unknown_attribute();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");

    let warned = project.supdoc(&["check", "--json"]).assert_exit(0).json();
    let _denied = project.supdoc(&["check", "--deny-warnings"]).assert_exit(1);

    assert_eq!(
        finding_kinds(&warned),
        ["code-block-unknown-attribute"],
        "{warned}"
    );
}

#[test]
fn known_attributes_and_a_bare_language_draw_no_warning() {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        "```rust no_run edition2021\nfn main() {}\n```\n\n~~~sh ignore(needs a server)\necho hi\n~~~\n\n```\nplain\n```\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

/// A recorded `basic` project with the feature on and one `sh` block carrying `attributes`.
fn project_with_block(attributes: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```sh {attributes}\necho hi\n```\n"),
    );
    project
}

#[test]
fn ignore_without_a_reason_warns_and_fails_only_under_deny_warnings() {
    let project = project_with_block("ignore");

    let warned = project.supdoc(&["check", "--json"]).assert_exit(0).json();
    let _denied = project.supdoc(&["check", "--deny-warnings"]).assert_exit(1);

    assert_eq!(
        finding_kinds(&warned),
        ["code-block-ignore-reason"],
        "{warned}"
    );
}

#[test]
fn ignore_with_blank_parentheses_warns_and_with_a_reason_does_not() {
    let blank = project_with_block("no_lint(  )");
    let reasoned = project_with_block("no_lint(slow, needs a server)");

    let blank_json = blank.supdoc(&["check", "--json"]).assert_exit(0).json();
    let reasoned_json = reasoned.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert_eq!(
        finding_kinds(&blank_json),
        ["code-block-ignore-reason"],
        "{blank_json}"
    );
    assert!(finding_kinds(&reasoned_json).is_empty(), "{reasoned_json}");
}

/// A recorded `basic` project with the feature on and a `rust` block in `docs/blocks.md`
/// carrying `attributes`, with `config` appended to the config.
fn project_with_rust_block(attributes: &str, config: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n{config}"),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```rust {attributes}\nfn add() {{}}\n```\n"),
    );
    project
}

/// How many `code-block-source-missing` findings a `--json` check printed.
fn source_missing_count(json: &serde_json::Value) -> usize {
    finding_kinds(json)
        .iter()
        .filter(|kind| *kind == "code-block-source-missing")
        .count()
}

#[test]
fn from_and_uses_that_resolve_draw_no_finding() {
    let project = project_with_rust_block(
        "from(../src/lib.rs#add) uses(/src/lib.rs#add) from(core:lib.rs)",
        "\n[namespaces]\ncore = \"src\"\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

#[test]
fn from_a_missing_symbol_or_file_fails_once_per_value() {
    let project = project_with_rust_block(
        "from(../src/lib.rs#addd) uses(/src/gone.rs#add) from(../src/lib.rs#add)",
        "",
    );

    let run = project.supdoc(&["check", "--json"]);
    let json = run.assert_exit(1).json();

    assert_eq!(source_missing_count(&json), 2, "{json}");
}

#[test]
fn from_a_climb_an_unknown_namespace_or_a_url_fails() {
    let project = project_with_rust_block(
        "from(../../outside.rs#add) uses(nope:lib.rs#add) from(https://example.com/a.rs)",
        "",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(1).json();

    assert_eq!(source_missing_count(&json), 3, "{json}");
}

#[test]
fn from_or_uses_through_a_symlink_out_of_the_root_never_shows_the_outside_file() {
    let marker = "OUTSIDE_MARKER_7f3a";
    let outside = tempfile::tempdir().expect("an outside directory should be creatable");
    std::fs::write(
        outside.path().join("secret.rs"),
        format!("pub fn secret() {{ let _ = \"{marker}\"; }}\n"),
    )
    .expect("the outside file should be writable");
    let project = project_with_rust_block(
        "from(/src/secret.rs#secret) uses(/src/secret.rs#secret)",
        "",
    );
    symlink(
        outside.path().join("secret.rs"),
        project.path("src/secret.rs"),
    )
    .expect("the symlink should be creatable");

    let json_run = project.supdoc(&["check", "--json"]);
    let text_run = project.supdoc(&["check"]);

    let json = json_run.assert_exit(1).json();
    let _ = text_run.assert_exit(1);
    assert_eq!(source_missing_count(&json), 2, "{json}");
    for run in [&json_run, &text_run] {
        assert!(
            !run.stdout.contains(marker) && !run.stderr.contains(marker),
            "`supdoc {}` showed the outside file\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}
