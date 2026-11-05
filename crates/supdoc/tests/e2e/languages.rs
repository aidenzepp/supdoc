//! Links into each language's symbols are recorded, pass, and fail when the symbol changes (docref's
//! TypeScript, Markdown, sub-declaration, JavaScript, Python, Go and Bash tests, plus TOML).

use crate::support::{Project, array_at, at, links};

#[test]
fn typescript_rust_and_markdown_heading_links_are_recorded_and_pass() {
    let project = Project::fixture("basic");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&[
            "docs/overview.md#architecture",
            "src/app.ts#VERSION",
            "src/app.ts#greet",
            "src/lib.rs#A",
            "src/lib.rs#add",
        ]),
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn rust_impl_method_and_markdown_dotted_heading_links_are_recorded_and_pass() {
    let project = Project::fixture("scoped");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&[
            "docs/overview.md#overview.foo.example",
            "src/lib.rs#Config.validate",
        ]),
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn sub_declaration_links_in_rust_and_typescript_are_recorded_and_pass() {
    let project = Project::fixture("subdecl");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&[
            "src/app.ts#App.render",
            "src/app.ts#Direction.Up",
            "src/app.ts#ServerConfig.host",
            "src/app.ts#Status.Active",
            "src/lib.rs#Config.host",
            "src/lib.rs#Handler.handle",
            "src/lib.rs#Message.Quit",
        ]),
        "struct fields, enum variants, trait methods, interface properties, class methods and enum \
         members should each be recorded by their dotted path"
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn changing_a_struct_field_type_fails_its_link() {
    let project = Project::fixture("subdecl");
    project.record();
    project.replace("src/lib.rs", "host: String", "host: Vec<u8>");

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("link-failed") && check.stdout.contains("src/lib.rs#Config.host"),
        "the field's link should fail: {}",
        check.stdout
    );
}

#[test]
fn removing_a_struct_field_breaks_its_link() {
    let project = Project::fixture("subdecl");
    project.record();
    project.replace("src/lib.rs", "    host: String,\n", "");

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("resolve-not-found") && check.stdout.contains("Config.host"),
        "the removed field's link should be broken: {}",
        check.stdout
    );
}

#[test]
fn javascript_links_are_recorded_and_pass() {
    let project = Project::fixture("javascript");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&[
            "src/app.js#App",
            "src/app.js#App.render",
            "src/app.js#VERSION",
            "src/app.js#greet",
        ]),
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn python_links_are_recorded_and_pass() {
    let project = Project::fixture("python");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&[
            "src/app.py#Config",
            "src/app.py#Config.address",
            "src/app.py#Config.host",
            "src/app.py#Config.validate",
            "src/app.py#MAX_RETRIES",
            "src/app.py#process",
        ]),
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn changing_a_python_method_body_fails_its_link() {
    let project = Project::fixture("python");
    project.record();
    project.replace(
        "src/app.py",
        "return len(self.host) > 0",
        "return bool(self.host)",
    );

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    // The method's link fails, and so does its enclosing `Config` struct link.
    assert_eq!(at(&report, "/data/summary/failed"), 2, "{report}");
    assert_eq!(at(&report, "/data/summary/broken"), 0, "{report}");
    let failed: Vec<&str> = array_at(&report, "/data/links")
        .iter()
        .filter(|link| at(link, "/state") == "failed")
        .map(|link| {
            at(link, "/symbol")
                .as_str()
                .expect("a link's symbol should be a string")
        })
        .collect();
    assert_eq!(failed, ["Config", "Config.validate"], "{report}");
}

#[test]
fn go_links_are_recorded_and_pass() {
    let project = Project::fixture("golang");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&[
            "src/main.go#Config",
            "src/main.go#Config.Host",
            "src/main.go#Config.Validate",
            "src/main.go#Handler",
            "src/main.go#Handler.Handle",
            "src/main.go#MaxRetries",
            "src/main.go#globalState",
            "src/main.go#process",
        ]),
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn changing_a_go_function_body_fails_its_link() {
    let project = Project::fixture("golang");
    project.record();
    project.replace("src/main.go", "return data", "return data + data");

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(at(&report, "/data/summary/failed"), 1, "{report}");
    assert_eq!(at(&report, "/data/summary/broken"), 0, "{report}");
    assert_eq!(at(&report, "/data/links/0/symbol"), "process", "{report}");
}

