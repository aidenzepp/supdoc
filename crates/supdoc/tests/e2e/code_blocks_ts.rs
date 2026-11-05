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

/// Writes an executable `#!/bin/sh` stub named `biome` in `dir` that passes `biome lint` and runs
/// `script` for anything else, the format step.
fn stub_biome(dir: &Path, script: &str) {
    stub_biome_lint(dir, &format!("[ \"$1\" = lint ] && exit 0\n{script}"));
}

/// Writes an executable `#!/bin/sh` stub named `biome` running `script` in `dir`, for every
/// subcommand.
fn stub_biome_lint(dir: &Path, script: &str) {
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
fn the_audited_exploit_a_cr_ended_comment_before_plugins_never_reaches_biome() {
    for name in ["biome.jsonc", ".biome.jsonc"] {
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
            name,
            "{\"formatter\":{} // x\r, \"plugins\": [\"./p.js\"]\n}",
        );

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .json();

        let seen = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(
            !seen.contains("plugins") && !seen.contains("p.js"),
            "{name}: biome was given {seen}: {json}"
        );
        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{name}: {json}"
        );
    }
}

#[test]
fn a_dir_scoped_override_applies_to_a_block_in_that_dir_and_not_outside_it() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let seen = outside.path().join("path.txt");
    stub_biome(
        outside.path(),
        &format!(
            "echo \"${{3#--stdin-file-path=}}\" > '{}'; /bin/cat",
            seen.display()
        ),
    );
    let project = ts_project("const a = 1;");
    project.write(
        "biome.json",
        "{\"overrides\":[{\"includes\":[\"docs/**\"],\"formatter\":{\"indentWidth\":8}}]}",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    let path = std::fs::read_to_string(&seen).expect("the stub recorded the path");
    let (private, relative) = path
        .trim()
        .split_once("/biome-config/")
        .expect("the virtual path is under the private config dir");
    assert!(private.contains("supdoc/code-blocks"), "{path}");
    assert!(relative.ends_with("/docs/blocks.md.1.ts"), "{path}");
    assert!(
        !relative.contains(".tmp"),
        "the project's own path leaked: {path}"
    );
}

