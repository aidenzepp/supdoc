//! Checking the code blocks in docs: the `[features.code-blocks]` gate and the warnings `check`
//! gives for a block's info string.

use std::os::unix::fs::symlink;

use crate::support::{Project, array_at, at};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// The doc every test writes: a list item holding a block whose attribute is unknown.
const UNKNOWN_ATTRIBUTE_DOC: &str = "# Blocks\n\n- Run it:\n\n  ```sh hidden\n  echo hi\n  ```\n";

/// A recorded copy of the `basic` fixture with [`UNKNOWN_ATTRIBUTE_DOC`] at `docs/blocks.md`.
fn project_with_unknown_attribute() -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("docs/blocks.md", UNKNOWN_ATTRIBUTE_DOC);
    project
}

#[test]
fn without_the_table_a_doc_with_blocks_leaves_the_check_report_unchanged() {
    let project = Project::fixture("basic");
    project.record();
    let before = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    project.write("docs/blocks.md", UNKNOWN_ATTRIBUTE_DOC);
    let after = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert_eq!(
        before, after,
        "with the feature off, no block should be judged"
    );
}

#[test]
fn an_unknown_attribute_warns_and_fails_only_under_deny_warnings() {
    let project = project_with_unknown_attribute();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");

    let warned = project.supdoc(&["check", "--json"]).assert_exit(0).json();
    let _denied = project.supdoc(&["check", "--deny-warnings"]).assert_exit(1);

    assert_eq!(
        finding_kinds(&warned),
        ["code-block-unknown-attribute"],
        "{warned}"
    );
}

#[test]
fn known_attributes_and_a_bare_language_draw_no_warning() {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        "```rust no_run edition2021\nfn main() {}\n```\n\n~~~sh ignore(needs a server)\necho hi\n~~~\n\n```\nplain\n```\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

/// A recorded `basic` project with the feature on and one `sh` block carrying `attributes`.
fn project_with_block(attributes: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```sh {attributes}\necho hi\n```\n"),
    );
    project
}

#[test]
fn ignore_without_a_reason_warns_and_fails_only_under_deny_warnings() {
    let project = project_with_block("ignore");

    let warned = project.supdoc(&["check", "--json"]).assert_exit(0).json();
    let _denied = project.supdoc(&["check", "--deny-warnings"]).assert_exit(1);

    assert_eq!(
        finding_kinds(&warned),
        ["code-block-ignore-reason"],
        "{warned}"
    );
}

#[test]
fn ignore_with_blank_parentheses_warns_and_with_a_reason_does_not() {
    let blank = project_with_block("no_lint(  )");
    let reasoned = project_with_block("no_lint(slow, needs a server)");

    let blank_json = blank.supdoc(&["check", "--json"]).assert_exit(0).json();
    let reasoned_json = reasoned.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert_eq!(
        finding_kinds(&blank_json),
        ["code-block-ignore-reason"],
        "{blank_json}"
    );
    assert!(finding_kinds(&reasoned_json).is_empty(), "{reasoned_json}");
}

/// A recorded `basic` project with the feature on and a `rust` block in `docs/blocks.md`
/// carrying `attributes`, with `config` appended to the config.
fn project_with_rust_block(attributes: &str, config: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n{config}"),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```rust {attributes}\nfn add() {{}}\n```\n"),
    );
    project
}

/// How many `code-block-source-missing` findings a `--json` check printed.
fn source_missing_count(json: &serde_json::Value) -> usize {
    finding_kinds(json)
        .iter()
        .filter(|kind| *kind == "code-block-source-missing")
        .count()
}

