//! Checking blocks that declare `fragment(<kind>)`: kinds a tool doesn't list, the kinds the
//! static cartridges check natively, and the kinds rustfmt reads inside a frame.

#![cfg(test)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

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
    let json = check(&project_with_info("fragment(entries)"));

    assert_eq!(
        finding_kinds(&json),
        ["code-block-fragment-unsupported"; 2],
        "{json}"
    );
    let block = at(&json, "/data/code_blocks/0");
    assert_eq!(block["fragment"], "entries", "{json}");
    assert_eq!(block["format"], "not_run", "{json}");
    assert_eq!(block["lint"], "not_run", "{json}");
    assert_eq!(
        block["reason"],
        "rustfmt can't check an entries fragment; clippy can't check an entries fragment",
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
    let output = project_with_info("fragment(entries)")
        .supdoc(&["check"])
        .stdout
        .clone();

    assert!(
        output.contains("format not run (rustfmt can't check an entries fragment)"),
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
fn native_rustfmt_cant_check_entries_children_or_declarations() {
    for kind in ["entries", "children", "declarations"] {
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
    let Some(env) = real_rustfmt("native_real_rustfmt_formats_a_method_without_its_impl") else {
        return;
    };
    let vars = borrowed(&env);
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

/// The variables a run of the real rustfmt needs, for the test named `test`: `PATH` holding only
/// rustfmt's directory, and rustup's and cargo's homes, which rustup's proxy reads. `None` when
/// rustfmt is not on `PATH` (see [`real_tool_dir`]).
fn real_rustfmt(test: &str) -> Option<Vec<(&'static str, PathBuf)>> {
    let dir = real_tool_dir("rustfmt", test)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".rustup")));
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".cargo")));
    let mut env = vec![("PATH", dir)];
    env.extend(rustup.map(|path| ("RUSTUP_HOME", path)));
    env.extend(cargo.map(|path| ("CARGO_HOME", path)));
    Some(env)
}

/// `env` as [`Project::supdoc_in_with`] takes it.
fn borrowed<'env>(env: &'env [(&'static str, PathBuf)]) -> Vec<(&'static str, &'env Path)> {
    env.iter()
        .map(|(name, path)| (*name, path.as_path()))
        .collect()
}

/// The frame rustfmt reads `statements` and `expression` blocks in, and the name only it holds.
const RUSTFMT_FRAME: [&str; 2] = ["fn supdoc_fragment() {", "supdoc_fragment"];

/// A doc with prose around one Rust block declaring `fragment(<kind>)` and holding `body`.
fn framed_doc(kind: &str, body: &str) -> String {
    format!("# Blocks\n\nBefore `x`.\n\n```rust fragment({kind})\n{body}```\n\nAfter `y`.\n")
}

/// A recorded `basic` project with rustfmt alone on, and `doc` at `docs/blocks.md`.
fn rustfmt_project(doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n{RUST_FORMAT_ONLY}"),
    );
    project.write("docs/blocks.md", doc);
    project
}

/// Asserts that no finding of `json` shows a line or name of rustfmt's frame in its message,
/// help, location or fix.
fn assert_no_frame_text(json: &serde_json::Value) {
    for finding in array_at(json, "/findings") {
        for field in ["message", "help", "location", "fix"] {
            let text = finding[field].to_string();
            for frame in RUSTFMT_FRAME {
                assert!(!text.contains(frame), "{field} shows {frame:?}: {finding}");
            }
        }
    }
}

/// Checks `formatted` and `unformatted`, each the body of a `fragment(<kind>)` block, with the
/// real rustfmt in `env`: the formatted one passes, the unformatted one fails
/// `code-block-unformatted` with a fix, and `fix --yes` turns its doc into the formatted one's,
/// byte for byte. No finding shows the frame.
fn assert_rustfmt_frames(
    env: &[(&'static str, PathBuf)],
    kind: &str,
    formatted: &str,
    unformatted: &str,
) {
    let vars = borrowed(env);
    let passing = rustfmt_project(&framed_doc(kind, formatted));
    let failing = rustfmt_project(&framed_doc(kind, unformatted));

    let passes = passing
        .supdoc_in_with(".", &["check", "--json"], &vars)
        .assert_exit(0)
        .json();
    let fails = failing
        .supdoc_in_with(".", &["check", "--json"], &vars)
        .assert_exit(1)
        .json();
    let _fix = failing
        .supdoc_in_with(".", &["fix", "--yes"], &vars)
        .assert_exit(0);

    assert_passed_with_every_step_run(&passes);
    assert_eq!(
        finding_kinds(&fails),
        ["code-block-unformatted"],
        "{kind}: {fails}"
    );
    assert!(
        at(&fails, "/findings/0/fix/replacement").is_string(),
        "{kind}: {fails}"
    );
    assert_no_frame_text(&fails);
    assert_eq!(
        failing.read("docs/blocks.md"),
        framed_doc(kind, formatted),
        "{kind}: the fix rewrites only the block's own lines"
    );
}

#[test]
fn rustfmt_formats_the_repro_block_as_statements() {
    let Some(env) = real_rustfmt("rustfmt_formats_the_repro_block_as_statements") else {
        return;
    };

    assert_rustfmt_frames(
        &env,
        "statements",
        "let limit = parse_limit(\"8\")?;\nassert_eq!(limit, 8);\n",
        "let limit=parse_limit(\"8\")?;\nassert_eq!(limit,8);\n",
    );
}

#[test]
fn frame_format_real_rustfmt_checks_statements_and_keeps_a_literal_exact() {
    let test = "frame_format_real_rustfmt_checks_statements_and_keeps_a_literal_exact";
    let Some(env) = real_rustfmt(test) else {
        return;
    };

    assert_rustfmt_frames(
        &env,
        "statements",
        "let s = \"a\n    b\";\nif s.is_empty() {\n    return;\n}\n",
        "let  s = \"a\n    b\";\nif s.is_empty() { return; }\n",
    );
}

#[test]
fn frame_format_real_rustfmt_checks_an_expression() {
    let Some(env) = real_rustfmt("frame_format_real_rustfmt_checks_an_expression") else {
        return;
    };

    assert_rustfmt_frames(
        &env,
        "expression",
        "parse_limit(\"8\").unwrap_or(0) + 1\n",
        "parse_limit( \"8\" ).unwrap_or(0)+1\n",
    );
}

#[test]
fn frame_format_real_rustfmt_holds_a_line_to_the_projects_max_width() {
    let test = "frame_format_real_rustfmt_holds_a_line_to_the_projects_max_width";
    let Some(env) = real_rustfmt(test) else {
        return;
    };
    let vars = borrowed(&env);
    let at_width = "let total = compute(alpha, beta, gamma);\n";
    let over_width = "let total = compute(alpha, beta, gammas);\n";
    assert_eq!(at_width.trim_end().len(), 40, "the passing line is 40 wide");
    let check_at_40 = |body: &str, exit: i32| {
        let project = rustfmt_project(&framed_doc("statements", body));
        project.write("rustfmt.toml", "max_width = 40\n");
        project
            .supdoc_in_with(".", &["check", "--json"], &vars)
            .assert_exit(exit)
            .json()
    };

    let passes = check_at_40(at_width, 0);
    let fails = check_at_40(over_width, 1);

    assert_passed_with_every_step_run(&passes);
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    assert_no_frame_text(&fails);
}