#[test]
fn the_real_biome_applies_a_dir_scoped_override_only_inside_that_dir() {
    let Some(dir) = real_tool_dir(
        "biome",
        "the_real_biome_applies_a_dir_scoped_override_only_inside_that_dir",
    ) else {
        return;
    };
    let project = project_with_block(
        "[features.code-blocks.typescript]\n",
        "ts",
        "function f() {\n        return 1;\n}",
    );
    project.write(
        "biome.json",
        "{\"overrides\":[{\"includes\":[\"docs/**\"],\"formatter\":{\"indentStyle\":\"space\",\"indentWidth\":8}}]}",
    );
    project.write(
        "other/blocks.md",
        "# O\n\n```ts\nfunction f() {\n        return 1;\n}\n```\n",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    let states: Vec<_> = array_at(&json, "/data/code_blocks")
        .iter()
        .map(|block| {
            (
                block["file"].as_str().unwrap_or_default().to_owned(),
                block["state"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert!(
        states.contains(&("docs/blocks.md".to_owned(), "passed".to_owned())),
        "{json}"
    );
    assert!(
        states.contains(&("other/blocks.md".to_owned(), "failed".to_owned())),
        "{json}"
    );
}

#[test]
fn a_relative_tailwind_stylesheet_is_given_to_biome_absolute_and_an_outside_one_is_not_checked() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let record = outside.path().join("seen.json");
    stub_biome(
        outside.path(),
        &format!(
            "/bin/cp \"${{4#--config-path=}}\" '{}'; /bin/cat",
            record.display()
        ),
    );
    let inside = ts_project("const a = 1;");
    inside.write(
        "biome.json",
        "{\"tailwind\":{\"stylesheet\":\"./docs/app.css\"}}",
    );
    inside.write("docs/app.css", "");
    let escaping = ts_project("const a = 1;");
    escaping.write(
        "biome.json",
        "{\"tailwind\":{\"stylesheet\":\"../app.css\"}}",
    );

    let passed = inside
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();
    let refused = escaping
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(
        at(&passed, "/data/summary/code_blocks/passed"),
        1,
        "{passed}"
    );
    let seen = std::fs::read_to_string(&record).expect("the stub recorded the config");
    assert!(
        seen.contains("/docs/app.css\"") && !seen.contains("./docs"),
        "{seen}"
    );
    assert_eq!(
        finding_kinds(&refused),
        ["code-block-not-checked"],
        "{refused}"
    );
    assert!(
        at(&refused, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("tailwind.stylesheet")),
        "{refused}"
    );
}

#[test]
fn a_config_error_biome_reports_names_the_projects_config_file() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(
        outside.path(),
        "echo \"${4#--config-path=}:1:2 configuration ━━━━\" >&2\nexit 1",
    );
    let project = ts_project("const a = 1;");
    project.write("biome.json", "{}");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    let message = at(&json, "/findings/0/message")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("biome.json:1:2") && !message.contains("biome-config"),
        "{message}"
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

/// A stub that passes `biome format` as `/bin/cat`, and for `biome lint` records its arguments, its
/// block file's path and content and the config beside it in `records`, then prints `report` and exits
/// `code`.
fn lint_stub(records: &Path, report: &str, code: u8) -> String {
    let records = records.display();
    format!(
        "[ \"$1\" = lint ] || exec /bin/cat\n\
         for arg in \"$@\"; do echo \"$arg\"; done > '{records}/args'\n\
         for arg in \"$@\"; do file=\"$arg\"; done\n\
         /bin/cp \"$file\" '{records}/block'\n\
         /bin/cp \"${{file%/*}}/biome.json\" '{records}/config'\n\
         echo \"$file\" > '{records}/file'\n\
         echo '{report}'\n\
         exit {code}"
    )
}

#[test]
fn lint_a_diagnostic_is_a_code_block_lint_error_at_its_doc_line_from_a_private_file() {
    let project = ts_project("const a = 1;\ndebugger;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let report = r#"{"summary":{},"diagnostics":[{"severity":"error","message":"This is an unexpected use of the debugger statement.","category":"lint/suspicious/noDebugger","location":{"path":"x","start":{"line":2,"column":1},"end":{"line":2,"column":10}},"advices":[]}],"command":"lint"}"#;
    stub_biome_lint(outside.path(), &lint_stub(outside.path(), report, 1));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert_eq!(at(&json, "/findings/0/location/column"), 1, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("`biome-lint` reports `suspicious/noDebugger`")),
        "{json}"
    );
    let read = |name: &str| {
        std::fs::read_to_string(outside.path().join(name)).expect("the stub recorded its run")
    };
    let args = read("args");
    for skipped in [
        "--skip=correctness/noUnusedVariables",
        "--skip=correctness/noUnusedImports",
        "--skip=correctness/noUnusedFunctionParameters",
        "--reporter=json",
    ] {
        assert!(args.lines().any(|arg| arg == skipped), "{args}");
    }
    assert_eq!(read("block"), "const a = 1;\ndebugger;\n");
    let file = read("file");
    let file = Path::new(file.trim());
    assert!(
        file.to_string_lossy().contains("/biome-lint/")
            && file.ends_with("blocks.md.1.ts")
            && !file.starts_with(project.path("")),
        "{file:?}"
    );
    assert!(
        args.lines().any(|arg| arg
            == format!(
                "--config-path={}",
                file.with_file_name("biome.json").display()
            )),
        "with no project config, the config sits beside the block: {args}"
    );
    assert_eq!(read("config"), "{}");
    assert!(
        !file.parent().is_some_and(Path::exists),
        "the run's private directory should be gone: {file:?}"
    );
}

#[test]
fn lint_a_clean_block_passes_and_lint_off_in_the_tool_table_never_runs_biome_lint() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let clean = r#"{"summary":{},"diagnostics":[],"command":"lint"}"#;
    stub_biome_lint(outside.path(), &lint_stub(outside.path(), clean, 0));
    let passing = project_with_block("[features.code-blocks.javascript]\n", "js", "f();");
    let off = project_with_block(
        "[features.code-blocks.javascript]\n\
         [features.code-blocks.javascript.biome-lint]\nlint = \"off\"\n",
        "js",
        "f();",
    );

    let passes = passing
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();
    assert!(
        outside.path().join("args").exists(),
        "biome lint should have run"
    );
    std::fs::remove_file(outside.path().join("args")).expect("the record should be removable");
    let skipped = off
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(
        at(&skipped, "/data/summary/code_blocks/passed"),
        1,
        "{skipped}"
    );
    assert!(
        !outside.path().join("args").exists(),
        "biome lint ran though its table turns it off"
    );
}

#[test]
fn lint_the_real_biome_reports_a_debugger_skips_unused_names_and_honors_a_dir_scoped_override() {
    let Some(dir) = real_tool_dir(
        "biome",
        "lint_the_real_biome_reports_a_debugger_skips_unused_names_and_honors_a_dir_scoped_override",
    ) else {
        return;
    };
    let body = "import { x } from \"y\";\n\nconst unused = 1;\nfunction f(a) {}\ndebugger;";
    let project = project_with_block("[features.code-blocks.javascript]\n", "js", body);
    project.write(
        "biome.json",
        "{\"overrides\":[{\"includes\":[\"other/**\"],\"linter\":{\"rules\":{\"suspicious\":{\"noDebugger\":\"off\"}}}}]}",
    );
    project.write("other/blocks.md", &format!("# O\n\n```js\n{body}\n```\n"));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    let found: Vec<_> = array_at(&json, "/findings")
        .iter()
        .map(|finding| {
            (
                finding["kind"].as_str().unwrap_or_default().to_owned(),
                finding
                    .pointer("/location/file")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                finding
                    .pointer("/location/line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [("code-block-lint".to_owned(), "docs/blocks.md".to_owned(), 8)],
        "{json}"
    );
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("suspicious/noDebugger")),
        "{json}"
    );
}
