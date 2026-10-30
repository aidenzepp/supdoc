//! Recording links, detecting failed and broken ones, and accepting changes (docref's init, check and
//! update tests, plus its whole-file tests).

use crate::support::{Project, at, links};

#[test]
fn init_writes_only_the_config_and_the_first_update_records_links_that_then_pass() {
    let project = Project::fixture("basic");

    let _ = project.supdoc(&["init"]).assert_exit(0);
    assert!(
        project.path("supdoc.config.toml").is_file(),
        "init should write supdoc.config.toml"
    );
    assert!(
        !project.path("supdoc.lock").exists(),
        "init should leave the lockfile to the first update (05 C10)"
    );

    project.record();
    assert!(
        project.path("supdoc.lock").is_file(),
        "update --yes should create supdoc.lock"
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn check_fails_a_new_link_until_update_records_it() {
    let project = Project::fixture("basic");

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("link-new"),
        "unrecorded links should be reported as new: {}",
        check.stdout
    );
}

#[test]
fn check_fails_a_link_whose_symbol_changed() {
    let project = Project::fixture("basic");
    project.record();
    project.replace("src/lib.rs", "const A: i32 = 10;", "const A: i32 = 20;");

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("link-failed") && check.stdout.contains("src/lib.rs#A"),
        "the edited constant's link should be reported as failed: {}",
        check.stdout
    );
}

#[test]
fn check_reports_a_link_to_a_removed_symbol_as_broken() {
    let project = Project::fixture("basic");
    project.record();
    project.replace("src/lib.rs", "const A: i32 = 10;\n", "");

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("resolve-not-found") && check.stdout.contains("1 broken"),
        "the link to the removed constant should be broken: {}",
        check.stdout
    );
}