#[test]
fn from_and_uses_that_resolve_draw_no_finding() {
    let project = project_with_rust_block(
        "from(../src/lib.rs#add) uses(/src/lib.rs#add) from(core:lib.rs)",
        "\n[namespaces]\ncore = \"src\"\n",
    );
    let lib = std::fs::read_to_string(project.path("src/lib.rs"))
        .expect("the fixture lib should be readable");
    project.write(
        "docs/blocks.md",
        &format!(
            "# Blocks\n\n```rust from(../src/lib.rs#add) uses(/src/lib.rs#add) from(core:lib.rs)\n\
             {ADD}\n\n{}\n```\n",
            lib.trim_end_matches('\n')
        ),
    );
    record_use(&project, "src/lib.rs", "add", ADD);

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

#[test]
fn from_a_missing_symbol_or_file_fails_once_per_value() {
    let project = project_with_rust_block(
        "from(../src/lib.rs#addd) uses(/src/gone.rs#add) from(../src/lib.rs#add)",
        "",
    );

    let run = project.supdoc(&["check", "--json"]);
    let json = run.assert_exit(1).json();

    assert_eq!(source_missing_count(&json), 2, "{json}");
}

#[test]
fn from_a_climb_an_unknown_namespace_or_a_url_fails() {
    let project = project_with_rust_block(
        "from(../../outside.rs#add) uses(nope:lib.rs#add) from(https://example.com/a.rs)",
        "",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(1).json();

    assert_eq!(source_missing_count(&json), 3, "{json}");
}

#[test]
fn from_or_uses_through_a_symlink_out_of_the_root_never_shows_the_outside_file() {
    let marker = "OUTSIDE_MARKER_7f3a";
    let outside = tempfile::tempdir().expect("an outside directory should be creatable");
    std::fs::write(
        outside.path().join("secret.rs"),
        format!("pub fn secret() {{ let _ = \"{marker}\"; }}\n"),
    )
    .expect("the outside file should be writable");
    let project = project_with_rust_block(
        "from(/src/secret.rs#secret) uses(/src/secret.rs#secret)",
        "",
    );
    symlink(
        outside.path().join("secret.rs"),
        project.path("src/secret.rs"),
    )
    .expect("the symlink should be creatable");

    let json_run = project.supdoc(&["check", "--json"]);
    let text_run = project.supdoc(&["check"]);

    let json = json_run.assert_exit(1).json();
    let _ = text_run.assert_exit(1);
    assert_eq!(source_missing_count(&json), 2, "{json}");
    for run in [&json_run, &text_run] {
        assert!(
            !run.stdout.contains(marker) && !run.stderr.contains(marker),
            "`supdoc {}` showed the outside file\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

/// The `add` function of the `basic` fixture's `src/lib.rs`, as an excerpt holds it.
const ADD: &str = "fn add(x: i32) -> i32 {\n    x + A\n}";

/// A recorded `basic` project with the feature on and `docs/blocks.md` holding one
/// `from(../src/lib.rs#add)` block whose body is `body`.
fn project_with_excerpt(body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```rust from(../src/lib.rs#add)\n{body}\n```\n"),
    );
    project
}

#[test]
fn excerpt_an_exact_copy_passes() {
    let project = project_with_excerpt(ADD);

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

#[test]
fn excerpt_an_edited_symbol_makes_the_block_stale() {
    let project = project_with_excerpt(ADD);
    project.write(
        "src/lib.rs",
        "const A: i32 = 10;\n\nfn add(x: i32) -> i32 {\n    x + A + 1\n}\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(1).json();

    assert!(
        finding_kinds(&json).contains(&"code-block-stale".to_owned()),
        "{json}"
    );
}

#[test]
fn excerpt_a_missing_part_reports_only_the_missing_source() {
    let project = project_with_excerpt("stale text");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```rust from(../src/lib.rs#add) from(../src/lib.rs#gone)\nstale\n```\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(1).json();

    assert_eq!(
        finding_kinds(&json),
        ["code-block-source-missing"],
        "{json}"
    );
}

/// The source the `uses_` tests track: `Counter.bump`, a method inside its `impl`.
const COUNTER: &str =
    "pub struct Counter;\n\nimpl Counter {\n    pub fn bump(&self) -> i32 {\n        1\n    }\n}\n";

/// The bytes of `Counter.bump` in [`COUNTER`], as supdoc hashes them.
const BUMP: &str = "pub fn bump(&self) -> i32 {\n        1\n    }";

/// A recorded `basic` project with the feature on, `src/counter.rs` holding [`COUNTER`], and
/// `docs/blocks.md` holding a hand-written `impl` block marked `uses(../src/counter.rs#Counter.bump)`.
fn project_with_use() -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write("src/counter.rs", COUNTER);
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```rust uses(../src/counter.rs#Counter.bump)\nimpl Counter {\n    pub fn bump(&self) -> i32 { 1 }\n}\n```\n",
    );
    project
}

/// Records `symbol_text`'s hash in `supdoc.lock` for `docs/blocks.md`'s `uses(...)` target
/// `target#symbol`, as `supdoc update` will.
fn record_use(project: &Project, target: &str, symbol: &str, symbol_text: &str) {
    use std::fmt::Write as _;

    use sha2::{Digest, Sha256};
    let hash =
        Sha256::digest(symbol_text.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
    let lock = project
        .read("supdoc.lock")
        .replace("version = 1", "version = 2");
    project.write(
        "supdoc.lock",
        &format!(
            "{lock}\n[[code-blocks.uses]]\nsource = \"docs/blocks.md\"\ntarget = \"{target}\"\nsymbol = \"{symbol}\"\nhash = \"{hash}\"\n"
        ),
    );
}

#[test]
fn uses_an_unrecorded_target_is_new_and_passes_once_recorded() {
    let project = project_with_use();

    let unrecorded = project.supdoc(&["check", "--json"]).assert_exit(1).json();
    record_use(&project, "src/counter.rs", "Counter.bump", BUMP);
    let recorded = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert_eq!(
        finding_kinds(&unrecorded),
        ["code-block-uses-new"],
        "{unrecorded}"
    );
    assert!(finding_kinds(&recorded).is_empty(), "{recorded}");
}

#[test]
fn uses_a_recorded_method_that_changes_fails_the_check() {
    let project = project_with_use();
    record_use(&project, "src/counter.rs", "Counter.bump", BUMP);
    let _passing = project.supdoc(&["check"]).assert_exit(0);

    project.replace("src/counter.rs", "        1\n", "        2\n");
    let json = project.supdoc(&["check", "--json"]).assert_exit(1).json();

    assert_eq!(finding_kinds(&json), ["code-block-uses-changed"], "{json}");
}

#[test]
fn update_preview_lists_the_new_use_and_writes_nothing() {
    let project = project_with_use();
    let before = project.read("supdoc.lock");

    let json = project.supdoc(&["update", "--json"]).assert_exit(0).json();

    let changes = array_at(&json, "/data/changes");
    assert_eq!(changes.len(), 1, "{json}");
    assert_eq!(at(&json, "/data/changes/0/action"), "record", "{json}");
    assert_eq!(at(&json, "/data/changes/0/uses"), true, "{json}");
    assert_eq!(
        project.read("supdoc.lock"),
        before,
        "a preview should not write the lockfile"
    );
}

#[test]
fn update_yes_records_the_use_and_the_next_check_passes() {
    let project = project_with_use();
    let _failing = project.supdoc(&["check"]).assert_exit(1);

    let _written = project.supdoc(&["update", "--yes"]).assert_exit(0);
    let recorded = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert!(finding_kinds(&recorded).is_empty(), "{recorded}");
    assert!(
        project.read("supdoc.lock").contains("[[code-blocks.uses]]"),
        "the lockfile should hold the recorded use"
    );

    project.replace("src/counter.rs", "        1\n", "        2\n");
    let _changed = project.supdoc(&["check"]).assert_exit(1);
    let _accepted = project.supdoc(&["update", "--yes"]).assert_exit(0);
    let _passing = project.supdoc(&["check"]).assert_exit(0);
}

#[test]
fn update_of_one_doc_keeps_another_docs_entries() {
    let project = project_with_use();
    project.write(
        "docs/other.md",
        "# Other\n\n```rust uses(../src/counter.rs#Counter.bump)\nimpl Counter {}\n```\n",
    );
    let _both = project.supdoc(&["update", "--yes"]).assert_exit(0);
    let both = project.read("supdoc.lock");
    project.write("docs/other.md", "# Other\n");

    let _one = project
        .supdoc(&["update", "docs/blocks.md", "--yes"])
        .assert_exit(0);

    assert_eq!(
        project.read("supdoc.lock"),
        both,
        "updating one doc should keep the entries of a doc outside its scope"
    );
}

#[test]
fn update_does_not_record_a_use_that_does_not_resolve() {
    let project = project_with_use();
    project.replace("docs/blocks.md", "Counter.bump", "Counter.missing");

    let json = project
        .supdoc(&["update", "--yes", "--json"])
        .assert_exit(0)
        .json();

    assert_eq!(
        finding_kinds(&json),
        ["code-block-source-missing"],
        "{json}"
    );
    assert!(
        !project.read("supdoc.lock").contains("code-blocks"),
        "an unresolved target should not be recorded"
    );
}

#[test]
fn uses_help_names_the_doc_from_where_check_ran() {
    let project = project_with_use();

    let from_root = project.supdoc(&["check"]);
    let from_docs = project.supdoc_in("docs", &["check"]);
    let _failed = from_root.assert_exit(1);
    let _failed = from_docs.assert_exit(1);

    assert!(
        from_root
            .stdout
            .contains("run `supdoc update docs/blocks.md --yes`"),
        "stdout: {}",
        from_root.stdout
    );
    assert!(
        from_docs
            .stdout
            .contains("run `supdoc update blocks.md --yes`"),
        "stdout: {}",
        from_docs.stdout
    );
}

#[test]
fn update_points_source_missing_at_the_uses_value() {
    let project = project_with_use();
    project.replace("docs/blocks.md", "Counter.bump", "Counter.missing");

    let run = project.supdoc(&["update", "--yes"]);
    let _recorded = run.assert_exit(0);

    assert!(
        run.stdout.contains("docs/blocks.md:3:14"),
        "the finding should point at the value after `rust uses(`:\n{}",
        run.stdout
    );
}

#[test]
fn a_python_block_names_the_chosen_formatter_when_it_is_missing() {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.python]\nformatter = \"black\"\n",
    );
    project.write("docs/blocks.md", "# Blocks\n\n```py\nx = 1\n```\n");
    let empty = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", empty.path())])
        .assert_exit(0)
        .json();

    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`black` isn't on PATH",
        "{json}"
    );
}

/// The text check's lines from `Links` on, with `·` separators written as `, `.
fn summary_lines(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip_while(|line| !line.starts_with("Links  "))
        .map(|line| line.replace(" · ", ", "))
        .collect()
}

#[test]
fn summary_counts_each_block_once_worst_first_after_the_links_line() {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\n",
    );
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```rust from(../src/lib.rs#gone)\nfn gone() {}\n```\n\n\
         ```rust\nfn main() {}\n```\n\n```sh\necho hi\n```\n\n```text\nplain\n```\n\n\
         ```toml\na = 1\n```\n\n```py ignore(needs a server)\nprint()\n```\n",
    );

    let run = project.supdoc(&["check"]);
    let _failed = run.assert_exit(1);

    let lines = summary_lines(&run.stdout);
    assert_eq!(
        lines.get(1).map(String::as_str),
        Some(
            "Code blocks  1 failed, 1 tool missing, 1 unknown language, 2 not checked, \
             1 lint not run (clippy needs trust), 1 opted out"
        ),
        "the code-block line should follow the Links line:\n{}",
        run.stdout
    );
}

