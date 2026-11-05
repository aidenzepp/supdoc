//! Checking blocks that declare `fragment(<kind>)`: kinds a tool doesn't list, and the kinds the
//! static cartridges check natively.

#![cfg(test)]

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

/// A recorded `basic` project with the rust table and one `rust` block written with `info`.
fn project_with_info(info: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```rust {info}\nlet x = 1;\n```\n"),
    );
    project
}

/// The `--json` check of `project`, whatever its exit code.
fn check(project: &Project) -> serde_json::Value {
    project.supdoc(&["check", "--json"]).json()
}

#[test]
fn a_kind_no_tool_lists_leaves_both_steps_not_run_with_one_warning_each() {
    let json = check(&project_with_info("fragment(statements)"));

    assert_eq!(
        finding_kinds(&json),
        ["code-block-fragment-unsupported"; 2],
        "{json}"
    );
    let block = at(&json, "/data/code_blocks/0");
    assert_eq!(block["fragment"], "statements", "{json}");
    assert_eq!(block["format"], "not_run", "{json}");
    assert_eq!(block["lint"], "not_run", "{json}");
    assert_eq!(
        block["reason"],
        "rustfmt can't check a statements fragment; clippy can't check a statements fragment",
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_not_run"),
        1,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{json}"
    );
}

#[test]
fn the_summary_line_counts_the_step_under_its_own_reason() {
    let output = project_with_info("fragment(expression)")
        .supdoc(&["check"])
        .stdout
        .clone();

    assert!(
        output.contains("format not run (rustfmt can't check an expression fragment)"),
        "{output}"
    );
}

#[test]
fn turning_both_steps_off_skips_them_and_the_warning_goes() {
    let json = check(&project_with_info(
        "fragment(statements) format(off: no fragment support) lint(off: no fragment support)",
    ));

    assert!(finding_kinds(&json).is_empty(), "{json}");
    let block = at(&json, "/data/code_blocks/0");
    assert_eq!(block["format"], "skipped", "{json}");
    assert_eq!(block["lint"], "skipped", "{json}");
}

#[test]
fn turning_one_step_off_leaves_the_warning_for_the_other() {
    let json = check(&project_with_info(
        "fragment(members) format(off: no fragment support)",
    ));

    assert_eq!(
        finding_kinds(&json),
        ["code-block-fragment-unsupported"],
        "{json}"
    );
    let block = at(&json, "/data/code_blocks/0");
    assert_eq!(block["format"], "skipped", "{json}");
    assert_eq!(block["lint"], "not_run", "{json}");
}

#[test]
fn an_unknown_empty_bare_or_second_fragment_is_an_unknown_attribute_listing_the_kinds() {
    for info in [
        "fragment(body)",
        "fragment()",
        "fragment",
        "fragment(statements) fragment(expression)",
    ] {
        let json = check(&project_with_info(&format!(
            "{info} format(off: x) lint(off: x)"
        )));

        assert_eq!(
            finding_kinds(&json)
                .iter()
                .filter(|kind| *kind == "code-block-unknown-attribute")
                .count(),
            1,
            "{info}: {json}"
        );
        let help = array_at(&json, "/findings")
            .iter()
            .find_map(|finding| finding["help"].as_str())
            .unwrap_or_default();
        assert!(
            help.contains(
                "`statements`, `expression`, `entries`, `children`, `members`, `declarations`"
            ),
            "{info}: {help}"
        );
    }
}

#[test]
fn an_ignored_block_still_validates_the_kind_but_runs_nothing() {
    let valid = check(&project_with_info(
        "fragment(statements) ignore(needs a fn)",
    ));
    let invalid = check(&project_with_info("fragment(body) ignore(needs a fn)"));

    assert!(finding_kinds(&valid).is_empty(), "{valid}");
    assert_eq!(
        finding_kinds(&invalid),
        ["code-block-unknown-attribute"],
        "{invalid}"
    );
}

#[test]
fn a_block_with_no_fragment_has_no_fragment_in_its_json() {
    let json = check(&project_with_info("ignore(plain)"));

    let block = at(&json, "/data/code_blocks/0");
    assert!(block.get("fragment").is_none(), "{json}");
}

