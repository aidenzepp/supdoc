//! Checking Markdown code blocks.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use crate::support::{Project, array_at, at, real_tool_dir};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// A recorded `basic` project with the markdown table (and `config` after it) and `docs/blocks.md`
/// holding `blocks`.
fn project_with_blocks(config: &str, blocks: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.markdown]\n{config}"),
    );
    project.write("docs/blocks.md", &format!("# Blocks\n\n{blocks}\n"));
    project
}

/// A recorded project with one `markdown` block holding `body`.
fn project_with_block(config: &str, body: &str) -> Project {
    project_with_blocks(config, &format!("```markdown\n{body}\n```"))
}

/// Writes an executable `#!/bin/sh` stub named `name` running `script` in `dir`.
fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A `rumdl` stub that formats by echoing its input and lints by running `check`, a script that
/// sees rumdl's `check` arguments as `$@`.
fn rumdl_stub(dir: &Path, check: &str) {
    stub(
        dir,
        "rumdl",
        &format!("if [ \"$1\" = fmt ]; then /bin/cat; exit 0; fi\nshift\n{check}"),
    );
}

#[test]
fn a_nested_fence_inside_a_four_backtick_block_is_one_block() {
    let project = project_with_blocks(
        "",
        "````markdown\n# Example\n\n```rust\nfn main() {}\n```\n````",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(outside.path(), "echo '[]'");

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(array_at(&json, "/data/code_blocks").len(), 1, "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/language"),
        "markdown",
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_block_goes_to_rumdl_on_stdin_under_its_virtual_path_with_no_config() {
    let project = project_with_block("", "text");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(
        outside.path(),
        "case \"$*\" in\n\
         \"--no-cache --output-format=json --extend-disable=MD041 --no-config --stdin --stdin-filename \"*/docs/blocks.md.1.md) echo '[]'; exit 0;;\n\
         esac\nexit 3",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn an_unformatted_block_is_reported_with_rumdl_named() {
    let project = project_with_block(
        "[features.code-blocks.markdown.rumdl]\nlint = \"off\"\n",
        "* a\n\n\n* b",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "rumdl",
        "case \"$*\" in\n\
         \"fmt --no-cache --extend-disable=MD041 --no-config --stdin-filename \"*\"/docs/blocks.md.1.md -\") /bin/cat > /dev/null; printf '* a\\n\\n* b\\n';;\n\
         *) exit 3;;\nesac",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`rumdl-format` would format it differently",
        "{json}"
    );
}

#[test]
fn lint_diagnostics_from_rumdl_are_lint_errors_at_their_doc_line_and_column() {
    let project = project_with_block("", "# T\n\n<div>x</div>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(
        outside.path(),
        "echo '[{\"file\":\"x\",\"line\":3,\"column\":1,\"rule\":\"MD033\",\"message\":\"Inline HTML found: <div>\"}]'\nexit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
}

#[test]
fn the_nearest_project_config_is_passed_explicitly() {
    let project = project_with_block("", "text");
    project.write("docs/rumdl.toml", "[global]\nline-length = 20\n");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(
        outside.path(),
        "case \"$*\" in\n\
         *\" --config=\"*/docs/rumdl.toml\" --stdin \"*) echo '[]'; exit 0;;\n\
         esac\nexit 3",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn a_config_with_code_block_tools_makes_the_block_not_checked() {
    let project = project_with_block("", "text");
    project.write(
        ".rumdl.toml",
        "[code-block-tools]\nenabled = true\n\n[code-block-tools.tools.x]\ncommand = [\"sh\"]\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(outside.path(), "rumdl", "exit 9");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    assert!(
        at(&json, "/data/code_blocks/0/reason")
            .as_str()
            .is_some_and(|reason| reason.contains(".rumdl.toml")),
        "{json}"
    );
}

#[test]
fn the_real_rumdl_passes_a_clean_block_and_fails_a_dirty_one() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_passes_a_clean_block_and_fails_a_dirty_one",
    ) else {
        return;
    };
    let clean = project_with_block("", "## Sub\n\ntext");
    let dirty = project_with_block("", "## Sub\n\n\n\ntext   ");

    let passes = clean
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = dirty
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert!(
        finding_kinds(&fails).contains(&"code-block-unformatted".to_owned()),
        "{fails}"
    );
}
