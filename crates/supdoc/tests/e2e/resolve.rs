//! `supdoc resolve` lists a file's symbols and confirms one (docref's resolve tests).

use std::collections::BTreeSet;

use crate::support::{Project, array_at, at, links};

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
fn resolve_file_outside_the_root_is_allowed_and_json_uses_dotdot() {
    let project = Project::fixture("basic");
    let outside = tempfile::tempdir().expect("a second temporary directory should be creatable");
    let file = outside.path().join("elsewhere.rs");
    std::fs::write(&file, "pub fn zed() {}\n").expect("the outside file should be writable");
    let file = file
        .to_str()
        .expect("the temp directory path should be UTF-8");

    let report = project
        .supdoc(&["--json", "resolve", file])
        .assert_exit(0)
        .json();

    let shown = at(&report, "/data/file")
        .as_str()
        .unwrap_or_else(|| panic!("`/data/file` should be a string: {report}"));
    assert!(
        shown.starts_with("../") && shown.ends_with("/elsewhere.rs"),
        "a file outside the root should be named with `..` (R16), got {shown:?}"
    );
    assert_lists(
        &array_at(&report, "/data/symbols")
            .iter()
            .filter_map(|symbol| symbol.pointer("/path")?.as_str().map(str::to_owned))
            .collect(),
        &["zed"],
        shown,
    );
}