#[test]
fn bash_links_are_recorded_and_pass() {
    let project = Project::fixture("bash");

    project.record();

    assert_eq!(
        project.locked_links("supdoc.lock"),
        links(&[
            "src/deploy.sh#DEPLOY_DIR",
            "src/deploy.sh#MAX_RETRIES",
            "src/deploy.sh#deploy_app",
            "src/deploy.sh#run_tests",
            "src/deploy.sh#setup_env",
        ]),
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn changing_a_bash_function_body_fails_its_link() {
    let project = Project::fixture("bash");
    project.record();
    project.replace("src/deploy.sh", "mkdir -p", "mkdir -pv");

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(at(&report, "/data/summary/failed"), 1, "{report}");
    assert_eq!(at(&report, "/data/summary/broken"), 0, "{report}");
    assert_eq!(at(&report, "/data/links/0/symbol"), "setup_env", "{report}");
}

/// A project whose guide links a TOML key: the bash fixture plus a manifest and a guide.
fn toml_project() -> Project {
    let project = Project::fixture("bash");
    project.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
    );
    project.write(
        "docs/release.md",
        "The [version](../Cargo.toml#package.version) is bumped each release.\n",
    );
    project
}

#[test]
fn toml_key_links_are_recorded_and_pass() {
    let project = toml_project();

    project.record();

    assert!(
        project
            .locked_links("supdoc.lock")
            .contains("Cargo.toml#package.version"),
        "the TOML key's link should be recorded: {:?}",
        project.locked_links("supdoc.lock")
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn toml_changing_a_key_value_fails_its_link() {
    let project = toml_project();
    project.record();
    project.replace("Cargo.toml", "version = \"0.1.0\"", "version = \"0.2.0\"");

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(at(&report, "/data/summary/failed"), 1, "{report}");
    assert_eq!(at(&report, "/data/summary/broken"), 0, "{report}");
    assert_eq!(
        at(&report, "/data/links/0/symbol"),
        "package.version",
        "{report}"
    );
}

#[test]
fn toml_target_over_the_key_budget_is_a_broken_link_update_leaves_unrecorded() {
    // Decided on purpose: a key-path budget failure is a read failure like `read-too-large`, so
    // `update` refuses to record the link and `list` shows it broken with the limit's code.
    let project = toml_project();

    let update = project.supdoc(&["--max-key-segments", "2", "update", "--yes"]);
    let list = project
        .supdoc(&["--max-key-segments", "2", "--json", "list"])
        .assert_exit(0)
        .json();

    let _ = update.assert_exit(0);
    assert!(
        update.stdout.contains("read-too-many-keys")
            && update
                .stdout
                .contains("Update can't record a broken link until it is repaired"),
        "update should name the limit and refuse to record the link: {}",
        update.stdout
    );
    assert!(
        !project.path("supdoc.lock").exists()
            || !project
                .locked_links("supdoc.lock")
                .contains("Cargo.toml#package.version"),
        "the over-budget link should not be recorded"
    );
    assert_eq!(at(&list, "/data/summary/broken"), 1, "{list}");
    let broken: Vec<(Option<&str>, Option<&str>)> = array_at(&list, "/data/links")
        .iter()
        .filter(|link| link.pointer("/state").and_then(serde_json::Value::as_str) == Some("broken"))
        .map(|link| {
            (
                link.pointer("/target").and_then(serde_json::Value::as_str),
                link.pointer("/finding/kind")
                    .and_then(serde_json::Value::as_str),
            )
        })
        .collect();
    assert_eq!(
        broken,
        [(Some("Cargo.toml"), Some("read-too-many-keys"))],
        "{list}"
    );
}

/// A project whose guide links JSON keys: the bash fixture plus a package manifest and a guide.
fn json_project() -> Project {
    let project = Project::fixture("bash");
    project.write(
        "package.json",
        "{\n  \"scripts\": {\n    \"build\": \"tsc -p .\"\n  },\n  \"exports\": {\n    \"./utils\": \"./dist/utils.js\"\n  }\n}\n",
    );
    project.write(
        "docs/build.md",
        "Run [the build](../package.json#scripts.build); import [utils](../package.json#exports.~1/utils).\n",
    );
    project
}

#[test]
fn json_key_links_are_recorded_and_pass() {
    let project = json_project();

    project.record();

    let locked = project.locked_links("supdoc.lock");
    assert!(
        locked.contains("package.json#scripts.build")
            && locked.contains("package.json#exports.~1/utils"),
        "both JSON keys' links should be recorded: {locked:?}"
    );
    let _ = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn json_changing_a_key_value_fails_its_link() {
    let project = json_project();
    project.record();
    project.replace("package.json", "tsc -p .", "tsc -b");

    let report = project.supdoc(&["--json", "check"]).assert_exit(1).json();

    assert_eq!(at(&report, "/data/summary/failed"), 1, "{report}");
    assert_eq!(at(&report, "/data/summary/broken"), 0, "{report}");
    let failed: Vec<&str> = array_at(&report, "/data/links")
        .iter()
        .filter(|link| link.pointer("/state").and_then(serde_json::Value::as_str) == Some("failed"))
        .filter_map(|link| link.pointer("/symbol").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(failed, ["scripts.build"], "{report}");
}
