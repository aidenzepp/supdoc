//! `--json` reports from `check` and `list` (docref's `--format json` tests).

use crate::support::{Project, array_at, at};

#[test]
fn check_json_counts_every_passed_link_and_lists_them_only_with_verbose() {
    let project = Project::fixture("basic");
    project.record();

    let quiet = project.supdoc(&["--json", "check"]).assert_exit(0).json();
    let verbose = project
        .supdoc(&["--json", "check", "-v"])
        .assert_exit(0)
        .json();

    assert_eq!(at(&quiet, "/ok"), true, "{quiet}");
    let summary = at(&quiet, "/data/summary");
    assert_eq!(
        summary,
        &serde_json::json!({"passed": 5, "failed": 0, "broken": 0, "new": 0, "obsolete": 0}),
    );
    assert!(
        array_at(&quiet, "/data/links").is_empty(),
        "passed links are counted but not listed by default: {quiet}"
    );
    assert_eq!(
        array_at(&verbose, "/data/links").len(),
        5,
        "-v should list the passed links: {verbose}"
    );
}

#[test]
fn check_json_lists_a_failed_link_with_its_finding() {
    let project = Project::fixture("basic");
    project.record();
    project.replace("src/lib.rs", "const A: i32 = 10;", "const A: i32 = 20;");

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(at(&report, "/ok"), false, "{report}");
    assert_eq!(at(&report, "/data/summary/failed"), 1, "{report}");
    let link = at(&report, "/data/links/0");
    assert_eq!(at(link, "/state"), "failed", "{report}");
    assert_eq!(at(link, "/symbol"), "A", "{report}");
    assert_eq!(at(link, "/finding/kind"), "link/failed", "{report}");
}

#[test]
fn check_json_gives_a_broken_link_the_reason_it_broke() {
    let project = Project::fixture("basic");
    project.record();
    project.replace("src/lib.rs", "const A: i32 = 10;\n", "");

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(at(&report, "/data/summary/broken"), 1, "{report}");
    let link = at(&report, "/data/links/0");
    assert_eq!(at(link, "/state"), "broken", "{report}");
    assert_eq!(at(link, "/finding/kind"), "resolve/not-found", "{report}");
    assert!(
        at(link, "/finding/message")
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "the finding should explain the break: {report}"
    );
}

#[test]
fn list_json_gives_every_link_its_state_source_and_target() {
    let project = Project::fixture("basic");
    project.record();

    let report = project.supdoc(&["--json", "list"]).assert_exit(0).json();

    let links = array_at(&report, "/data/links");
    assert_eq!(links.len(), 5, "{report}");
    for link in links {
        assert_eq!(at(link, "/state"), "passed", "{link}");
        for field in ["/source", "/target"] {
            assert!(
                at(link, field)
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "`{field}` should name a file: {link}"
            );
        }
    }
}
