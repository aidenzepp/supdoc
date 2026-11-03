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
        Some("Code blocks  1 failed, 1 unknown language, 3 not checked, 1 opted out"),
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
