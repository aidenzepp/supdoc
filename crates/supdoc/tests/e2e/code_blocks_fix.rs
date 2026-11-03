//! `supdoc fix` on code blocks: stale excerpts and formatter output.

use crate::support::Project;

/// The text of `add` in the `basic` fixture's `src/lib.rs`, as a `from(...)` block holds it.
const ADD: &str = "fn add(x: i32) -> i32 {\n    x + A\n}";

/// A doc whose `from(../src/lib.rs#add)` block no longer matches its source.
const STALE_DOC: &str = "# Blocks\n\n```rust from(../src/lib.rs#add)\nfn add() {}\n```\n";

/// A recorded `basic` project with the feature on and `docs/blocks.md` holding `doc`.
fn project_with(doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write("docs/blocks.md", doc);
    project
}

#[test]
fn a_preview_of_a_stale_block_writes_nothing() {
    let project = project_with(STALE_DOC);

    let run = project.supdoc(&["fix"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        project.read("docs/blocks.md"),
        STALE_DOC,
        "a preview should leave the doc as it was"
    );
    assert!(
        run.stderr.contains("1 fix in 1 file. Nothing written."),
        "the preview should count the block's fix:\n{}",
        run.stderr
    );
}

#[test]
fn fix_yes_refreshes_a_stale_block_so_the_next_check_passes() {
    let project = project_with(STALE_DOC);

    let run = project.supdoc(&["fix", "--yes"]);
    let _ = run.assert_exit(0);
    let _check = project.supdoc(&["check"]).assert_exit(0);

    assert_eq!(
        project.read("docs/blocks.md"),
        format!("# Blocks\n\n```rust from(../src/lib.rs#add)\n{ADD}\n```\n"),
    );
    assert_eq!(
        run.stderr, "Fixed 1 code block in 1 file.\n",
        "a block-only fix should count blocks and not name `update`"
    );
}

#[test]
fn a_stale_block_and_a_fixable_broken_link_in_one_doc_are_both_fixed() {
    let project = project_with(&format!("See [add](../src/lib.rs#ad).\n\n{STALE_DOC}"));

    let run = project.supdoc(&["fix", "--yes"]);
    let _ = run.assert_exit(0);

    assert_eq!(
        project.read("docs/blocks.md"),
        format!(
            "See [add](../src/lib.rs#add).\n\n# Blocks\n\n```rust from(../src/lib.rs#add)\n{ADD}\n```\n"
        ),
    );
    assert!(
        run.stderr
            .starts_with("Fixed 1 link and 1 code block in 1 file. Run `supdoc update"),
        "the note should count both and send the link to `update`:\n{}",
        run.stderr
    );
}