#[test]
fn summary_has_no_code_block_line_without_the_table() {
    let project = Project::fixture("basic");
    project.record();
    project.write("docs/blocks.md", "# Blocks\n\n```rust\nfn main() {}\n```\n");

    let run = project.supdoc(&["check"]);
    let _passed = run.assert_exit(0);

    assert!(
        !run.stdout.contains("Code blocks"),
        "with the feature off there should be no code-block line:\n{}",
        run.stdout
    );
}

/// A recorded `basic` project with the feature on and one block of each kind in
/// `docs/blocks.md`: a failed excerpt, an unknown language, two opted out and a passing one.
fn project_with_mixed_blocks() -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```rust from(../src/lib.rs#gone)\nfn gone() {}\n```\n\n\
         ```toml\na = 1\n```\n\n```toml\nb = 2\n```\n\n\
         ```py ignore(needs a server)\nprint()\n```\n\n```text\nplain\n```\n",
    );
    project
}

#[test]
fn json_counts_sit_beside_the_link_counts_and_list_the_blocks_not_passed() {
    let project = project_with_mixed_blocks();

    let json = project.supdoc(&["check", "--json"]).assert_exit(1).json();

    assert_eq!(at(&json, "/data/summary/code_blocks/blocks"), 5, "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/unknown_language"), 2);
    assert!(
        at(&json, "/data/summary/passed").is_u64(),
        "the link counts should keep their path: {json}"
    );
    let blocks = array_at(&json, "/data/code_blocks");
    assert_eq!(blocks.len(), 5, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/file"),
        "docs/blocks.md",
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/line"), 3, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/language"), "rust", "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/3/reason"),
        "needs a server",
        "{json}"
    );
    assert_eq!(at(&json, "/schema_version"), 2);
}

