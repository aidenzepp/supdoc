//! Binary contracts with no other owner: the `fix` object in `check --json` (r-tests T4) and
//! the `SUPDOC_YES` / `SUPDOC_DRY_RUN` variables deciding whether `update` writes (r-tests T8).

use super::support::Project;

/// A project whose only doc has multibyte text before a link to a misspelled symbol.
fn misspelled_in_multibyte_doc() -> Project {
    let project = Project::empty();
    project.write("supdoc.config.toml", "");
    project.write(
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    project.write("docs/guide.md", "# Gü\n\nSée [ad](../src/lib.rs#ad) ok.\n");
    project
}

#[test]
fn check_json_fix_uses_byte_offsets_and_a_root_relative_file_from_a_subdirectory() {
    let project = misspelled_in_multibyte_doc();
    let output = project
        .command(&["--json", "check"], &[])
        .current_dir(project.path().join("docs"))
        .output()
        .expect("the supdoc binary should start");
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let document: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json should print one JSON document");

    let fix = document.pointer("/data/links/0/finding/fix");
    assert_eq!(
        fix,
        Some(&serde_json::json!({
            "file": "docs/guide.md",
            "start": 31,
            "end": 33,
            "replacement": "add",
        })),
        "the fix should name the root-relative file and byte (not character) offsets of `ad`; stdout was: {stdout}"
    );
}

#[test]
fn supdoc_yes_lets_update_write_the_lock_and_supdoc_dry_run_stops_it() {
    let project = misspelled_in_multibyte_doc();
    project.write(
        "docs/guide.md",
        "# Guide\n\nThe [add](../src/lib.rs#add) function sums.\n",
    );

    let dry = project.run_with(&["update"], &[("SUPDOC_YES", "1"), ("SUPDOC_DRY_RUN", "1")]);
    assert!(
        !project.path().join("supdoc.lock").exists(),
        "SUPDOC_DRY_RUN=1 should stop `update` from writing supdoc.lock even with SUPDOC_YES=1: {}",
        dry.transcript()
    );

    assert_eq!(
        dry.code,
        Some(0),
        "SUPDOC_DRY_RUN=1 `update` is a preview and should exit 0: {}",
        dry.transcript()
    );
    assert!(
        dry.stderr.contains(
            "Drop --dry-run (or unset SUPDOC_DRY_RUN) and add --yes to apply, as in `supdoc update --yes`."
        ),
        "the preview should say to unset SUPDOC_DRY_RUN and add --yes: {}",
        dry.transcript()
    );

    let before = project.files();
    let yes = project.run_with(&["update"], &[("SUPDOC_YES", "1")]);
    assert_eq!(
        yes.code,
        Some(0),
        "SUPDOC_YES=1 `update` should succeed: {}",
        yes.transcript()
    );
    assert!(
        project.path().join("supdoc.lock").exists(),
        "SUPDOC_YES=1 should let `update` write supdoc.lock: {}",
        yes.transcript()
    );
    assert!(
        project.files() != before,
        "the SUPDOC_YES=1 run should have changed the project files"
    );
}
