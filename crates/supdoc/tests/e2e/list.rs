//! `supdoc list` shows every link and its state, or only the links to one target (docref's `status`
//! and `refs` tests).

use std::collections::BTreeSet;

use crate::support::{Project, array_at, at, links};

/// The links `supdoc --json list [args]` shows, as `state source -> target[#symbol]`.
fn listed(project: &Project, args: &[&str]) -> BTreeSet<String> {
    let command: Vec<&str> = ["--json", "list"].iter().chain(args).copied().collect();
    let report = project.supdoc(&command).assert_exit(0).json();
    array_at(&report, "/data/links")
        .iter()
        .map(|link| {
            let text = |pointer: &str| {
                at(link, pointer)
                    .as_str()
                    .unwrap_or_else(|| panic!("`{pointer}` should be a string: {link}"))
                    .to_owned()
            };
            let target = link
                .get("symbol")
                .and_then(serde_json::Value::as_str)
                .map_or_else(
                    || text("/target"),
                    |symbol| format!("{}#{symbol}", text("/target")),
                );
            format!("{} {} -> {target}", text("/state"), text("/source"))
        })
        .collect()
}

#[test]
fn list_shows_every_link_with_its_state_including_passed() {
    let project = Project::fixture("basic");
    project.record();

    assert_eq!(
        listed(&project, &[]),
        links(&[
            "passed docs/api.md -> src/app.ts#VERSION",
            "passed docs/api.md -> src/app.ts#greet",
            "passed docs/guide.md -> docs/overview.md#architecture",
            "passed docs/guide.md -> src/lib.rs#A",
            "passed docs/guide.md -> src/lib.rs#add",
        ]),
    );
}

#[test]
fn list_text_shows_a_whole_file_link_without_a_symbol() {
    let project = Project::fixture("wholefile");
    project.record();

    let run = project.supdoc(&["list"]);

    let _ = run.assert_exit(0);
    let lines: Vec<&str> = run.stdout.lines().collect();
    assert!(
        lines.iter().any(|line| line.ends_with(" src/lib.rs")),
        "one line should end with the bare file: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.ends_with(" src/lib.rs#add")),
        "one line should end with the symbol link: {lines:?}"
    );
}

#[test]
fn list_with_a_file_target_shows_the_links_to_that_file() {
    let project = Project::fixture("basic");
    project.record();

    assert_eq!(
        listed(&project, &["src/lib.rs"]),
        links(&[
            "passed docs/guide.md -> src/lib.rs#A",
            "passed docs/guide.md -> src/lib.rs#add",
        ]),
    );
}

#[test]
fn list_with_a_symbol_target_shows_only_links_to_that_symbol() {
    let project = Project::fixture("basic");
    project.record();

    assert_eq!(
        listed(&project, &["src/lib.rs#add"]),
        links(&["passed docs/guide.md -> src/lib.rs#add"]),
    );
}

#[test]
fn list_with_a_namespaced_target_shows_the_links_through_that_namespace() {
    let project = Project::fixture("namespaced");
    project.record();

    assert_eq!(
        listed(&project, &["auth:src/lib.rs"]),
        links(&["passed docs/guide.md -> auth:src/lib.rs#validate"]),
    );
}

#[test]
fn list_with_an_unlinked_target_prints_nothing_and_says_so_on_stderr() {
    let project = Project::fixture("basic");
    project.record();

    let run = project.supdoc(&["list", "nonexistent.rs"]);

    let _ = run.assert_exit(0);
    assert_eq!(run.stdout, "", "no links means no data on stdout");
    assert!(
        run.stderr.contains("No links to nonexistent.rs"),
        "stderr should say nothing matched: {}",
        run.stderr
    );
}