#[test]
fn json_leaves_out_code_blocks_when_the_feature_is_off() {
    let project = Project::fixture("basic");
    project.record();
    project.write("docs/blocks.md", "# Blocks\n\n```toml\na = 1\n```\n");

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert!(json.pointer("/data/code_blocks").is_none(), "{json}");
    assert!(
        json.pointer("/data/summary/code_blocks").is_none(),
        "{json}"
    );
}

#[test]
fn json_lists_each_uses_target_with_its_state() {
    let project = project_with_use();
    let _recorded = project.supdoc(&["update", "--yes"]).assert_exit(0);
    project.replace("src/counter.rs", "1", "2");

    let json = project.supdoc(&["check", "--json"]).assert_exit(1).json();

    let blocks = array_at(&json, "/data/code_blocks");
    assert_eq!(blocks.len(), 1, "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/uses/0/target"),
        "src/counter.rs#Counter.bump",
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/code_blocks/0/uses/0/state"),
        "changed",
        "{json}"
    );
}

#[test]
fn json_jsonl_writes_one_code_block_record_per_entry_and_the_counts_on_the_summary() {
    let project = project_with_mixed_blocks();

    let run = project.supdoc(&["--jsonl", "check"]);
    let _failed = run.assert_exit(1);

    let lines: Vec<serde_json::Value> = run
        .stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("each line should be JSON ({error}): {line}"))
        })
        .collect();
    let records = lines
        .iter()
        .filter(|line| line["type"] == "code_block")
        .count();
    let summary = lines.last().expect("the stream should end with a summary");
    assert_eq!(records, 5, "{}", run.stdout);
    assert_eq!(summary["type"], "summary", "{}", run.stdout);
    assert_eq!(at(summary, "/summary/code_blocks/blocks"), 5, "{summary}");
}

#[test]
fn verbose_text_lists_unknown_languages_with_counts_and_reasons() {
    let project = project_with_mixed_blocks();

    let run = project.supdoc(&["check", "-v"]);
    let _failed = run.assert_exit(1);

    for line in [
        "unknown language  toml: 2",
        "opted out  docs/blocks.md:15 (needs a server)",
    ] {
        assert!(
            run.stdout.contains(line),
            "missing `{line}`:\n{}",
            run.stdout
        );
    }
}

#[test]
fn a_rust_doc_fence_leading_with_an_attribute_counts_as_rust_not_unknown() {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "src/blocks.rs",
        "/// ```no_run\n/// fn main() {}\n/// ```\npub fn f() {}\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();

    assert_eq!(
        at(&json, "/data/summary/code_blocks/unknown_language"),
        0,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        1,
        "{json}"
    );
}

/// The shell table with its linter off, so a format test sees only the formatter.
const SHELL_FORMAT_ONLY: &str = "[features.code-blocks]\n[features.code-blocks.shell]\n[features.code-blocks.shell.shellcheck]\nlint = \"off\"\n";

/// A recorded `basic` project with the shell table on, its linter off, and two `sh` blocks in
/// `docs/blocks.md`.
fn project_with_shell_blocks() -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", SHELL_FORMAT_ONLY);
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```sh\necho hi\n```\n\n```sh\necho bye\n```\n",
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `name` in `dir`.
fn stub(dir: &std::path::Path, name: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir).expect("the stub directory should be creatable");
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

