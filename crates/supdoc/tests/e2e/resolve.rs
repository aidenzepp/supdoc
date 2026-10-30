//! `supdoc resolve` lists a file's symbols and confirms one (docref's resolve tests).

use std::collections::BTreeSet;

use crate::support::{Project, Run, array_at, at, links};

/// The symbol paths `supdoc --json resolve <file>` lists, run from `project`'s root.
fn listed_symbols(project: &Project, file: &str) -> BTreeSet<String> {
    let report = project
        .supdoc(&["--json", "resolve", file])
        .assert_exit(0)
        .json();
    array_at(&report, "/data/symbols")
        .iter()
        .map(|symbol| {
            at(symbol, "/path")
                .as_str()
                .unwrap_or_else(|| panic!("a symbol's path should be a string: {symbol}"))
                .to_owned()
        })
        .collect()
}

/// Fails unless `listed` has every symbol in `expected`, naming the missing ones.
fn assert_lists(listed: &BTreeSet<String>, expected: &[&str], file: &str) {
    let missing: Vec<_> = links(expected).difference(listed).cloned().collect();
    assert!(
        missing.is_empty(),
        "resolve {file} should list {missing:?}; it listed {listed:?}"
    );
}

#[test]
fn resolve_lists_the_symbols_of_a_rust_file() {
    let project = Project::fixture("basic");

    assert_lists(
        &listed_symbols(&project, "src/lib.rs"),
        &["A", "add"],
        "src/lib.rs",
    );
}

#[test]
fn resolve_with_a_symbol_prints_its_full_link_path() {
    let project = Project::fixture("basic");

    let run = project.supdoc(&["resolve", "src/lib.rs", "add"]);

    let _ = run.assert_exit(0);
    assert_eq!(run.stdout.trim_end(), "src/lib.rs#add");
}

#[test]
fn resolve_lists_markdown_headings_with_their_github_anchors() {
    let project = Project::fixture("basic");

    let report = project
        .supdoc(&["--json", "resolve", "docs/overview.md"])
        .assert_exit(0)
        .json();

    let symbols = array_at(&report, "/data/symbols");
    for (path, anchor) in [
        ("overview.architecture", "architecture"),
        ("overview.configuration", "configuration"),
    ] {
        let symbol = symbols
            .iter()
            .find(|symbol| symbol.get("path").and_then(serde_json::Value::as_str) == Some(path))
            .unwrap_or_else(|| panic!("resolve should list heading `{path}`: {report}"));
        assert_eq!(
            at(symbol, "/aliases"),
            &serde_json::json!([anchor]),
            "`{path}` should also answer to its GitHub anchor"
        );
    }
}

