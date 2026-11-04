//! Checking blocks that declare `fragment(<kind>)`, while no tool lists a fragment kind.

use crate::support::{Project, array_at, at};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// A recorded `basic` project with the rust table and one `rust` block written with `info`.
fn project_with_info(info: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```rust {info}\nlet x = 1;\n```\n"),
    );
    project
}

/// The `--json` check of `project`, whatever its exit code.
fn check(project: &Project) -> serde_json::Value {
    project.supdoc(&["check", "--json"]).json()
}

#[test]
fn a_kind_no_tool_lists_leaves_both_steps_not_run_with_one_warning_each() {
    let json = check(&project_with_info("fragment(statements)"));

    assert_eq!(
        finding_kinds(&json),
        ["code-block-fragment-unsupported"; 2],
        "{json}"
    );
    let block = at(&json, "/data/code_blocks/0");
    assert_eq!(block["fragment"], "statements", "{json}");
    assert_eq!(block["format"], "not_run", "{json}");
    assert_eq!(block["lint"], "not_run", "{json}");
    assert_eq!(
        block["reason"],
        "rustfmt can't check a statements fragment; clippy can't check a statements fragment",
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_not_run"),
        1,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{json}"
    );
}

#[test]
fn the_summary_line_counts_the_step_under_its_own_reason() {
    let output = project_with_info("fragment(expression)")
        .supdoc(&["check"])
        .stdout
        .clone();

    assert!(
        output.contains("format not run (rustfmt can't check an expression fragment)"),
        "{output}"
    );
}

#[test]
fn turning_both_steps_off_skips_them_and_the_warning_goes() {
    let json = check(&project_with_info(
        "fragment(statements) format(off: no fragment support) lint(off: no fragment support)",
    ));

    assert!(finding_kinds(&json).is_empty(), "{json}");
    let block = at(&json, "/data/code_blocks/0");
    assert_eq!(block["format"], "skipped", "{json}");
    assert_eq!(block["lint"], "skipped", "{json}");
}

#[test]
fn turning_one_step_off_leaves_the_warning_for_the_other() {
    let json = check(&project_with_info(
        "fragment(members) format(off: no fragment support)",
    ));

    assert_eq!(
        finding_kinds(&json),
        ["code-block-fragment-unsupported"],
        "{json}"
    );
    let block = at(&json, "/data/code_blocks/0");
    assert_eq!(block["format"], "skipped", "{json}");
    assert_eq!(block["lint"], "not_run", "{json}");
}

#[test]
fn an_unknown_empty_bare_or_second_fragment_is_an_unknown_attribute_listing_the_kinds() {
    for info in [
        "fragment(body)",
        "fragment()",
        "fragment",
        "fragment(statements) fragment(expression)",
    ] {
        let json = check(&project_with_info(&format!(
            "{info} format(off: x) lint(off: x)"
        )));

        assert_eq!(
            finding_kinds(&json)
                .iter()
                .filter(|kind| *kind == "code-block-unknown-attribute")
                .count(),
            1,
            "{info}: {json}"
        );
        let help = array_at(&json, "/findings")
            .iter()
            .find_map(|finding| finding["help"].as_str())
            .unwrap_or_default();
        assert!(
            help.contains(
                "`statements`, `expression`, `entries`, `children`, `members`, `declarations`"
            ),
            "{info}: {help}"
        );
    }
}

#[test]
fn an_ignored_block_still_validates_the_kind_but_runs_nothing() {
    let valid = check(&project_with_info(
        "fragment(statements) ignore(needs a fn)",
    ));
    let invalid = check(&project_with_info("fragment(body) ignore(needs a fn)"));

    assert!(finding_kinds(&valid).is_empty(), "{valid}");
    assert_eq!(
        finding_kinds(&invalid),
        ["code-block-unknown-attribute"],
        "{invalid}"
    );
}

#[test]
fn a_block_with_no_fragment_has_no_fragment_in_its_json() {
    let json = check(&project_with_info("ignore(plain)"));

    let block = at(&json, "/data/code_blocks/0");
    assert!(block.get("fragment").is_none(), "{json}");
}
