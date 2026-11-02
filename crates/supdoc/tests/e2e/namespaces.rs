//! Namespaced links, config `include`/`exclude` and `extends`, and the namespace commands (docref's
//! namespace and config tests).

use crate::support::{Project, array_at, at, links};

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

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(at(&report, "/data/summary/failed"), 1, "{report}");
    assert_eq!(at(&report, "/data/summary/broken"), 0, "{report}");
    assert_eq!(at(&report, "/data/links/0/symbol"), "validate", "{report}");
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

#[test]
fn namespace_rename_prints_the_walk_findings_without_changing_its_other_lines() {
    let project = Project::fixture("namespaced");
    project.record();
    let args = ["namespace", "rename", "auth", "authn"];
    let plain = project.supdoc(&args);
    let _ = plain.assert_exit(0);
    std::fs::write(project.path("rules"), "gui*.md\n")
        .expect("the outside rules should be writable");
    std::os::unix::fs::symlink(project.path("rules"), project.path(".gitignore"))
        .expect("the symlink should be creatable");

    let preview = project.supdoc(&args);
    let applied = project.supdoc(&["namespace", "rename", "auth", "authn", "--yes"]);

    let _ = preview.assert_exit(0);
    let _ = applied.assert_exit(0);
    for run in [&preview, &applied] {
        assert!(
            run.stdout.starts_with(&plain.stdout),
            "the config, link and lock lines should not change\nwithout:\n{}\nwith:\n{}",
            plain.stdout,
            run.stdout
        );
        assert_eq!(
            run.stdout.matches("scan-ignore-file-symlink").count(),
            1,
            "`supdoc {}` should print the symlink warning once\n{}",
            run.args,
            run.stdout
        );
    }
}

#[test]
fn namespace_rename_rewrites_untracked_links_by_their_written_prefix() {
    let project = Project::fixture("namespaced");
    project.write(
        "docs/untracked.md",
        "[bare](auth:) [page](auth:x.html#s) [logo](auth:logo.png)\n",
    );
    project.record();
    let _ = project.supdoc(&["check"]).assert_exit(0);

    let _ = project
        .supdoc(&["namespace", "rename", "auth", "identity", "--yes"])
        .assert_exit(0);

    assert_eq!(
        project.read("docs/untracked.md"),
        "[bare](identity:) [page](identity:x.html#s) [logo](identity:logo.png)\n",
        "every link written with the `auth:` prefix should move to `identity:`"
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn namespace_rename_refuses_an_oversized_ignore_file_before_writing_anything() {
    let project = Project::fixture("namespaced");
    project.write("docs/hidden.md", "[v](auth:src/lib.rs#validate)\n");
    project.write(
        ".gitignore",
        &format!("docs/hidden.md\n{}\n", "#".repeat(300_000)),
    );
    let files = [
        "supdoc.config.toml",
        "docs/guide.md",
        "docs/hidden.md",
        ".gitignore",
    ];
    let before: Vec<String> = files.iter().map(|file| project.read(file)).collect();
    let lock_before = std::fs::read(project.path("supdoc.lock")).ok();

    let run = project.supdoc(&["namespace", "rename", "auth", "authn", "--yes"]);

    let _ = run.assert_exit(78);
    assert!(
        format!("{}{}", run.stdout, run.stderr).contains("ignore-too-large"),
        "the refusal should name `ignore-too-large`\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    let after: Vec<String> = files.iter().map(|file| project.read(file)).collect();
    assert_eq!(after, before, "no file should change");
    assert_eq!(
        std::fs::read(project.path("supdoc.lock")).ok(),
        lock_before,
        "the lock should not change"
    );
}