#[test]
fn tool_missing_from_path_warns_once_and_counts_each_block() {
    let project = project_with_shell_blocks();

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();
    let _denied = project.supdoc(&["check", "--deny-warnings"]).assert_exit(1);

    assert_eq!(finding_kinds(&json), ["code-block-tool-missing"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/tool_missing"),
        2,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "tool_missing",
        "{json}"
    );
}

#[test]
fn tool_inside_the_checkout_is_refused_and_its_blocks_not_checked() {
    let project = project_with_shell_blocks();
    let bin = project.path("bin");
    stub(&bin, "shfmt");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &bin)])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        2,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/code_blocks/1/reason"),
        "`shfmt` is inside the checkout",
        "{json}"
    );
}

/// Runs `check --json` on [`project_with_shell_blocks`] with an `shfmt` stub running `script`,
/// outside the checkout, as the only `PATH` entry, after `args`, asserting it exits `exit`.
fn check_with_shfmt(script: &str, args: &[&str], exit: i32) -> serde_json::Value {
    use std::os::unix::fs::PermissionsExt as _;
    let project = project_with_shell_blocks();
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let path = outside.path().join("shfmt");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    let mut all = vec!["check", "--json"];
    all.extend_from_slice(args);
    project
        .supdoc_in_with(".", &all, &[("PATH", outside.path())])
        .assert_exit(exit)
        .json()
}

#[test]
fn shell_format_with_the_real_shfmt_passes_formatted_blocks_and_fails_others() {
    let Some(dir) = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .find(|dir| dir.is_absolute() && dir.join("shfmt").is_file())
    else {
        // Written to the process's stderr directly, which `cargo test` shows for a passing test.
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED shell_format_with_the_real_shfmt: shfmt is not on PATH\n",
        );
        return;
    };
    let project = project_with_shell_blocks();
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```sh\necho hi\n```\n\n```sh\nif true;then echo bye;fi\n```\n",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/failed"), 1, "{json}");
}

#[test]
fn shell_format_equal_output_passes_every_block() {
    let json = check_with_shfmt("/bin/cat", &[], 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 2, "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        0,
        "{json}"
    );
}

#[test]
fn shell_format_different_output_is_unformatted() {
    let json = check_with_shfmt("echo formatted", &[], 1);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-unformatted", "code-block-unformatted"],
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/failed"), 2, "{json}");
}

#[test]
fn shell_format_a_non_zero_exit_quotes_stderr_and_names_the_block_by_its_index() {
    let json = check_with_shfmt("echo \"bad $2\" >&2\nexit 1", &[], 1);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-tool-failed", "code-block-tool-failed"],
        "{json}"
    );
    let text = json.to_string();
    assert!(text.contains("docs/blocks.md.1.sh"), "{json}");
    assert!(text.contains("docs/blocks.md.2.sh"), "{json}");
}

#[test]
fn parse_shfmt_syntax_error_is_code_block_parse_at_the_tools_line() {
    let json = check_with_shfmt("echo \"$0:1:1: bad\" >&2\nexit 1", &[], 1);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-parse", "code-block-parse"],
        "{json}"
    );
    assert!(json.to_string().contains("ignore(<why>)"), "{json}");
}

#[test]
fn parse_other_failures_stay_tool_failed() {
    let json = check_with_shfmt("echo oops >&2\nexit 1", &[], 1);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-tool-failed", "code-block-tool-failed"],
        "{json}"
    );
}

#[test]
fn shell_format_a_tool_past_the_timeout_is_killed_and_reported() {
    let started = std::time::Instant::now();
    let json = check_with_shfmt("/bin/sleep 30", &["--code-block-timeout", "200ms"], 1);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-tool-timeout", "code-block-tool-timeout"],
        "{json}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
}

