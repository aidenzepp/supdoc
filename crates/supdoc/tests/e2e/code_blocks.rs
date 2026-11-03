//! Checking the code blocks in docs: the `[features.code-blocks]` gate and the warnings `check`
//! gives for a block's info string.

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