/// A recorded `basic` project whose code-blocks config holds `config`, with one `tag` block
/// written with `info` and holding `body` in `docs/blocks.md`.
fn project_with_block(config: &str, tag: &str, info: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n{config}"),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag} {info}\n{body}```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `name` running `script` in `dir`.
fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// The `--json` check of `project` with `dir` as the only `PATH` entry, asserting it exits `exit`.
fn check_with_path(project: &Project, dir: &Path, exit: i32) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", dir)])
        .assert_exit(exit)
        .json()
}

/// Asserts that `json`'s only block passed, with no finding and no step left not run.
fn assert_passed_with_every_step_run(json: &serde_json::Value) {
    assert!(finding_kinds(json).is_empty(), "{json}");
    for (pointer, count) in [("passed", 1), ("format_not_run", 0), ("lint_not_run", 0)] {
        assert_eq!(
            at(json, &format!("/data/summary/code_blocks/{pointer}")),
            count,
            "{pointer}: {json}"
        );
    }
}

/// The Rust table with clippy off, so only rustfmt sees a block.
const RUST_FORMAT_ONLY: &str =
    "[features.code-blocks.rust]\n[features.code-blocks.rust.clippy]\nlint = \"off\"\n";

/// The shell table, shfmt and shellcheck both on.
const SHELL: &str = "[features.code-blocks.shell]\n";

/// The Python table, ruff format and ruff check both on.
const PYTHON: &str = "[features.code-blocks.python]\n";

/// The Python table with black selected as its formatter, and ruff check.
const PYTHON_BLACK: &str = "[features.code-blocks.python]\nformatter = \"black\"\n";

/// A method without its `impl`, formatted.
const RUST_METHOD: &str = "pub fn resolve(&self) -> u32 {\n    self.value\n}\n";

/// A function body with a top-level `return`, formatted.
const PYTHON_BODY: &str = "x = compute(1)\nreturn x\n";

#[test]
fn native_rustfmt_checks_a_members_fragment() {
    let project = project_with_block(RUST_FORMAT_ONLY, "rust", "fragment(members)", RUST_METHOD);
    let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(dir.path(), "rustfmt", "exec /bin/cat");

    let json = check_with_path(&project, dir.path(), 0);

    assert_passed_with_every_step_run(&json);
}

#[test]
fn native_rustfmt_still_cant_check_statements_or_an_expression() {
    for kind in ["statements", "expression"] {
        let project = project_with_block(
            RUST_FORMAT_ONLY,
            "rust",
            &format!("fragment({kind})"),
            "let x = 1;\n",
        );
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub(dir.path(), "rustfmt", "exit 9");

        let json = check_with_path(&project, dir.path(), 0);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-fragment-unsupported"],
            "{kind}: {json}"
        );
        assert_eq!(
            at(&json, "/data/code_blocks/0/format"),
            "not_run",
            "{kind}: {json}"
        );
    }
}

#[test]
fn native_shfmt_and_shellcheck_check_statements_and_an_expression() {
    for kind in ["statements", "expression"] {
        let project = project_with_block(
            SHELL,
            "sh",
            &format!("fragment({kind})"),
            "echo \"$HOME\"\n",
        );
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub(dir.path(), "shfmt", "exec /bin/cat");
        stub(
            dir.path(),
            "shellcheck",
            "/bin/cat > /dev/null; echo '{\"comments\":[]}'",
        );

        assert_passed_with_every_step_run(&check_with_path(&project, dir.path(), 0));
    }
}

#[test]
fn native_ruff_and_black_check_statements_an_expression_and_members() {
    for config in [PYTHON, PYTHON_BLACK] {
        for kind in ["statements", "expression", "members"] {
            let project = project_with_block(config, "py", &format!("fragment({kind})"), "x = 1\n");
            let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
            stub(
                dir.path(),
                "ruff",
                "[ \"$1\" = check ] && { /bin/cat > /dev/null; echo '[]'; exit 0; }\nexec /bin/cat",
            );
            stub(dir.path(), "black", "exec /bin/cat");

            let json = check_with_path(&project, dir.path(), 0);

            assert_passed_with_every_step_run(&json);
        }
    }
}

#[test]
fn native_python_entries_stay_unsupported() {
    let project = project_with_block(PYTHON, "py", "fragment(entries)", "\"a\": 1,\n");
    let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(dir.path(), "ruff", "exit 9");

    let json = check_with_path(&project, dir.path(), 0);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-fragment-unsupported"; 2],
        "{json}"
    );
}