#[test]
fn resolve_names_every_full_path_when_a_bare_name_is_ambiguous() {
    let project = Project::fixture("scoped");

    let run = project.supdoc(&["resolve", "src/lib.rs", "validate"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("resolve-ambiguous")
            && run.stderr.contains("Config.validate")
            && run.stderr.contains("Server.validate"),
        "the error should offer both qualified paths: {}",
        run.stderr
    );
}

#[test]
fn resolve_reaches_a_duplicate_heading_through_its_numbered_anchor() {
    let project = Project::fixture("scoped");

    let first = project.supdoc(&["resolve", "docs/overview.md", "example"]);
    let second = project.supdoc(&["resolve", "docs/overview.md", "example-1"]);

    let _ = first.assert_exit(0);
    let _ = second.assert_exit(0);
    assert_eq!(
        first.stdout.trim_end(),
        "docs/overview.md#overview.foo.example",
        "the first `### Example` owns the plain anchor"
    );
    assert_eq!(
        second.stdout.trim_end(),
        "docs/overview.md#overview.bar.example",
        "the second `### Example` gets `-1`"
    );
}

#[test]
fn resolve_lists_rust_fields_variants_and_trait_methods() {
    let project = Project::fixture("subdecl");

    assert_lists(
        &listed_symbols(&project, "src/lib.rs"),
        &["Config.host", "Message.Quit", "Handler.handle"],
        "src/lib.rs",
    );
}

#[test]
fn resolve_lists_javascript_symbols() {
    let project = Project::fixture("javascript");

    assert_lists(
        &listed_symbols(&project, "src/app.js"),
        &["VERSION", "greet", "App", "App.render"],
        "src/app.js",
    );
}

#[test]
fn resolve_lists_python_symbols() {
    let project = Project::fixture("python");

    assert_lists(
        &listed_symbols(&project, "src/app.py"),
        &["MAX_RETRIES", "process", "Config.host", "Config.validate"],
        "src/app.py",
    );
}

#[test]
fn resolve_lists_go_symbols() {
    let project = Project::fixture("golang");

    assert_lists(
        &listed_symbols(&project, "src/main.go"),
        &[
            "MaxRetries",
            "Config.Host",
            "Config.Validate",
            "Handler.Handle",
        ],
        "src/main.go",
    );
}

#[test]
fn resolve_lists_bash_symbols() {
    let project = Project::fixture("bash");

    assert_lists(
        &listed_symbols(&project, "src/deploy.sh"),
        &[
            "MAX_RETRIES",
            "DEPLOY_DIR",
            "setup_env",
            "deploy_app",
            "run_tests",
        ],
        "src/deploy.sh",
    );
}

#[test]
fn resolve_non_utf8_file_exits_1_with_read_not_utf8() {
    let project = Project::fixture("basic");
    std::fs::write(project.path("src/binary.rs"), [0xff_u8, 0xfe, 0x00])
        .expect("the binary file should be writable");

    let run = project.supdoc(&["--json", "resolve", "src/binary.rs"]);

    let _ = run.assert_exit(1);
    assert_eq!(
        at(
            &serde_json::from_str(&run.stderr).unwrap_or_default(),
            "/kind"
        ),
        "read-not-utf8",
        "a file that is not UTF-8 should be refused (R6); stderr: {}",
        run.stderr
    );
}

#[test]
fn resolve_file_over_16_mib_exits_1_with_read_too_large() {
    let project = Project::fixture("basic");
    let big = std::fs::File::create(project.path("src/big.rs"))
        .expect("the big file should be creatable");
    big.set_len(16 * 1024 * 1024 + 1)
        .expect("the big file should be extendable");

    let run = project.supdoc(&["--json", "resolve", "src/big.rs"]);

    let _ = run.assert_exit(1);
    assert_eq!(
        at(
            &serde_json::from_str(&run.stderr).unwrap_or_default(),
            "/kind"
        ),
        "read-too-large",
        "a file over 16 MiB should be refused (R6); stderr: {}",
        run.stderr
    );
}

#[test]
fn resolve_file_outside_the_root_is_refused_without_listing_it() {
    let project = Project::fixture("basic");
    let outside = tempfile::tempdir().expect("a second temporary directory should be creatable");
    let file = outside.path().join("elsewhere.rs");
    std::fs::write(&file, "pub fn zed() {}\n").expect("the outside file should be writable");
    let file = file
        .to_str()
        .expect("the temp directory path should be UTF-8");

    for run in both_formats(&project, ".", &[file]) {
        assert_refused_as_outside(&run, "zed");
    }
}

/// The `monorepo` fixture with `services/web` as a project whose config is `config`, and the
/// issue #54 files in `services/outside`, next to the project and so outside its root.
fn project_beside_outside_files(config: &str) -> Project {
    let project = Project::fixture("monorepo");
    std::fs::write(project.path("services/web/supdoc.config.toml"), config)
        .expect("the config should be writable");
    std::fs::create_dir(project.path("services/outside"))
        .expect("the outside directory should be creatable");
    for (name, text) in [
        ("s.rs", "fn secret_api_key_xyz() {}\n"),
        ("notes.md", "# private heading about salary\n"),
        ("o.rs", "fn ok_one() {}\n"),
    ] {
        std::fs::write(project.path(&format!("services/outside/{name}")), text)
            .expect("the outside file should be writable");
    }
    project
}

/// `supdoc resolve <args>` run from `dir` as text and as `--json`.
fn both_formats(project: &Project, dir: &str, args: &[&str]) -> [Run; 2] {
    let text = [&["resolve"][..], args].concat();
    let json = [&["--json", "resolve"][..], args].concat();
    [project.supdoc_in(dir, &text), project.supdoc_in(dir, &json)]
}

/// Fails unless `run` was refused with `cli-path-outside-project` (exit 2) and neither stream
/// shows `leak`, a symbol or heading of the outside file.
fn assert_refused_as_outside(run: &Run, leak: &str) {
    let _ = run.assert_exit(2);
    assert!(
        run.stderr.contains("cli-path-outside-project"),
        "`supdoc {}` should be refused as outside the project; stderr:\n{}",
        run.args,
        run.stderr
    );
    assert!(
        !run.stdout.contains(leak) && !run.stderr.contains(leak),
        "`supdoc {}` showed `{leak}` from a file outside the project\nstdout:\n{}\nstderr:\n{}",
        run.args,
        run.stdout,
        run.stderr
    );
}

#[cfg(unix)]
#[test]
fn resolve_through_a_symlink_out_of_the_root_lists_no_outside_symbol() {
    let project = project_beside_outside_files("");
    std::os::unix::fs::symlink("../../outside/s.rs", project.path("services/web/src/x.rs"))
        .expect("the symlink should be creatable");

    for run in both_formats(&project, "services/web", &["src/x.rs"]) {
        assert_refused_as_outside(&run, "secret_api_key_xyz");
    }
}

#[cfg(unix)]
#[test]
fn resolve_through_a_markdown_symlink_out_of_the_root_lists_no_outside_heading() {
    let project = project_beside_outside_files("");
    std::os::unix::fs::symlink("../outside/notes.md", project.path("services/web/n.md"))
        .expect("the symlink should be creatable");

    for run in both_formats(&project, "services/web", &["n.md"]) {
        assert_refused_as_outside(&run, "salary");
    }
}

#[cfg(unix)]
#[test]
fn resolve_of_a_missing_symbol_through_a_symlink_out_of_the_root_suggests_no_outside_symbol() {
    let project = project_beside_outside_files("");
    std::os::unix::fs::symlink("../../outside/o.rs", project.path("services/web/src/b.rs"))
        .expect("the symlink should be creatable");

    for run in both_formats(&project, "services/web", &["src/b.rs", "SECRET_TOKEN"]) {
        assert_refused_as_outside(&run, "ok_one");
    }
}

#[test]
fn resolve_of_a_typed_path_climbing_out_of_the_root_is_refused() {
    let project = project_beside_outside_files("");

    for run in both_formats(&project, "services/web", &["../outside/s.rs"]) {
        assert_refused_as_outside(&run, "secret_api_key_xyz");
    }
}

#[test]
fn resolve_of_a_namespaced_path_climbing_out_of_the_root_is_refused() {
    let project = project_beside_outside_files("[namespaces]\nns = \"src\"\n");

    for run in both_formats(&project, "services/web", &["ns:../../outside/s.rs"]) {
        assert_refused_as_outside(&run, "secret_api_key_xyz");
    }
}

#[cfg(unix)]
#[test]
fn resolve_through_a_symlink_to_a_file_inside_the_root_still_lists_its_symbols() {
    let project = Project::fixture("basic");
    std::os::unix::fs::symlink("lib.rs", project.path("src/alias.rs"))
        .expect("the symlink should be creatable");

    assert_lists(
        &listed_symbols(&project, "src/alias.rs"),
        &["A", "add"],
        "src/alias.rs",
    );
}