/// Whether `/bin/kill -0` still reaches the process `pid` within two seconds of polling.
fn still_alive(pid: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let alive = std::process::Command::new("/bin/kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("/bin/kill should run")
            .success();
        if !alive {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

/// How a `check` run that got SIGTERM while its tools ran ended.
struct Terminated {
    /// supdoc's own exit status.
    status: std::process::ExitStatus,
    /// Everything supdoc wrote to stdout.
    stdout: String,
    /// Everything supdoc wrote to stderr.
    stderr: String,
    /// The pid of every tool supdoc started, one per tool run.
    tools: Vec<String>,
}

/// Runs `check` on [`project_with_shell_blocks`] with `blocks` shell blocks and an `shfmt` stub
/// that records its pid and sleeps, sends supdoc SIGTERM once `running` tools have started, and
/// waits for it to end.
fn sigterm_while_tools_run(blocks: usize, running: usize) -> Terminated {
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};
    let project = project_with_shell_blocks();
    // Each block distinct, so every one runs its own tool.
    let mut doc = String::from("# Blocks\n");
    for n in 0..blocks {
        writeln!(doc, "\n```sh\necho {n}\n```").expect("writing to a String should not fail");
    }
    project.write("docs/blocks.md", &doc);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let pids = outside.path().join("pids");
    std::fs::create_dir(&pids).expect("the pid directory should be creatable");
    let bin = outside.path().join("bin");
    std::fs::create_dir(&bin).expect("the stub directory should be creatable");
    let stub = bin.join("shfmt");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\n: > '{}/'$$\nexec /bin/sleep 30\n",
            pids.display()
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    let (stdout_path, stderr_path) = (outside.path().join("out"), outside.path().join("err"));
    let started = Instant::now();
    let mut supdoc = std::process::Command::new(env!("CARGO_BIN_EXE_supdoc"))
        .args(["check", "--code-block-timeout", "60s"])
        .current_dir(project.path("."))
        .env_clear()
        .env("PATH", &bin)
        .stdout(std::fs::File::create(&stdout_path).expect("the stdout file should be creatable"))
        .stderr(std::fs::File::create(&stderr_path).expect("the stderr file should be creatable"))
        .spawn()
        .expect("the supdoc binary should start");
    let started_tools = || {
        std::fs::read_dir(&pids)
            .expect("the pid directory should be readable")
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect::<Vec<_>>()
    };

    while started_tools().len() < running {
        if started.elapsed() > Duration::from_secs(10) {
            drop(supdoc.kill());
            panic!("only {} stub tools started", started_tools().len());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let sent = std::process::Command::new("/bin/kill")
        .args(["-TERM", &supdoc.id().to_string()])
        .status()
        .expect("/bin/kill should run");
    assert!(sent.success(), "SIGTERM should be delivered");
    let status = loop {
        if let Some(status) = supdoc.try_wait().expect("the run should be waitable") {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            drop(supdoc.kill());
            panic!("supdoc did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read =
        |path| std::fs::read_to_string(path).expect("the captured stream should be readable");
    Terminated {
        status,
        stdout: read(&stdout_path),
        stderr: read(&stderr_path),
        tools: started_tools(),
    }
}

/// The tools in `tools` that still run, each killed so none outlives the test.
fn survivors(tools: &[String]) -> Vec<String> {
    let alive: Vec<String> = tools
        .iter()
        .filter(|pid| still_alive(pid))
        .cloned()
        .collect();
    for pid in &alive {
        let _killed = std::process::Command::new("/bin/kill")
            .args(["-KILL", pid])
            .stderr(std::process::Stdio::null())
            .status();
    }
    alive
}

#[test]
fn sigterm_during_a_tool_run_kills_the_tool_and_ends_supdoc_by_sigterm() {
    use std::os::unix::process::ExitStatusExt as _;

    let run = sigterm_while_tools_run(2, 1);

    assert_eq!(
        run.status.signal(),
        Some(15),
        "supdoc should end by SIGTERM: {:?}",
        run.status
    );
    assert_eq!(
        (run.stdout.as_str(), run.stderr.as_str()),
        ("", ""),
        "supdoc should print nothing after SIGTERM"
    );
    assert_eq!(
        survivors(&run.tools),
        Vec::<String>::new(),
        "no stub tool should outlive supdoc; started: {:?}",
        run.tools
    );
}

#[test]
fn sigterm_leaves_no_tool_running_when_blocks_outnumber_workers() {
    use std::os::unix::process::ExitStatusExt as _;
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8);

    let run = sigterm_while_tools_run(40, workers);

    assert_eq!(
        run.status.signal(),
        Some(15),
        "supdoc should end by SIGTERM: {:?}",
        run.status
    );
    assert_eq!(
        survivors(&run.tools),
        Vec::<String>::new(),
        "no stub tool, even one started after the signal, should outlive supdoc; started: {:?}",
        run.tools
    );
}

/// Runs `check --json` on [`project_with_shell_blocks`] with `config` as its config and an
/// `shfmt` stub running `script` as the only `PATH` entry, outside the checkout.
fn check_shell_with(config: &str, script: &str) -> serde_json::Value {
    use std::os::unix::fs::PermissionsExt as _;
    let project = project_with_shell_blocks();
    project.write("supdoc.config.toml", config);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let path = outside.path().join("shfmt");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json()
}

#[test]
fn selection_format_off_counts_format_off_and_never_runs_the_tool() {
    let project = project_with_shell_blocks();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.shell]\n[features.code-blocks.shell.shfmt]\nformat = \"off\"\n[features.code-blocks.shell.shellcheck]\nlint = \"off\"\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();
    let run = project.supdoc(&["check", "-v"]);

    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_off"),
        2,
        "{json}"
    );
    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert!(run.stdout.contains("format off"), "{}", run.stdout);
}

#[test]
fn selection_no_format_counts_format_skipped_with_its_reason() {
    let project = project_with_shell_blocks();
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```sh no_format(hand aligned)\necho   hi\n```\n",
    );

    let json = project.supdoc(&["check", "--json"]).assert_exit(0).json();
    let run = project.supdoc(&["check", "-v"]);

    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_skipped"),
        1,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "hand aligned; `lint = \"off\"` in `[features.code-blocks.shell.shellcheck]`",
        "{json}"
    );
    assert!(
        run.stdout.contains("(hand aligned; `lint = \"off\"` in"),
        "both steps' reasons should show: {}",
        run.stdout
    );
}

#[test]
fn lint_select_lint_off_counts_lint_off_beside_the_format_outcome() {
    let project = project_with_shell_blocks();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.shell]\n[features.code-blocks.shell.shellcheck]\nlint = \"off\"\n",
    );
    let outside = counting_shfmt();

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/lint_off"), 2, "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 2, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "ran", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "off", "{json}");
}