#[test]
fn native_ruff_leaves_off_the_function_body_rules_only_for_statements() {
    for (info, ignored) in [
        ("fragment(statements)", "statements"),
        ("fragment(expression)", "plain"),
        ("", "plain"),
    ] {
        let project = project_with_block(PYTHON, "py", info, PYTHON_BODY);
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let record = dir.path().join("ignored");
        stub(
            dir.path(),
            "ruff",
            &format!(
                "[ \"$1\" = check ] || exec /bin/cat\n/bin/cat > /dev/null\n\
                 case \"$*\" in *' --ignore F821,F401,F841,F706,F704,PLE1142 '*) echo statements;; \
                 *' --ignore F821,F401,F841 '*) echo plain;; *) echo \"$*\";; esac > '{}'\n\
                 echo '[]'",
                record.display()
            ),
        );

        let json = check_with_path(&project, dir.path(), 0);

        assert!(finding_kinds(&json).is_empty(), "{info:?}: {json}");
        let written = std::fs::read_to_string(&record)
            .expect("the ruff stub should record which rules it was told to ignore");
        assert_eq!(written.trim(), ignored, "{info:?}");
    }
}

#[test]
fn native_real_rustfmt_formats_a_method_without_its_impl() {
    let Some(dir) = real_tool_dir(
        "rustfmt",
        "native_real_rustfmt_formats_a_method_without_its_impl",
    ) else {
        return;
    };
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".rustup")));
    let cargo = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".cargo")));
    let mut vars: Vec<(&str, &Path)> = vec![("PATH", &dir)];
    vars.extend(rustup.as_deref().map(|path| ("RUSTUP_HOME", path)));
    vars.extend(cargo.as_deref().map(|path| ("CARGO_HOME", path)));
    let formatted = project_with_block(RUST_FORMAT_ONLY, "rust", "fragment(members)", RUST_METHOD);
    let unformatted = project_with_block(
        RUST_FORMAT_ONLY,
        "rust",
        "fragment(members)",
        "pub fn resolve(&self)->u32{ self.value }\n",
    );

    let passes = formatted
        .supdoc_in_with(".", &["check", "--json"], &vars)
        .assert_exit(0)
        .json();
    let fails = unformatted
        .supdoc_in_with(".", &["check", "--json"], &vars)
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
}

#[test]
fn native_real_shfmt_and_shellcheck_pass_a_statements_fragment() {
    let test = "native_real_shfmt_and_shellcheck_pass_a_statements_fragment";
    let (Some(shfmt), Some(shellcheck)) = (
        real_tool_dir("shfmt", test),
        real_tool_dir("shellcheck", test),
    ) else {
        return;
    };
    let path = std::env::join_paths([shfmt, shellcheck]).expect("tool directories should join");
    let project = project_with_block(
        SHELL,
        "sh",
        "fragment(statements)",
        "if [ -f x ]; then\n\techo hi\nfi\n",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", Path::new(&path))])
        .assert_exit(0)
        .json();

    assert_passed_with_every_step_run(&json);
}

#[test]
fn native_real_ruff_passes_a_top_level_return_only_as_statements() {
    let test = "native_real_ruff_passes_a_top_level_return_only_as_statements";
    let Some(dir) = real_tool_dir("ruff", test) else {
        return;
    };
    let statements = project_with_block(PYTHON, "py", "fragment(statements)", PYTHON_BODY);
    let whole_file = project_with_block(PYTHON, "py", "", PYTHON_BODY);

    let passes = check_with_path(&statements, &dir, 0);
    let fails = check_with_path(&whole_file, &dir, 1);

    assert_passed_with_every_step_run(&passes);
    assert_eq!(finding_kinds(&fails), ["code-block-lint"], "{fails}");
}

#[test]
fn native_real_black_formats_a_top_level_return_as_statements() {
    let test = "native_real_black_formats_a_top_level_return_as_statements";
    let (Some(black), Some(ruff)) = (real_tool_dir("black", test), real_tool_dir("ruff", test))
    else {
        return;
    };
    let path = std::env::join_paths([black, ruff]).expect("tool directories should join");
    let project = project_with_block(PYTHON_BLACK, "py", "fragment(statements)", PYTHON_BODY);

    let json = check_with_path(&project, Path::new(&path), 0);

    assert_passed_with_every_step_run(&json);
}
