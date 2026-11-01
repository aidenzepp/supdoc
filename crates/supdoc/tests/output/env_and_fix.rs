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

#[test]
fn check_json_fix_offsets_produce_the_same_file_as_fix_yes() {
    let project = misspelled_in_multibyte_doc();
    let output = project
        .command(&["--json", "check"], &[])
        .output()
        .expect("the supdoc binary should start");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("--json should print one JSON document");
    let fix = document
        .pointer("/data/links/0/finding/fix")
        .expect("the misspelled link should carry a fix object");
    let file = fix["file"].as_str().expect("fix.file should be a string");
    let start = usize::try_from(fix["start"].as_u64().expect("fix.start should be a number"))
        .expect("fix.start should fit usize");
    let end = usize::try_from(fix["end"].as_u64().expect("fix.end should be a number"))
        .expect("fix.end should fit usize");
    let replacement = fix["replacement"]
        .as_str()
        .expect("fix.replacement should be a string");

    let original = std::fs::read(project.path().join(file)).expect("the doc should be readable");
    let mut applied = original
        .get(..start)
        .expect("fix.start should lie inside the file")
        .to_vec();
    applied.extend_from_slice(replacement.as_bytes());
    applied.extend_from_slice(
        original
            .get(end..)
            .expect("fix.end should lie inside the file"),
    );

    let run = project.run(&["fix", "--yes"]);
    let fixed = std::fs::read(project.path().join(file)).expect("the doc should be readable");
    assert_eq!(
        String::from_utf8_lossy(&applied),
        String::from_utf8_lossy(&fixed),
        "applying the JSON fix offsets should give the file `fix --yes` writes: {}",
        run.transcript()
    );
}