#[test]
fn lint_select_no_lint_counts_lint_skipped_with_its_reason() {
    let project = project_with_shell_blocks();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.shell]\n",
    );
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```sh no_lint(needs a server)\necho hi\n```\n\n```sh\necho bye\n```\n",
    );
    let outside = counting_shfmt();

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_skipped"),
        1,
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 2, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "skipped", "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "needs a server",
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/1/lint"), "not_run", "{json}");
}

#[test]
fn lint_select_an_unknown_linter_or_a_command_table_exits_78() {
    for line in [
        "[features.code-blocks.python]\nlinter = \"pylint\"",
        "[features.code-blocks.rust]\nlinter = \"shellcheck\"",
        "[features.code-blocks.shell]\nlinter = { command = [\"shellcheck\"] }",
    ] {
        let project = project_with_shell_blocks();
        project.write(
            "supdoc.config.toml",
            &format!("[features.code-blocks]\n{line}\n"),
        );

        let run = project.supdoc(&["check", "--json"]);
        let _checked = run.assert_exit(78);

        assert!(
            run.stderr.contains("\"config-parse\""),
            "`{line}`: {}",
            run.stderr
        );
    }
}

#[test]
fn selection_dialect_posix_reaches_shfmt_as_ln_posix() {
    let json = check_shell_with(
        "[features.code-blocks]\n[features.code-blocks.shell]\ndialect = \"posix\"\n",
        "echo \"args: $*\" >&2\nexit 1",
    );

    assert!(json.to_string().contains("-ln posix"), "{json}");
}

/// An `shfmt` stub outside the checkout that appends a line to `runs.log` beside it each time it
/// runs and prints its stdin: the directory holding both.
fn counting_shfmt() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let log = outside.path().join("runs.log");
    let path = outside.path().join("shfmt");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho run >> '{}'\n/bin/cat\n", log.display()),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    outside
}

/// How many times the [`counting_shfmt`] stub in `outside` has run.
fn runs(outside: &tempfile::TempDir) -> usize {
    std::fs::read_to_string(outside.path().join("runs.log")).map_or(0, |log| log.lines().count())
}

/// Runs `check --json` on `project` with the stub in `outside` as `PATH` and `cache` as
/// `XDG_CACHE_HOME`, asserting it passes every block.
fn check_cached(project: &Project, outside: &tempfile::TempDir, cache: &std::path::Path) {
    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", outside.path()), ("XDG_CACHE_HOME", cache)],
        )
        .assert_exit(0)
        .json();
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 2, "{json}");
}

#[test]
fn cache_a_second_unchanged_run_runs_no_formatter() {
    let project = project_with_shell_blocks();
    let outside = counting_shfmt();
    let cache = outside.path().join("cache");

    check_cached(&project, &outside, &cache);
    let first = runs(&outside);
    check_cached(&project, &outside, &cache);

    assert_eq!((first, runs(&outside)), (2, 2));
}

#[test]
fn cache_a_changed_block_runs_again_and_a_corrupt_entry_never_passes() {
    let project = project_with_shell_blocks();
    let outside = counting_shfmt();
    let cache = outside.path().join("cache");
    check_cached(&project, &outside, &cache);

    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```sh\necho hi\n```\n\n```sh\necho changed\n```\n",
    );
    check_cached(&project, &outside, &cache);
    let after_change = runs(&outside);
    for dir in std::fs::read_dir(cache.join("supdoc/code-blocks")).expect("the cache should exist")
    {
        let dir = dir.expect("the cache entry should list").path();
        for entry in std::fs::read_dir(dir).expect("the root's cache should list") {
            let entry = entry.expect("the cache entry should list").path();
            std::fs::write(entry, "garbage").expect("the entry should be writable");
        }
    }
    check_cached(&project, &outside, &cache);

    assert_eq!((after_change, runs(&outside)), (3, 5));
}

#[test]
fn cache_a_config_appearing_in_the_docs_own_directory_runs_the_tool_again() {
    let project = project_with_shell_blocks();
    let outside = counting_shfmt();
    let cache = outside.path().join("cache");
    check_cached(&project, &outside, &cache);

    project.write("docs/.editorconfig", "[*.sh]\nindent_style = tab\n");
    check_cached(&project, &outside, &cache);

    assert_eq!(runs(&outside), 4);
}

/// How many blocks the parallel tests write.
const PARALLEL_BLOCKS: usize = 16;

/// Runs `check --json` on a recorded `basic` project with the shell table on, its linter off, and
/// [`PARALLEL_BLOCKS`] `sh` blocks in `docs/blocks.md`, with an `shfmt` stub running `script`
/// as the only `PATH` entry, asserting it exits `exit`. Returns the report and how long the run
/// took.
fn check_many_with_shfmt(script: &str, exit: i32) -> (serde_json::Value, std::time::Duration) {
    use std::os::unix::fs::PermissionsExt as _;
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", SHELL_FORMAT_ONLY);
    let blocks = (1..=PARALLEL_BLOCKS)
        .map(|index| format!("```sh\necho {index}\n```\n"))
        .collect::<Vec<_>>()
        .join("\n");
    project.write("docs/blocks.md", &format!("# Blocks\n\n{blocks}"));
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let path = outside.path().join("shfmt");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    let started = std::time::Instant::now();
    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(exit)
        .json();
    (json, started.elapsed())
}

