//! `supdoc fix` on code blocks: stale excerpts and formatter output.

use std::path::Path;

use crate::support::{Project, array_at};

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

/// A doc with one `sh` block whose body has a doubled space.
const UNFORMATTED_DOC: &str = "# Blocks\n\n- Run it:\n\n  ```sh\n  echo  hi\n  ```\n";

/// A recorded `basic` project with the shell table on, `docs/blocks.md` holding
/// [`UNFORMATTED_DOC`], and an `shfmt` stub running `script` in `outside`, a directory outside
/// the checkout to use as the only `PATH` entry.
fn project_with_shfmt(outside: &Path, script: &str) -> Project {
    use std::os::unix::fs::PermissionsExt as _;
    let project = project_with(UNFORMATTED_DOC);
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.shell]\n",
    );
    let stub = outside.join("shfmt");
    std::fs::write(&stub, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    project
}

#[test]
fn format_fix_yes_writes_the_formatters_output_so_the_next_check_passes() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_with_shfmt(outside.path(), "/usr/bin/sed 's/  */ /g'");
    let path = [("PATH", outside.path())];

    let run = project.supdoc_in_with(".", &["fix", "--yes"], &path);
    let _ = run.assert_exit(0);
    let _check = project
        .supdoc_in_with(".", &["check"], &path)
        .assert_exit(0);

    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n- Run it:\n\n  ```sh\n  echo hi\n  ```\n",
        "the fix should replace only the body, keeping the list item's indent"
    );
    assert_eq!(
        run.stderr, "Fixed 1 code block in 1 file.\n",
        "a formatter-only fix should count blocks and not name `update`"
    );
}

#[test]
fn format_output_that_would_close_the_fence_is_never_written() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_with_shfmt(outside.path(), "printf 'echo hi\\n```\\n'");
    let path = [("PATH", outside.path())];

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);
    let json = project
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(1)
        .json();

    assert_eq!(project.read("docs/blocks.md"), UNFORMATTED_DOC);
    let findings = array_at(&json, "/findings");
    let kinds: Vec<&str> = findings
        .iter()
        .filter_map(|finding| finding["kind"].as_str())
        .collect();
    assert_eq!(
        kinds,
        ["code-block-unformatted", "code-block-fix-unsafe"],
        "{json}"
    );
    assert!(
        findings.iter().all(|finding| finding.get("fix").is_none()),
        "no finding should offer the fix: {json}"
    );
}
