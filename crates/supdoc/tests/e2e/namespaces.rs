//! Namespaced links, config `include`/`exclude` and `extends`, and the namespace commands (docref's
//! namespace and config tests).

use crate::support::{Project, array_at, links};

#[test]
fn namespaced_and_plain_links_are_recorded_in_their_written_form_and_pass() {
    let project = Project::fixture("namespaced");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&["auth:src/lib.rs#validate", "src/lib.rs#add"]),
        "the namespaced link keeps its `auth:` form"
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn changing_a_namespaced_target_fails_its_link() {
    let project = Project::fixture("namespaced");
    project.record();
    project.replace(
        "services/auth/src/lib.rs",
        "!input.is_empty()",
        "input.len() > 3",
    );

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("auth:src/lib.rs#validate"),
        "the namespaced link should fail: {}",
        check.stdout
    );
}

#[test]
fn config_exclude_keeps_links_in_excluded_docs_out_of_the_lockfile() {
    let project = Project::fixture("configured");

    project.record();

    let lock = project.read("supdoc.lock");
    assert!(
        lock.contains("docs/guide.md") && !lock.contains("ignored.md"),
        "only docs/guide.md's link should be recorded:\n{lock}"
    );
}

#[test]
fn namespace_list_shows_each_name_and_its_folder() {
    let project = Project::fixture("namespaced");

    let run = project.supdoc(&["namespace", "list"]);

    let _ = run.assert_exit(0);
    assert!(
        run.stdout.contains("auth") && run.stdout.contains("services/auth"),
        "the list should map auth to services/auth: {}",
        run.stdout
    );
}

#[test]
fn config_show_marks_a_missing_namespace_folder_as_not_existing() {
    let project = Project::fixture("namespaced");
    std::fs::write(
        project.path("supdoc.config.toml"),
        "[namespaces]\nauth = \"services/auth\"\nghost = \"services/ghost\"\n",
    )
    .expect("the config should be writable");

    let report = project
        .supdoc(&["--json", "config", "show"])
        .assert_exit(0)
        .json();

    let exists = |name: &str| -> serde_json::Value {
        array_at(&report, "/data/namespaces")
            .iter()
            .find(|namespace| {
                namespace
                    .pointer("/name")
                    .and_then(serde_json::Value::as_str)
                    == Some(name)
            })
            .and_then(|namespace| namespace.pointer("/exists").cloned())
            .unwrap_or_else(|| panic!("`config show` should list namespace {name}: {report}"))
    };
    assert_eq!(
        exists("auth"),
        true,
        "services/auth is in the fixture (CS4)"
    );
    assert_eq!(exists("ghost"), false, "services/ghost is not (CS4)");
}

#[test]
fn namespace_rename_rewrites_config_lockfile_and_links_and_check_still_passes() {
    let project = Project::fixture("namespaced");
    project.record();

    let _ = project
        .supdoc(&["namespace", "rename", "auth", "authn", "--yes"])
        .assert_exit(0);

    let config = project.read("supdoc.config.toml");
    assert!(
        config.contains("authn = \"services/auth\"") && !config.contains("auth ="),
        "the config key should be renamed:\n{config}"
    );
    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&["authn:src/lib.rs#validate", "src/lib.rs#add"]),
        "the lock entry should move to the new name"
    );
    let guide = project.read("docs/guide.md");
    assert!(
        guide.contains("](authn:src/lib.rs#validate)") && !guide.contains("](auth:"),
        "the link should use the new name:\n{guide}"
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}
