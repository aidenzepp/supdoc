//! Links into each language's symbols are recorded, pass, and fail when the symbol changes (docref's
//! TypeScript, Markdown, sub-declaration, JavaScript, Python, Go and Bash tests).

use crate::support::{Project, links};

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

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("src/app.py#Config.validate"),
        "the method's link should fail: {}",
        check.stdout
    );
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

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("src/main.go#process"),
        "the function's link should fail: {}",
        check.stdout
    );
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

    let check = project.supdoc(&["check"]);

    let _ = check.assert_exit(1);
    assert!(
        check.stdout.contains("src/deploy.sh#setup_env"),
        "the function's link should fail: {}",
        check.stdout
    );
}