#[test]
fn parallel_sixteen_blocks_with_a_300ms_formatter_finish_in_under_3s() {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    if cores < 4 {
        // Written to the process's stderr directly, which `cargo test` shows for a passing test.
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!(
                "SKIPPED parallel_sixteen_blocks: available_parallelism() is {cores}, below 4\n"
            )
            .as_bytes(),
        );
        return;
    }

    let (json, took) = check_many_with_shfmt("/bin/sleep 0.3\n/bin/cat", 0);

    assert_eq!(
        at(&json, "/data/summary/code_blocks/passed"),
        PARALLEL_BLOCKS,
        "{json}"
    );
    assert!(
        took < std::time::Duration::from_secs(3),
        "16 runs of 300ms took {took:?}, so they didn't run in parallel"
    );
}

#[test]
fn parallel_findings_come_out_in_document_order() {
    // Earlier blocks sleep longer, so their runs finish last.
    let script = "n=${2##*.md.}\nn=${n%.sh}\n/bin/sleep \"$(printf '0.%02d' $(( (17 - n) * 5 )))\"\necho \"bad $2\" >&2\nexit 1";

    let (json, _took) = check_many_with_shfmt(script, 1);

    let messages: Vec<String> = array_at(&json, "/findings")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(messages.len(), PARALLEL_BLOCKS, "{json}");
    for (position, message) in messages.iter().enumerate() {
        let name = format!("docs/blocks.md.{}.sh", position + 1);
        assert!(
            message.contains("code-block-tool-failed") && message.contains(&name),
            "finding {position} should be block {}'s tool failure: {message}",
            position + 1
        );
    }
    assert_eq!(
        at(&json, "/data/summary/code_blocks/failed"),
        PARALLEL_BLOCKS,
        "{json}"
    );
}

/// A recorded `basic` project with the shell table (and `config` after it) and `docs/blocks.md`
/// holding `doc`, plus an outside directory holding a `shellcheck` stub running `script` and an
/// `shfmt` that prints its stdin.
fn project_with_shellcheck(config: &str, doc: &str, script: &str) -> (Project, tempfile::TempDir) {
    use std::os::unix::fs::PermissionsExt as _;
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.shell]\n{config}"),
    );
    project.write("docs/blocks.md", doc);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for (name, body) in [("shellcheck", script), ("shfmt", "/bin/cat")] {
        let path = outside.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the stub should be writable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
    }
    (project, outside)
}

#[test]
fn shell_lint_runs_shellcheck_with_its_dialect_and_no_rc_and_places_each_diagnostic() {
    let (project, outside) = project_with_shellcheck(
        "dialect = \"posix\"\n",
        "# Blocks\n\n```sh\nif true; then\n\techo $x\nfi\n```\n",
        "[ \"$*\" = '-s sh --norc --format=json1 --exclude=SC2154,SC2034,SC1091 -' ] || { echo \"argv: $*\" >&2; exit 3; }\n\
         /bin/cat > /dev/null\n\
         echo '{\"comments\":[{\"file\":\"-\",\"line\":2,\"endLine\":2,\"column\":14,\"endColumn\":16,\"level\":\"info\",\"code\":2086,\"message\":\"Double quote to prevent globbing and word splitting.\",\"fix\":null}]}'\n\
         exit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    // The tab before `echo` spans shellcheck's columns 1-8, so its column 14 is the 7th character.
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert_eq!(at(&json, "/findings/0/location/column"), 7, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn shell_lint_a_non_zero_exit_without_comments_is_a_tool_failure() {
    let (project, outside) = project_with_shellcheck(
        "",
        "# Blocks\n\n```sh\necho hi\n```\n",
        "echo 'shellcheck: internal error' >&2\nexit 2",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
}

#[test]
fn shell_lint_with_the_real_shellcheck_passes_a_clean_block_and_fails_a_placeholder() {
    let Some(dir) = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .find(|dir| dir.is_absolute() && dir.join("shellcheck").is_file())
    else {
        // Written to the process's stderr directly, which `cargo test` shows for a passing test.
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            b"SKIPPED shell_lint_with_the_real_shellcheck: shellcheck is not on PATH\n",
        );
        return;
    };
    let (project, _outside) = project_with_shellcheck(
        "[features.code-blocks.shell.shfmt]\nformat = \"off\"\n",
        "# Blocks\n\n```sh\necho \"hi\"\n```\n\n```sh\necho $1\n```\n\n```sh\ncp <src> <dst>\n```\n",
        "exit 0",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    // `code_blocks` lists only the blocks that didn't pass: `echo $1` (SC2086) and the placeholder.
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/failed"), 2, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/line"), 7, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/1/line"), 11, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/1/state"), "failed", "{json}");
}
