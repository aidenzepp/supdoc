//! Checking TypeScript and JavaScript code blocks, formatted by biome.

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

/// A recorded `basic` project with `tables` (written under `[features.code-blocks]`) and one
/// block tagged `tag` holding `body` in `docs/blocks.md`.
fn project_with_block(tables: &str, tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n{tables}"),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
    );
    project
}

/// A project with a `typescript` table and one `ts` block.
fn ts_project(body: &str) -> Project {
    project_with_block("[features.code-blocks.typescript]\n", "ts", body)
}

/// Writes an executable `#!/bin/sh` stub named `biome` running `script` in `dir`.
fn stub_biome(dir: &Path, script: &str) {
    let path = dir.join("biome");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A stub that accepts only `biome format --colors=off --stdin-file-path=<doc>.1.<ext>` with a
/// `--config-path=` flag, then runs `then`.
fn stub_for(ext: &str, then: &str) -> String {
    format!(
        "[ \"$1 $2\" = 'format --colors=off' ] || exit 3\n\
         case \"$3\" in --stdin-file-path=*/docs/blocks.md.1.{ext}) ;; *) exit 4;; esac\n\
         case \"$4\" in --config-path=*) ;; *) exit 5;; esac\n\
         {then}"
    )
}

#[test]
fn a_formatted_block_passes_and_biome_gets_the_virtual_path_and_an_empty_config() {
    let project = ts_project("const a = 1;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), &stub_for("ts", "/bin/cat"));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn an_unformatted_block_fails_with_biomes_output() {
    let project = ts_project("const   a = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(
        outside.path(),
        &stub_for("ts", "/bin/cat >/dev/null\necho 'const a = 1;'"),
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`biome-format` would format it differently",
        "{json}"
    );
}

#[test]
fn a_parse_error_is_reported_at_the_doc_line_biome_names() {
    let project = ts_project("const a = 1;\nlet b = ;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(
        outside.path(),
        &stub_for(
            "ts",
            "echo \"${3#--stdin-file-path=}:2:9 parse ━━━━━━━━\" >&2\n\
             echo '  × Expected an expression' >&2\nexit 1",
        ),
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn each_tag_gets_the_extension_it_names() {
    for (tag, table, ext) in [
        ("typescript", "typescript", "ts"),
        ("mts", "typescript", "mts"),
        ("cts", "typescript", "cts"),
        ("tsx", "typescript", "tsx"),
        ("js", "javascript", "js"),
        ("javascript", "javascript", "js"),
        ("mjs", "javascript", "mjs"),
        ("cjs", "javascript", "cjs"),
        ("jsx", "javascript", "jsx"),
    ] {
        let project = project_with_block(
            &format!("[features.code-blocks.{table}]\n"),
            tag,
            "const a = 1;",
        );
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_biome(outside.path(), &stub_for(ext, "/bin/cat"));

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "tag {tag}: {json}"
        );
    }
}

#[test]
fn a_language_without_its_table_is_not_checked() {
    let project = project_with_block("[features.code-blocks.javascript]\n", "ts", "let a;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), "exit 9");

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "no `[features.code-blocks.typescript]` table",
        "{json}"
    );
}

#[test]
fn biome_is_given_the_validated_config_as_strict_json_never_the_projects_bytes() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let record = outside.path().join("seen.json");
    stub_biome(
        outside.path(),
        &format!(
            "case \"$4\" in --config-path=*) /bin/cp \"${{4#--config-path=}}\" '{}'; /bin/cat;; *) exit 5;; esac",
            record.display()
        ),
    );
    let project = ts_project("const a = 1;");
    project.write(
        "docs/biome.json",
        "{ // note\n \"formatter\": { \"indentWidth\": 4, }, }",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    let seen = std::fs::read_to_string(&record).expect("the stub recorded the config");
    assert_eq!(seen, "{\"formatter\":{\"indentWidth\":4}}");
}

#[test]
fn a_config_with_plugins_hidden_by_a_bare_cr_comment_never_reaches_biome_with_them() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let record = outside.path().join("seen.json");
    stub_biome(
        outside.path(),
        &format!(
            "/bin/cp \"${{4#--config-path=}}\" '{}'; /bin/cat",
            record.display()
        ),
    );
    let project = ts_project("const a = 1;");
    project.write(
        "biome.jsonc",
        "{\"formatter\":{} // x, \"plugins\": [\"./p.js\"]\r}",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .json();

    let seen = std::fs::read_to_string(&record).unwrap_or_default();
    assert!(
        !seen.contains("plugins") && !seen.contains("p.js"),
        "biome was given {seen}: {json}"
    );
}

#[test]
fn a_config_with_plugins_makes_the_block_not_checked_with_its_own_help() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), "exit 9");
    let project = ts_project("const a = 1;");
    project.write(
        "biome.jsonc",
        "{ // load one\n \"plugins\": [\"./x.grit\"] }",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/findings/0/help")
            .as_str()
            .map(|h| h.contains("remove `plugins`")),
        Some(true),
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
}

#[test]
fn a_config_biome_rejects_is_a_tool_failure_never_a_block_parse_error() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(
        outside.path(),
        "echo \"${4#--config-path=}:2:3 parse ━━━━\" >&2\necho '  × bad' >&2\nexit 1",
    );
    let project = ts_project("const a = 1;");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
}

#[test]
fn the_real_biome_passes_a_formatted_block_fails_another_and_reports_a_parse_error() {
    let Some(dir) = real_tool_dir(
        "biome",
        "the_real_biome_passes_a_formatted_block_fails_another_and_reports_a_parse_error",
    ) else {
        return;
    };
    let formatted = ts_project("const a = 1;");
    let unformatted = ts_project("const   a = 1");
    let broken = ts_project("const a = 1;\nlet b = ;");

    let passes = formatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = unformatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();
    let parse = broken
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    assert_eq!(finding_kinds(&parse), ["code-block-parse"], "{parse}");
}

#[test]
fn the_real_biome_reads_a_tsx_block_as_jsx() {
    let Some(dir) = real_tool_dir("biome", "the_real_biome_reads_a_tsx_block_as_jsx") else {
        return;
    };
    let project = project_with_block(
        "[features.code-blocks.typescript]\n",
        "tsx",
        "const a = <div />;",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}