#[test]
fn update_previews_without_yes_then_accepts_the_failed_link_with_yes() {
    let project = Project::fixture("basic");
    project.record();
    project.replace("src/lib.rs", "const A: i32 = 10;", "const A: i32 = 20;");
    let recorded = project.read("supdoc.lock");

    let preview = project.supdoc(&["update"]);
    let _ = preview.assert_exit(0);
    assert!(
        preview.stdout.contains("src/lib.rs#A"),
        "the preview should name the change it would accept: {}",
        preview.stdout
    );
    assert_eq!(
        project.read("supdoc.lock"),
        recorded,
        "a preview should not write supdoc.lock"
    );
    let _ = project.supdoc(&["check"]).assert_exit(1);

    let _ = project.supdoc(&["update", "--yes"]).assert_exit(0);
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn update_of_one_doc_accepts_every_changed_link_written_in_it() {
    let project = Project::fixture("basic");
    project.record();
    project.replace("src/lib.rs", "const A: i32 = 10;", "const A: i32 = 99;");
    project.replace("src/lib.rs", "x + A", "x * A");
    let _ = project.supdoc(&["check"]).assert_exit(1);

    let _ = project
        .supdoc(&["update", "docs/guide.md", "--yes"])
        .assert_exit(0);

    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn reformatting_inside_a_symbol_fails_its_link_and_leaves_the_others_passing() {
    let project = Project::fixture("basic");
    project.record();
    project.replace(
        "src/lib.rs",
        "fn add(x: i32) -> i32 {",
        "fn add( x: i32 ) -> i32 {",
    );
    project.replace("src/lib.rs", "x + A", "x  +  A");

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(
        at(&report, "/data/summary/failed"),
        1,
        "a hash covers the symbol's exact bytes, whitespace included (03 R6): {report}"
    );
    assert_eq!(at(&report, "/data/links/0/symbol"), "add", "{report}");
    assert_eq!(at(&report, "/data/summary/passed"), 4, "{report}");
}

#[test]
fn adding_a_comment_above_a_symbol_keeps_its_link_passing() {
    let project = Project::fixture("basic");
    project.record();
    project.replace(
        "src/lib.rs",
        "const A: i32 = 10;",
        "// base offset\nconst A: i32 = 10;",
    );

    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn whole_file_link_is_recorded_without_a_symbol_and_image_links_are_not_tracked() {
    let project = Project::fixture("wholefile");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&["src/lib.rs", "src/lib.rs#add"]),
        "the whole-file link has no symbol, and the .png link is not tracked"
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn whole_file_link_fails_when_the_file_changes() {
    let project = Project::fixture("wholefile");
    project.record();
    project.replace("src/lib.rs", "x + y", "x * y");

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("`src/lib.rs` changed"),
        "the whole-file link should be reported as failed: {}",
        check.stdout
    );
}

#[test]
fn whole_file_link_fails_on_any_edit_while_a_link_to_an_untouched_symbol_passes() {
    let project = Project::fixture("wholefile");
    project.record();
    project.replace(
        "src/lib.rs",
        "pub const A: u32 = 42;",
        "pub const A: u32 = 43;",
    );

    let check = project.supdoc(&["--json", "check"]);

    let report = check.assert_exit(1).json();
    assert_eq!(
        at(&report, "/data/summary/failed"),
        1,
        "only the whole-file link should fail: {report}"
    );
    assert_eq!(
        at(&report, "/data/summary/passed"),
        1,
        "the link to the untouched `add` should pass: {report}"
    );
    let failed = at(&report, "/data/links/0");
    assert_eq!(at(failed, "/target"), "src/lib.rs", "{report}");
    assert!(
        failed.get("symbol").is_none(),
        "the failed link should be the whole-file one: {failed}"
    );
}

#[test]
fn updating_the_file_with_the_whole_file_link_accepts_it_while_a_symbol_link_elsewhere_stays_failed()
 {
    let project = Project::fixture("wholefile");
    std::fs::write(
        project.path("docs/guide.md"),
        "# Guide\n\nThe [core library](../src/lib.rs) contains the main logic.\n",
    )
    .expect("the guide should be writable");
    std::fs::write(
        project.path("docs/api.md"),
        "# API\n\nThe [`add`](../src/lib.rs#add) function applies the offset.\n",
    )
    .expect("the api doc should be writable");
    project.record();
    project.replace("src/lib.rs", "x + y", "x * y");

    let _ = project
        .supdoc(&["update", "docs/guide.md", "--yes"])
        .assert_exit(0);

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();
    assert_eq!(
        at(&report, "/data/summary/failed"),
        1,
        "only the symbol link should still fail: {report}"
    );
    let failed = at(&report, "/data/links/0");
    assert_eq!(at(failed, "/target"), "src/lib.rs", "{report}");
    assert_eq!(at(failed, "/symbol"), "add", "{report}");
}

#[test]
fn switching_line_endings_to_exact_refuses_until_update_migrates_the_lock() {
    let project = Project::fixture("basic");
    let lib = project.read("src/lib.rs");
    std::fs::write(project.path("src/lib.rs"), lib.replace('\n', "\r\n"))
        .expect("src/lib.rs should be writable");
    project.record();
    let _ = project.supdoc(&["check"]).assert_exit(0);
    std::fs::write(
        project.path("supdoc.config.toml"),
        "[features.hash]\nline-endings = \"exact\"\n",
    )
    .expect("supdoc.config.toml should be writable");

    let refused = project.supdoc(&["check"]);
    let _ = refused.assert_exit(78);
    assert!(
        refused.stdout.contains("lock-settings-mismatch")
            || refused.stderr.contains("lock-settings-mismatch"),
        "check should refuse with lock-settings-mismatch:\nstdout:\n{}\nstderr:\n{}",
        refused.stdout,
        refused.stderr
    );

    let recorded = project.read("supdoc.lock");
    let preview = project.supdoc(&["update"]);
    let _ = preview.assert_exit(0);
    assert!(
        preview.stdout.contains("settings") && preview.stdout.contains("line-endings"),
        "the preview should show the settings row: {}",
        preview.stdout
    );
    assert_eq!(
        project.read("supdoc.lock"),
        recorded,
        "a preview should not write supdoc.lock"
    );

    let _ = project.supdoc(&["update", "--yes"]).assert_exit(0);
    let _ = project.supdoc(&["check"]).assert_exit(0);
    let lock = project.read("supdoc.lock");
    assert!(
        lock.contains("line-endings = \"exact\""),
        "the migrated lock should record line-endings = \"exact\":\n{lock}"
    );
}
