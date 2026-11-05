//! Checking TypeScript and JavaScript code blocks, formatted by biome or prettier.

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

/// Writes an executable `#!/bin/sh` stub named `biome` in `dir` that passes `biome lint` with a
/// clean report and runs `script` for anything else, the format step.
fn stub_biome(dir: &Path, script: &str) {
    stub_biome_lint(
        dir,
        &format!(
            "[ \"$1\" = lint ] && echo '{{\"summary\":{{\"changed\":0,\"unchanged\":1}},\"diagnostics\":[]}}' && exit 0\n{script}"
        ),
    );
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
    let report = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[{"severity":"error","message":"This is an unexpected use of the debugger statement.","category":"lint/suspicious/noDebugger","location":{"path":"x","start":{"line":2,"column":1},"end":{"line":2,"column":10}},"advices":[]}],"command":"lint"}"#;
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
    let clean = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[],"command":"lint"}"#;
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

/// A JavaScript project whose one block holds `body`, with biome-format off so only lint judges
/// it, and `config` as its root `biome.json` when given.
fn lint_only(body: &str, config: Option<&str>) -> Project {
    let project = project_with_block(
        "[features.code-blocks.javascript]\n\
         [features.code-blocks.javascript.biome-format]\nformat = \"off\"\n",
        "js",
        body,
    );
    if let Some(config) = config {
        project.write("biome.json", config);
    }
    project
}

/// The kinds of the findings and the block's state when the real biome in `dir` checks `project`.
fn lint_outcome(project: &Project, dir: &Path) -> (Vec<String>, String) {
    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", dir)])
        .json();
    let state = at(&json, "/data/code_blocks/0/state")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    (finding_kinds(&json), state)
}

#[test]
fn lint_a_syntax_error_with_format_off_is_a_parse_error_at_its_doc_line() {
    let project = lint_only("const a = 1;\nlet b = ;", None);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let report = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[{"severity":"error","message":"Expected an expression but instead found `;`.","category":"parse","location":{"path":"x","start":{"line":2,"column":9}},"advices":[]}],"command":"lint"}"#;
    stub_biome_lint(outside.path(), &lint_stub(outside.path(), report, 1));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn lint_the_real_biome_reads_a_syntax_error_with_format_off_as_a_parse_error() {
    let Some(dir) = real_tool_dir(
        "biome",
        "lint_the_real_biome_reads_a_syntax_error_with_format_off_as_a_parse_error",
    ) else {
        return;
    };

    let json = lint_only("const a = 1;\nlet b = ;", None)
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn lint_the_real_biome_fails_a_block_on_any_diagnostic_whatever_its_severity() {
    let Some(dir) = real_tool_dir(
        "biome",
        "lint_the_real_biome_fails_a_block_on_any_diagnostic_whatever_its_severity",
    ) else {
        return;
    };
    let lint = |body: &str| lint_outcome(&lint_only(body, None), &dir);
    let failed = (vec!["code-block-lint".to_owned()], "failed".to_owned());

    assert_eq!(
        lint("a: {\n  f();\n  break a;\n}"),
        failed,
        "a confusing label (a warning) fails"
    );
    let (kinds, state) = lint("{ a: 1 }");
    assert!(
        state == "failed" && !kinds.is_empty() && kinds.iter().all(|k| k == "code-block-lint"),
        "a useless lone block statement (an info) fails: {kinds:?} {state}"
    );
    assert_eq!(lint("debugger;"), failed, "a debugger statement fails");
    assert_eq!(
        lint("f();"),
        (vec![], "passed".to_owned()),
        "a clean block passes"
    );
}

#[test]
fn lint_the_real_biome_lints_a_block_the_projects_includes_leave_out() {
    let Some(dir) = real_tool_dir(
        "biome",
        "lint_the_real_biome_lints_a_block_the_projects_includes_leave_out",
    ) else {
        return;
    };
    for config in [
        r#"{"files":{"includes":["src/**"]}}"#,
        r#"{"linter":{"includes":["src/**"]}}"#,
    ] {
        let outcome = lint_outcome(&lint_only("debugger;", Some(config)), &dir);

        assert_eq!(
            outcome,
            (vec!["code-block-lint".to_owned()], "failed".to_owned()),
            "{config}"
        );
    }
}

#[test]
fn lint_the_real_biome_lints_a_block_in_a_project_with_a_vcs_integration() {
    let Some(dir) = real_tool_dir(
        "biome",
        "lint_the_real_biome_lints_a_block_in_a_project_with_a_vcs_integration",
    ) else {
        return;
    };
    let config = r#"{"vcs":{"enabled":true,"clientKind":"git","useIgnoreFile":true,"root":"."}}"#;
    let lint = |body: &str| lint_outcome(&lint_only(body, Some(config)), &dir);

    assert_eq!(
        lint("f();"),
        (vec![], "passed".to_owned()),
        "a clean block passes"
    );
    assert_eq!(
        lint("debugger;"),
        (vec!["code-block-lint".to_owned()], "failed".to_owned()),
        "a debugger statement fails"
    );
}

#[test]
fn lint_the_real_biome_lints_a_block_larger_than_the_projects_max_size() {
    let Some(dir) = real_tool_dir(
        "biome",
        "lint_the_real_biome_lints_a_block_larger_than_the_projects_max_size",
    ) else {
        return;
    };
    let config = r#"{"files":{"maxSize":1}}"#;
    let lint = |body: &str| lint_outcome(&lint_only(body, Some(config)), &dir);

    assert_eq!(
        lint("f();"),
        (vec![], "passed".to_owned()),
        "a clean block passes"
    );
    assert_eq!(
        lint("debugger;"),
        (vec!["code-block-lint".to_owned()], "failed".to_owned()),
        "a debugger statement fails"
    );
}

#[test]
fn lint_a_run_that_checked_no_file_is_not_run_with_one_warning_never_a_pass() {
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let nothing = r#"{"summary":{"changed":0,"unchanged":0},"diagnostics":[],"command":"lint"}"#;
    stub_biome_lint(outside.path(), &lint_stub(outside.path(), nothing, 0));
    let project = lint_only("f();", None);
    project.write("docs/more.md", "# More\n\n```js\ng();\n```\n");

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
    for block in array_at(&json, "/data/code_blocks") {
        assert!(
            block["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("biome lint checked no file")),
            "{json}"
        );
    }
}

#[test]
fn lint_the_real_biome_names_the_projects_config_in_a_config_error() {
    let Some(dir) = real_tool_dir(
        "biome",
        "lint_the_real_biome_names_the_projects_config_in_a_config_error",
    ) else {
        return;
    };
    let project = lint_only("f();", Some(r#"{"linter":{"bogus":1}}"#));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
    let message = at(&json, "/findings/0/message")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        message.contains("/biome.json:1:12") && !message.contains("supdoc/code-blocks"),
        "the message should name the project's config, not supdoc's private copy: {message}"
    );
}

/// Every line of two or more characters biome lint's frames put around a block.
const FRAME_LINES: [&str; 8] = [
    "async function supdocFragment$() { await Promise.resolve();",
    "const supdocFragment$ = {",
    "};",
    "const supdocFragment$ = <div>",
    "</div>;",
    "abstract class SupdocFragment$ { supdocFrame$() { return this; }",
    "class SupdocFragment$ { supdocFrame$() { return this; }",
    "Promise.resolve()",
];

/// The expression frame's lines, each one character.
const FRAME_CHARS: [&str; 2] = ["(", ")"];

/// Every name only a frame introduces.
const FRAME_NAMES: [&str; 3] = ["supdocFragment$", "SupdocFragment$", "supdocFrame$"];

/// Asserts that no finding in `json` shows frame text in its message, help or location: no
/// frame line of two or more characters or frame-only name anywhere, no line that is exactly a
/// one-character frame line, and no frame token quoted alone, as a tool quotes what it expected
/// (`` `)` ``, `` `;` ``, `` `async` ``).
fn assert_no_frame_text(json: &serde_json::Value) {
    let tokens: Vec<String> = FRAME_LINES
        .iter()
        .chain(&FRAME_CHARS)
        .chain(["}"].iter())
        .flat_map(|line| {
            line.split_whitespace()
                .map(str::to_owned)
                .chain(
                    line.chars()
                        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace() && *c != '_')
                        .map(String::from),
                )
                .collect::<Vec<_>>()
        })
        .map(|token| format!("`{token}`"))
        .collect();
    for finding in array_at(json, "/findings") {
        for field in ["message", "help", "location"] {
            let text = match &finding[field] {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            for frame in FRAME_LINES.iter().chain(&FRAME_NAMES) {
                assert!(!text.contains(frame), "{field} shows `{frame}`: {finding}");
            }
            assert!(
                !text.lines().any(|line| FRAME_CHARS.contains(&line.trim())),
                "{field} shows the expression frame: {finding}"
            );
            for token in &tokens {
                assert!(
                    !text.contains(token.as_str()),
                    "{field} quotes the frame's {token}: {finding}"
                );
            }
        }
    }
}

/// The real biome's `--json -v` check of a `tag` block written with `info` and holding `body`,
/// with `biome_json` as the project's biome config when given, asserting it exits `exit` and
/// that no finding shows frame text. `None` when biome isn't on `PATH`.
fn frame_check_with(
    test: &str,
    biome_json: Option<&str>,
    (tag, info, body): (&str, &str, &str),
    exit: i32,
) -> Option<serde_json::Value> {
    let dir = real_tool_dir("biome", test)?;
    let project = project_with_block(
        "[features.code-blocks.typescript]\n[features.code-blocks.javascript]\n",
        &format!("{tag} {info}"),
        body,
    );
    if let Some(config) = biome_json {
        project.write("biome.json", config);
    }
    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", &dir)])
        .assert_exit(exit)
        .json();
    assert_no_frame_text(&json);
    Some(json)
}

/// [`frame_check_with`] under biome's default rules.
fn frame_check(
    test: &str,
    tag: &str,
    info: &str,
    body: &str,
    exit: i32,
) -> Option<serde_json::Value> {
    frame_check_with(test, None, (tag, info, body), exit)
}

/// The info every lint frame test writes after its tag, so it checks the lint step alone.
const LINT_ONLY: &str = "format(off: these tests check the lint step)";

/// The codes and lines of `json`'s findings, in order.
fn kinds_and_lines(json: &serde_json::Value) -> Vec<(String, u64)> {
    array_at(json, "/findings")
        .iter()
        .map(|finding| {
            (
                finding["kind"].as_str().unwrap_or_default().to_owned(),
                finding
                    .pointer("/location/line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            )
        })
        .collect()
}

/// Asserts that `json`'s only block passed.
fn assert_passed(json: &serde_json::Value, what: &str) {
    assert_eq!(
        at(json, "/data/summary/code_blocks/passed"),
        1,
        "{what}: {json}"
    );
}

#[test]
fn frame_lint_entries_draw_no_confusing_label_that_the_bare_block_draws() {
    let test = "frame_lint_entries_draw_no_confusing_label_that_the_bare_block_draws";
    let Some(framed) = frame_check(
        test,
        "ts",
        &format!("fragment(entries) {LINT_ONLY}"),
        "visible: true,\nlabel: \"Save\"",
        0,
    ) else {
        return;
    };
    assert_passed(&framed, "entries");

    let bare = frame_check(test, "ts", LINT_ONLY, "visible: true", 1)
        .expect("biome was on PATH a moment ago");
    assert!(bare.to_string().contains("noConfusingLabels"), "{bare}");
}

#[test]
fn frame_lint_children_draw_use_button_type_at_their_doc_lines() {
    let Some(json) = frame_check(
        "frame_lint_children_draw_use_button_type_at_their_doc_lines",
        "tsx",
        &format!("fragment(children) {LINT_ONLY}"),
        "<button>Save</button>\n<button>Cancel</button>",
        1,
    ) else {
        return;
    };

    let found = kinds_and_lines(&json);
    assert_eq!(
        found.iter().map(|(_, line)| *line).collect::<Vec<_>>(),
        [4, 5],
        "{json}"
    );
    assert!(
        array_at(&json, "/findings")
            .iter()
            .all(|finding| finding.to_string().contains("useButtonType")),
        "{json}"
    );
}

#[test]
fn frame_lint_one_child_is_not_a_useless_fragment() {
    let Some(json) = frame_check(
        "frame_lint_one_child_is_not_a_useless_fragment",
        "jsx",
        &format!("fragment(children) {LINT_ONLY}"),
        "<button type=\"button\">Save</button>",
        0,
    ) else {
        return;
    };

    assert_passed(&json, "one child");
}

#[test]
fn frame_lint_the_authors_own_useless_fragment_is_flagged_at_its_line() {
    let Some(json) = frame_check(
        "frame_lint_the_authors_own_useless_fragment_is_flagged_at_its_line",
        "tsx",
        &format!("fragment(children) {LINT_ONLY}"),
        "<a href=\"/x\">a</a>\n<><span /></>",
        1,
    ) else {
        return;
    };

    let useless: Vec<u64> = array_at(&json, "/findings")
        .iter()
        .filter(|finding| finding.to_string().contains("noUselessFragments"))
        .filter_map(|finding| {
            finding
                .pointer("/location/line")
                .and_then(serde_json::Value::as_u64)
        })
        .collect();
    assert_eq!(useless, [5], "{json}");
}

#[test]
fn frame_lint_children_in_a_ts_block_are_unsupported() {
    let Some(json) = frame_check(
        "frame_lint_children_in_a_ts_block_are_unsupported",
        "ts",
        &format!("fragment(children) {LINT_ONLY}"),
        "<button>Save</button>",
        0,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-fragment-unsupported".to_owned(), 3)],
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{json}"
    );
}

#[test]
fn frame_lint_statements_members_and_expressions_pass_as_written() {
    let test = "frame_lint_statements_members_and_expressions_pass_as_written";
    for (tag, kind, body) in [
        ("ts", "statements", "const total = 1 + 2;\nreturn total;"),
        (
            "ts",
            "statements",
            "const data = await fetch(\"/api\");\nreturn data;",
        ),
        ("ts", "members", "resolve(): number {\n\treturn 1;\n}"),
        ("ts", "members", "static create(): number {\n\treturn 1;\n}"),
        ("ts", "expression", "{ type: \"spring\", stiffness: 100 }"),
        ("js", "expression", "items.map((item) => item.id)"),
        ("ts", "expression", "a = 1"),
    ] {
        let Some(json) = frame_check(test, tag, &format!("fragment({kind}) {LINT_ONLY}"), body, 0)
        else {
            return;
        };
        assert_passed(&json, &format!("{tag} {kind} {body:?}"));
    }
}

#[test]
fn frame_lint_finds_a_rule_inside_the_block_at_its_doc_line() {
    let Some(json) = frame_check(
        "frame_lint_finds_a_rule_inside_the_block_at_its_doc_line",
        "ts",
        &format!("fragment(statements) {LINT_ONLY}"),
        "const total = 1;\ndebugger;\nreturn total;",
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-lint".to_owned(), 5)],
        "{json}"
    );
    assert!(
        at(&json, "/findings/0").to_string().contains("noDebugger"),
        "{json}"
    );
}

#[test]
fn frame_lint_a_block_that_isnt_its_kind_is_one_parse_error_without_biomes_message() {
    let test = "frame_lint_a_block_that_isnt_its_kind_is_one_parse_error_without_biomes_message";
    for (kind, body, line) in [
        ("expression", "foo();", 4),
        ("entries", "visible: true;", 4),
        ("statements", "const a = 1;\nyield a;", 5),
    ] {
        let Some(json) = frame_check(
            test,
            "ts",
            &format!("fragment({kind}) {LINT_ONLY}"),
            body,
            1,
        ) else {
            return;
        };
        assert_eq!(
            kinds_and_lines(&json),
            [("code-block-parse".to_owned(), line)],
            "{kind} {body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/message"),
            format!("`biome-lint` couldn't parse the block as `{kind}`").as_str(),
            "{json}"
        );
    }
}

#[test]
fn frame_lint_a_rule_on_a_frame_line_is_reported_at_the_attributes_in_supdocs_words() {
    let Some(json) = frame_check_with(
        "frame_lint_a_rule_on_a_frame_line_is_reported_at_the_attributes_in_supdocs_words",
        Some("{\"linter\":{\"rules\":{\"nursery\":{\"useExplicitType\":\"on\"}}}}"),
        (
            "tsx",
            &format!("fragment(children) {LINT_ONLY}"),
            "<button type=\"button\">Save</button>",
        ),
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-lint".to_owned(), 3)],
        "{json}"
    );
    assert_eq!(
        at(&json, "/findings/0/message"),
        "`biome-lint` reports `nursery/useExplicitType` where the block meets the frame supdoc \
         adds around a children fragment",
        "{json}"
    );
}

/// A project biome config that turns `suspicious/useAwait` on.
const USE_AWAIT_ON: &str = "{\"linter\":{\"rules\":{\"suspicious\":{\"useAwait\":\"on\"}}}}";

#[test]
fn frame_lint_the_authors_nested_static_only_class_is_flagged_at_its_line() {
    let Some(json) = frame_check(
        "frame_lint_the_authors_nested_static_only_class_is_flagged_at_its_line",
        "ts",
        &format!("fragment(members) {LINT_ONLY}"),
        "static Inner = class {\n\tstatic x = 1;\n};",
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-lint".to_owned(), 4)],
        "{json}"
    );
    assert!(
        at(&json, "/findings/0")
            .to_string()
            .contains("noStaticOnlyClass"),
        "{json}"
    );
}

#[test]
fn frame_lint_use_await_flags_the_authors_function_never_the_frame() {
    let test = "frame_lint_use_await_flags_the_authors_function_never_the_frame";
    let info = format!("fragment(statements) {LINT_ONLY}");
    let Some(flagged) = frame_check_with(
        test,
        Some(USE_AWAIT_ON),
        (
            "ts",
            &info,
            "async function load() {\n\treturn 1;\n}\nreturn load;",
        ),
        1,
    ) else {
        return;
    };
    assert_eq!(
        kinds_and_lines(&flagged),
        [("code-block-lint".to_owned(), 4)],
        "{flagged}"
    );
    assert!(
        at(&flagged, "/findings/0").to_string().contains("useAwait"),
        "{flagged}"
    );

    let quiet = frame_check_with(
        test,
        Some(USE_AWAIT_ON),
        ("ts", &info, "const total = 1;\nreturn total;"),
        0,
    )
    .expect("biome was on PATH a moment ago");
    assert_passed(&quiet, "statements with no await of its own");
}

#[test]
fn frame_lint_a_run_that_didnt_parse_as_its_kind_gives_only_the_parse_error() {
    let test = "frame_lint_a_run_that_didnt_parse_as_its_kind_gives_only_the_parse_error";
    for (tag, kind, body, line) in [
        ("ts", "members", "constructor() {\n\tsuper();\n}", 5),
        ("tsx", "children", "<a href=\"x\">a</a>\n<b>", 5),
        ("ts", "members", "x = 1;\nfoo( {", 3),
    ] {
        let Some(json) = frame_check(test, tag, &format!("fragment({kind}) {LINT_ONLY}"), body, 1)
        else {
            return;
        };
        assert_eq!(
            kinds_and_lines(&json),
            [("code-block-parse".to_owned(), line)],
            "{kind} {body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/message"),
            format!("`biome-lint` couldn't parse the block as `{kind}`").as_str(),
            "{json}"
        );
        let help = at(&json, "/findings/0/help").as_str().unwrap_or_default();
        assert!(
            !help.contains("name the kind") && help.contains("write the lines around the code"),
            "the help should offer an exit that works for every kind: {json}"
        );
    }
}

#[test]
fn frame_lint_a_leading_directive_in_statements_is_not_read_as_a_directive() {
    let Some(json) = frame_check(
        "frame_lint_a_leading_directive_in_statements_is_not_read_as_a_directive",
        "ts",
        &format!("fragment(statements) {LINT_ONLY}"),
        "\"use strict\";\nconst a = 1;\nconsole.log(a);",
        0,
    ) else {
        return;
    };

    assert_passed(&json, "a statements block opening with \"use strict\"");
}

#[test]
fn frame_lint_an_abstract_member_passes_in_a_ts_members_block() {
    let test = "frame_lint_an_abstract_member_passes_in_a_ts_members_block";
    let info = format!("fragment(members) {LINT_ONLY}");
    let Some(json) = frame_check(
        test,
        "ts",
        &info,
        "abstract area(): number;\ndescribe(): string {\n\treturn `${this.area()}`;\n}",
        0,
    ) else {
        return;
    };
    assert_passed(&json, "a ts members block with an abstract member");

    let flagged = frame_check(
        test,
        "ts",
        &info,
        "abstract area(): number;\ndescribe(): string {\n\tdebugger;\n\treturn `${this.area()}`;\n}",
        1,
    )
    .expect("biome was on PATH a moment ago");
    assert_eq!(
        kinds_and_lines(&flagged),
        [("code-block-lint".to_owned(), 6)],
        "{flagged}"
    );
    assert!(
        at(&flagged, "/findings/0")
            .to_string()
            .contains("noDebugger"),
        "{flagged}"
    );
}

#[test]
fn frame_lint_an_abstract_member_in_a_js_members_block_is_one_parse_error() {
    let Some(json) = frame_check(
        "frame_lint_an_abstract_member_in_a_js_members_block_is_one_parse_error",
        "js",
        &format!("fragment(members) {LINT_ONLY}"),
        "abstract area();",
        1,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(
        at(&json, "/findings/0/message"),
        "`biome-lint` couldn't parse the block as `members`",
        "{json}"
    );
}

#[test]
fn frame_lint_an_unframed_syntax_rule_is_a_parse_error_at_its_line() {
    let Some(json) = frame_check(
        "frame_lint_an_unframed_syntax_rule_is_a_parse_error_at_its_line",
        "ts",
        LINT_ONLY,
        "class A {\n\t#a = 1;\n\t#a = 2;\n}",
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-parse".to_owned(), 6)],
        "{json}"
    );
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("#a")),
        "{json}"
    );
    let help = at(&json, "/findings/0/help").as_str().unwrap_or_default();
    assert!(
        help.contains("lint(off") && !help.contains("format(off"),
        "the help should name the lint step's off switch: {json}"
    );
}

/// A doc holding one `tag fragment(<kind>)` block of `body` (each line ended by LF), with prose
/// before and after it that a fix must leave byte for byte.
fn format_frame_doc(tag: &str, kind: &str, body: &str) -> String {
    format!("# Blocks\n\nBefore `x`.\n\n```{tag} fragment({kind})\n{body}```\n\nAfter `y`.\n")
}

/// A recorded `basic` project with biome on for TypeScript and JavaScript, `doc` at
/// `docs/blocks.md` and `biome_json` as the project's biome config when given.
fn format_frame_project(doc: &str, biome_json: Option<&str>) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.typescript]\n[features.code-blocks.javascript]\n",
    );
    project.write("docs/blocks.md", doc);
    if let Some(config) = biome_json {
        project.write("biome.json", config);
    }
    project
}

/// Asserts that no finding's fix in `json` writes a name only a frame introduces or the
/// statements frame's own `await`.
fn assert_no_frame_in_fixes(json: &serde_json::Value) {
    for finding in array_at(json, "/findings") {
        let fix = finding["fix"].to_string();
        for frame in FRAME_NAMES.iter().chain(&["Promise.resolve"]) {
            assert!(!fix.contains(frame), "the fix writes `{frame}`: {finding}");
        }
    }
}

/// Checks `formatted` and `unformatted`, each the body of a `tag fragment(<kind>)` block, with
/// the real biome in `dir` under `biome_json`: the formatted one passes with both steps run, the
/// unformatted one fails `code-block-unformatted` alone, with a fix, and `fix --yes` turns its
/// doc into the formatted one's, byte for byte. No finding shows the frame.
fn assert_biome_frames(
    dir: &Path,
    biome_json: Option<&str>,
    block: (&str, &str),
    formatted: &str,
    unformatted: &str,
) {
    assert_biome_frames_in(dir, biome_json, block, (formatted, unformatted), false);
}

/// [`assert_biome_frames`] with each doc's lines ended by CRLF when `crlf`.
fn assert_biome_frames_in(
    dir: &Path,
    biome_json: Option<&str>,
    (tag, kind): (&str, &str),
    (formatted, unformatted): (&str, &str),
    crlf: bool,
) {
    let what = format!("{tag} {kind} {biome_json:?} crlf={crlf} {formatted:?}");
    let shape = |doc: String| {
        if crlf { doc.replace('\n', "\r\n") } else { doc }
    };
    let passing = format_frame_project(&shape(format_frame_doc(tag, kind, formatted)), biome_json);
    let failing =
        format_frame_project(&shape(format_frame_doc(tag, kind, unformatted)), biome_json);

    let passes = passing
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", dir)])
        .assert_exit(0)
        .json();
    let fails = failing
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", dir)])
        .assert_exit(1)
        .json();
    let _fix = failing
        .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", dir)])
        .assert_exit(0);

    assert_passed(&passes, &what);
    for count in ["format_not_run", "lint_not_run"] {
        assert_eq!(
            at(&passes, &format!("/data/summary/code_blocks/{count}")),
            0,
            "{what}: both steps should run: {passes}"
        );
    }
    assert_eq!(
        kinds_and_lines(&fails),
        [("code-block-unformatted".to_owned(), 5)],
        "{what}: {fails}"
    );
    assert!(
        at(&fails, "/findings/0/fix/replacement").is_string(),
        "{what}: {fails}"
    );
    assert_no_frame_text(&passes);
    assert_no_frame_text(&fails);
    assert_no_frame_in_fixes(&fails);
    assert_eq!(
        failing.read("docs/blocks.md"),
        shape(format_frame_doc(tag, kind, formatted)),
        "{what}: the fix rewrites only the block's own lines"
    );
}

#[test]
fn frame_format_entries_get_biomes_trailing_comma_on_the_authors_lines_only() {
    let Some(dir) = real_tool_dir(
        "biome",
        "frame_format_entries_get_biomes_trailing_comma_on_the_authors_lines_only",
    ) else {
        return;
    };

    assert_biome_frames(
        &dir,
        None,
        ("ts", "entries"),
        "visible: true,\nlabel: \"Save\",\n",
        "visible: true,\nlabel: \"Save\"\n",
    );
}

#[test]
fn frame_format_children_statements_and_members_pass_formatted_and_fix_unformatted() {
    let test = "frame_format_children_statements_and_members_pass_formatted_and_fix_unformatted";
    let Some(dir) = real_tool_dir("biome", test) else {
        return;
    };
    let cases = [
        (
            ("tsx", "children"),
            "<button type=\"button\">Save</button>\n<button type=\"button\">Cancel</button>\n",
            "<button   type=\"button\">Save</button>\n<button type=\"button\">Cancel</button>\n",
        ),
        (
            ("jsx", "children"),
            "<li>\n\t<a href=\"/docs\">Docs</a>\n</li>\n<li>Blog</li>\n",
            "<li><a href=\"/docs\">Docs</a></li>\n<li>Blog</li>\n",
        ),
        (
            ("ts", "statements"),
            "const data = await fetch(\"/api\");\nreturn data;\n",
            "const data = await fetch('/api')\nreturn data\n",
        ),
        (
            ("ts", "members"),
            "abstract area(): number;\n\nlabel(): string {\n\treturn \"shape\";\n}\n",
            "abstract area(): number;\n\nlabel(): string { return \"shape\" }\n",
        ),
        (
            ("js", "members"),
            "label() {\n\treturn \"shape\";\n}\n",
            "label() { return 'shape' }\n",
        ),
    ];

    for (block, formatted, unformatted) in cases {
        assert_biome_frames(&dir, None, block, formatted, unformatted);
    }
}

#[test]
fn frame_format_keeps_the_projects_indent_and_line_width_inside_the_frame() {
    let test = "frame_format_keeps_the_projects_indent_and_line_width_inside_the_frame";
    let Some(dir) = real_tool_dir("biome", test) else {
        return;
    };
    let spaces = r#"{"formatter":{"indentStyle":"space","indentWidth":4,"lineWidth":40}}"#;
    // 40 columns: biome keeps it on one line at a width of 40, and breaks it at 39.
    let fits = "items: [\"alpha\", \"beta\", \"gamma\", \"de\"],\n";
    let broken = "items: [\n    \"alpha\",\n    \"beta\",\n    \"gamma\",\n    \"delta\",\n],\n";

    assert_biome_frames(
        &dir,
        Some(spaces),
        ("ts", "entries"),
        fits,
        "items: [\"alpha\",\"beta\",\"gamma\",\"de\"],\n",
    );
    assert_biome_frames(
        &dir,
        Some(spaces),
        ("ts", "entries"),
        broken,
        "items: [\"alpha\", \"beta\", \"gamma\", \"delta\"],\n",
    );
    assert_biome_frames(
        &dir,
        Some(spaces),
        ("tsx", "children"),
        "<p className=\"lead\">Read the guide.</p>\n<p>Then try the demo below.</p>\n",
        "<p className=\"lead\">Read the guide.</p>\n<p>Then  try the demo below.</p>\n",
    );
}

/// The real biome's `--json -v` check of a `tag fragment(<kind>)` block of `body` in a
/// [`format_frame_project`], asserting it exits `exit` and that no finding shows frame text.
/// `None` when biome isn't on `PATH`.
fn format_frame_check(
    test: &str,
    biome_json: Option<&str>,
    (tag, kind, body): (&str, &str, &str),
    exit: i32,
) -> Option<serde_json::Value> {
    let dir = real_tool_dir("biome", test)?;
    let json = format_frame_project(&format_frame_doc(tag, kind, body), biome_json)
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", &dir)])
        .assert_exit(exit)
        .json();
    assert_no_frame_text(&json);
    assert_no_frame_in_fixes(&json);
    Some(json)
}

#[test]
fn frame_format_a_template_literal_in_children_keeps_every_byte_of_its_lines() {
    let test = "frame_format_a_template_literal_in_children_keeps_every_byte_of_its_lines";
    let Some(dir) = real_tool_dir("biome", test) else {
        return;
    };
    let spaces = r#"{"formatter":{"indentStyle":"space","indentWidth":2}}"#;

    for tag in ["tsx", "jsx"] {
        for config in [None, Some(spaces)] {
            for crlf in [false, true] {
                for continuation in ["\tline two", "  line two", "line two"] {
                    let formatted = format!("<p>{{`line one\n{continuation}`}}</p>\n");
                    let unformatted = format!("<p  >{{`line one\n{continuation}`}}</p>\n");
                    assert_biome_frames_in(
                        &dir,
                        config,
                        (tag, "children"),
                        (&formatted, &unformatted),
                        crlf,
                    );
                }
            }
        }
    }
}

#[test]
fn frame_format_a_block_comment_and_an_attribute_string_in_children_come_back_exact() {
    let test = "frame_format_a_block_comment_and_an_attribute_string_in_children_come_back_exact";
    let Some(dir) = real_tool_dir("biome", test) else {
        return;
    };

    assert_biome_frames(
        &dir,
        None,
        ("tsx", "children"),
        "{/* first\n   second */}\n<p\n\ttitle=\"a\n\tb\"\n>\n\tx\n</p>\n",
        "{/* first\n   second */}\n<p   title=\"a\n\tb\">x</p>\n",
    );
}

#[test]
fn frame_format_children_with_no_element_pass_as_written() {
    let test = "frame_format_children_with_no_element_pass_as_written";
    for body in ["", "\n", "Hello world\n", "{label}\n", "{props.children}\n"] {
        let Some(json) = format_frame_check(test, None, ("tsx", "children", body), 0) else {
            return;
        };
        assert_passed(&json, &format!("{body:?}"));
    }
    let json = format_frame_check(test, None, ("jsx", "children", "{ label }\n"), 1)
        .expect("biome was on PATH a moment ago");
    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-unformatted".to_owned(), 5)],
        "{json}"
    );
    assert_eq!(
        at(&json, "/findings/0/fix/replacement"),
        "{label}",
        "{json}"
    );
}

#[test]
fn frame_format_a_one_line_children_block_with_a_trailing_blank_line_is_fixed() {
    let Some(dir) = real_tool_dir(
        "biome",
        "frame_format_a_one_line_children_block_with_a_trailing_blank_line_is_fixed",
    ) else {
        return;
    };

    assert_biome_frames(&dir, None, ("tsx", "children"), "{label}\n", "{label}\n\n");
}

#[test]
fn frame_format_whitespace_at_a_one_line_childrens_edge_fails_closed_without_a_fix() {
    let test = "frame_format_whitespace_at_a_one_line_childrens_edge_fails_closed_without_a_fix";
    for body in ["Hello{\" \"}\n", "{\" \"}\n"] {
        let Some(json) = format_frame_check(test, None, ("tsx", "children", body), 1) else {
            return;
        };
        assert_eq!(
            kinds_and_lines(&json),
            [("code-block-tool-failed".to_owned(), 5)],
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/message"),
            "supdoc couldn't compare `biome-format`'s output with this children block: \
             `biome-format` printed it on one line with whitespace at its edge",
            "{body:?}: {json}"
        );
        assert!(
            json.pointer("/findings/0/fix").is_none(),
            "{body:?}: no fix: {json}"
        );
    }
}

#[test]
fn frame_format_a_syntax_error_in_each_kind_is_one_parse_error_in_supdocs_words() {
    let test = "frame_format_a_syntax_error_in_each_kind_is_one_parse_error_in_supdocs_words";
    for (tag, kind, body, line) in [
        ("ts", "entries", "visible: true;\nlabel: 1,\n", 6),
        (
            "ts",
            "statements",
            "const a = 1;\nconst b = ;\nreturn a;\n",
            7,
        ),
        ("ts", "members", "label() {\n\treturn 1 +;\n}\n", 7),
        // biome reads `</p` on into the next line and reports the error there.
        (
            "tsx",
            "children",
            "<p>one</p>\n<p>two</p\n<p>three</p>\n",
            8,
        ),
    ] {
        let Some(json) = format_frame_check(test, None, (tag, kind, body), 1) else {
            return;
        };
        assert_eq!(
            kinds_and_lines(&json),
            [("code-block-parse".to_owned(), line)],
            "{kind}: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/message"),
            format!("`biome-format` couldn't parse the block as `{kind}`").as_str(),
            "{kind}: never biome's own message: {json}"
        );
    }
}

#[test]
fn frame_format_a_line_width_override_is_named_in_the_not_run_warning() {
    let Some(json) = format_frame_check(
        "frame_format_a_line_width_override_is_named_in_the_not_run_warning",
        Some(r#"{"overrides":[{"includes":["**"],"javascript":{"formatter":{"lineWidth":40}}}]}"#),
        ("ts", "entries", "visible: true,\n"),
        0,
    ) else {
        return;
    };

    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_not_run"),
        1,
        "{json}"
    );
    let message = at(&json, "/findings/0/message")
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("line width") && !message.contains("indent the formatter uses"),
        "{json}"
    );
}

#[test]
fn frame_format_a_block_that_isnt_its_kind_is_one_parse_error_at_the_attributes() {
    // biome reports the unclosed call on the frame's closing `};`.
    let Some(json) = frame_check(
        "frame_format_a_block_that_isnt_its_kind_is_one_parse_error_at_the_attributes",
        "ts",
        "fragment(entries)",
        "label: render(",
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-parse".to_owned(), 3)],
        "{json}"
    );
    assert_eq!(
        at(&json, "/findings/0/message"),
        "`biome-format` couldn't parse the block as `entries`",
        "{json}"
    );
}

/// A recorded `basic` project with a `typescript` table and `source` at `src/blocks.ts`, and a
/// directory holding a stub `biome` whose lint passes and whose format turns every block into
/// `const a = 1;`.
fn jsdoc_project(source: &str) -> (Project, tempfile::TempDir) {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.typescript]\n",
    );
    project.write("src/blocks.ts", source);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), "/bin/cat >/dev/null\necho 'const a = 1;'");
    (project, outside)
}

/// A `/** */` comment on a method holding one unformatted `ts` block.
const JSDOC_SOURCE: &str = "export class C {\n  /**\n   * Adds.\n   *\n   * ```ts\n   * const   a = 1\n   * ```\n   */\n  add() {}\n}\n";

#[test]
fn jsdoc_block_is_checked_at_its_file_line() {
    let (project, outside) = jsdoc_project(JSDOC_SOURCE);

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn jsdoc_fix_keeps_the_gutter() {
    let (project, outside) = jsdoc_project(JSDOC_SOURCE);
    let path = [("PATH", outside.path())];

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);

    assert_eq!(
        project.read("src/blocks.ts"),
        JSDOC_SOURCE.replace("const   a = 1", "const a = 1;"),
        "the fix should rewrite the block's line behind its `   * ` gutter"
    );
}

#[test]
fn jsdoc_block_and_line_comments_are_not_checked() {
    let source = "/* ```ts\nconst   a = 1\n``` */\n// ```ts\n// const   a = 1\n// ```\nexport const b = 2;\n";
    let (project, outside) = jsdoc_project(source);

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

/// A `/** */` comment with no `*` gutter on a method, indented 4 spaces, holding one `ts` block of a
/// two-level object literal indented with tabs, as biome formats it, whose innermost line is
/// `inner`.
fn gutterless_source(inner: &str) -> String {
    format!(
        "export class C {{\n  /**\n    Makes.\n\n    ```ts\n    const a = {{\n    \tb: {{\n    \t\t{inner}\n    \t}},\n    }};\n    ```\n  */\n  make() {{}}\n}}\n"
    )
}

#[test]
fn jsdoc_gutterless_fix_keeps_the_comments_and_the_codes_indentation() {
    let (project, outside) = jsdoc_project(&gutterless_source("c:1,"));
    stub_biome(
        outside.path(),
        "/bin/cat >/dev/null\nprintf 'const a = {\\n\\tb: {\\n\\t\\tc: 1,\\n\\t},\\n};\\n'",
    );
    let path = [("PATH", outside.path())];

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);

    assert_eq!(
        project.read("src/blocks.ts"),
        gutterless_source("c: 1,"),
        "the fix should rewrite the block behind the comment's 4-space prefix, keeping the code's tabs"
    );
}

#[test]
fn jsdoc_the_real_biome_checks_and_fixes_a_gutterless_comments_indented_block() {
    let Some(dir) = real_tool_dir(
        "biome",
        "jsdoc_the_real_biome_checks_and_fixes_a_gutterless_comments_indented_block",
    ) else {
        return;
    };
    let path = [("PATH", dir.as_path())];
    let (formatted, _) = jsdoc_project(&gutterless_source("c: 1,"));
    let (unformatted, _) = jsdoc_project(&gutterless_source("c:1,"));

    let passes = formatted
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(0)
        .json();
    let fails = unformatted
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(1)
        .json();
    let _fix = unformatted
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    assert_eq!(at(&fails, "/findings/0/location/line"), 5, "{fails}");
    assert_eq!(
        unformatted.read("src/blocks.ts"),
        gutterless_source("c: 1,"),
        "biome's fix should land behind the comment's prefix with the code's indent kept"
    );
}

/// A `/** */` comment on a method with an unfenced `@example` holding one unformatted line,
/// followed by a `@returns` tag.
const EXAMPLE_SOURCE: &str = "export class C {\n  /**\n   * Adds.\n   * @example\n   * const   a = 1\n   * @returns nothing\n   */\n  add() {}\n}\n";

#[test]
fn example_without_a_fence_is_checked_at_its_tag_line() {
    let (project, outside) = jsdoc_project(EXAMPLE_SOURCE);

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
}

#[test]
fn example_fix_keeps_the_gutter_and_the_next_tag() {
    let (project, outside) = jsdoc_project(EXAMPLE_SOURCE);
    let path = [("PATH", outside.path())];

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);

    assert_eq!(
        project.read("src/blocks.ts"),
        EXAMPLE_SOURCE.replace("const   a = 1", "const a = 1;"),
        "the fix should rewrite the example's line behind its `   * ` gutter and keep `@returns`"
    );
}

/// The kinds of a `check --json` on a project with `table` under `[features.code-blocks]` and
/// `source` at `src/<name>`, whose stub `biome` formats only a block given the virtual extension
/// `extension`, turning it into `const a = 1;`, and fails on any other.
fn example_kinds_in(name: &str, table: &str, extension: &str) -> Vec<String> {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.{table}]\n"),
    );
    project.write(
        &format!("src/{name}"),
        "/**\n * @example\n * const   a = <A />\n */\nexport function f() {}\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(
        outside.path(),
        &format!(
            "case \"$3\" in *.{extension}) ;; *) exit 4;; esac\n/bin/cat >/dev/null\necho 'const a = 1;'"
        ),
    );
    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();
    finding_kinds(&json)
}

#[test]
fn example_in_a_js_file_is_js_and_in_a_tsx_file_is_tsx() {
    for (name, table, extension) in [
        ("blocks.js", "javascript", "js"),
        ("view.tsx", "typescript", "tsx"),
    ] {
        assert_eq!(
            example_kinds_in(name, table, extension),
            ["code-block-unformatted"],
            "{name}: biome should be given the block as `.{extension}`"
        );
    }
}

#[test]
fn example_fix_that_would_start_a_block_tag_or_close_the_comment_is_never_written() {
    for output in ["@sealed\\nclass A {}", "const a = 1; */"] {
        let (project, outside) = jsdoc_project(EXAMPLE_SOURCE);
        stub_biome(
            outside.path(),
            &format!("/bin/cat >/dev/null\nprintf '{output}\\n'"),
        );
        let path = [("PATH", outside.path())];

        let _fix = project
            .supdoc_in_with(".", &["fix", "--yes"], &path)
            .assert_exit(0);
        let json = project
            .supdoc_in_with(".", &["check", "--json"], &path)
            .assert_exit(1)
            .json();

        assert_eq!(project.read("src/blocks.ts"), EXAMPLE_SOURCE, "{output}");
        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted", "code-block-fix-unsafe"],
            "{output}: {json}"
        );
        assert_eq!(
            at(&json, "/findings/1/message"),
            "`biome-format`'s output for the block has a line that starts with `@` or holds `*/`, which would end its `@example` section or its comment, so `fix` won't write it",
            "{output}: {json}"
        );
        assert!(
            array_at(&json, "/findings")
                .iter()
                .all(|finding| finding.get("fix").is_none()),
            "{output}: no finding should offer the fix: {json}"
        );
    }
}

/// The config that selects prettier for `table`'s blocks, with biome lint's step off so prettier
/// is the block's one tool.
fn prettier_tables(table: &str) -> String {
    format!(
        "[features.code-blocks.{table}]\nformatter = \"prettier\"\n\
         [features.code-blocks.{table}.biome-lint]\nlint = \"off\"\n"
    )
}

/// A project whose one `ts` block, holding `body`, prettier formats.
fn prettier_project(body: &str) -> Project {
    project_with_block(&prettier_tables("typescript"), "ts", body)
}

/// Writes a `prettier` stub in `dir` that prints `3.9.10` for `--version`, and for any other call
/// appends its working directory and arguments to `<dir>/prettier-runs`, one line per run, then
/// runs `script`.
fn prettier_stub(dir: &Path, script: &str) {
    prettier_stub_versioned(dir, "echo 3.9.10", script);
}

/// [`prettier_stub`], running `version` for `--version` instead.
fn prettier_stub_versioned(dir: &Path, version: &str, script: &str) {
    let record = dir.join("prettier-runs");
    let path = dir.join("prettier");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then\n{version}\nexit 0\nfi\n\
             echo \"$(pwd -P) $*\" >> '{}'\n{script}\n",
            record.display()
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// The lines `prettier_stub` in `dir` recorded: one per run.
fn prettier_runs(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("prettier-runs"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Runs a trusted `check --json` on `project` with `path` as the only `PATH` entry and its cache
/// in `cache`, asserting it exits `exit`.
fn trusted_prettier_check(
    project: &Project,
    path: &Path,
    cache: &Path,
    exit: i32,
) -> serde_json::Value {
    project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[
                ("PATH", path),
                ("SUPDOC_TRUST", Path::new("1")),
                ("XDG_CACHE_HOME", cache),
            ],
        )
        .assert_exit(exit)
        .json()
}

#[test]
fn prettier_runs_from_the_root_with_its_overrides_and_spawns_again_on_a_second_run() {
    let project = prettier_project("const a = 1;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(outside.path(), "/bin/cat");

    let first = trusted_prettier_check(&project, outside.path(), cache.path(), 0);
    let second = trusted_prettier_check(&project, outside.path(), cache.path(), 0);

    let root = project
        .path(".")
        .canonicalize()
        .expect("the project root should resolve");
    let runs = prettier_runs(outside.path());
    assert_eq!(
        runs.len(),
        2,
        "prettier is never cached, so each run spawns it: {runs:?}"
    );
    for run in &runs {
        let (cwd, args) = run
            .split_once(' ')
            .expect("the record names a cwd and args");
        assert_eq!(Path::new(cwd), root, "prettier runs from the root: {run}");
        let args: Vec<&str> = args.split(' ').collect();
        let [
            "--ignore-path=",
            "--with-node-modules",
            "--no-require-pragma",
            "--no-check-ignore-pragma",
            "--no-insert-pragma",
            "--range-start=0",
            "--range-end=Infinity",
            "--parser=typescript",
            "--stdin-filepath",
            path,
        ] = args.as_slice()
        else {
            panic!("prettier should get its overrides and the block's path: {run}");
        };
        assert!(path.ends_with("/docs/blocks.md.1.ts"), "{run}");
    }
    for json in [first, second] {
        assert!(finding_kinds(&json).is_empty(), "{json}");
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    }
}

#[test]
fn prettier_in_the_projects_node_modules_runs_before_one_on_path() {
    let project = project_with_block(&prettier_tables("javascript"), "js", "const a = 1;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let bin = project.path("node_modules/.bin");
    std::fs::create_dir_all(&bin).expect("the node_modules bin directory should be creatable");
    prettier_stub(&bin, "/bin/cat");
    prettier_stub(outside.path(), "exit 9");

    let json = trusted_prettier_check(&project, outside.path(), cache.path(), 0);

    let runs = prettier_runs(&bin);
    assert_eq!(runs.len(), 1, "the root's prettier runs: {json}");
    assert!(
        runs.first()
            .is_some_and(|run| run.ends_with("/docs/blocks.md.1.js")),
        "a js block gets a .js path: {runs:?}"
    );
    assert!(
        prettier_runs(outside.path()).is_empty(),
        "the prettier of the same name on PATH never runs: {json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn prettier_ignore_in_the_block_is_format_not_run_naming_format_off() {
    let project = prettier_project("const a = 1;");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```ts\n// prettier-ignore\nconst  a = 1\n```\n\n\
         ```ts\nconst b = [/* prettier-ignore */ 1,2];\n```\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(outside.path(), "/bin/cat");

    let json = trusted_prettier_check(&project, outside.path(), cache.path(), 0);

    assert!(
        prettier_runs(outside.path()).is_empty(),
        "prettier never starts: {json}"
    );
    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/findings/0/severity"), "warning", "{json}");
    let help = at(&json, "/findings/0/help");
    assert!(
        help.as_str()
            .is_some_and(|help| help.contains("format(off: <reason>)")),
        "{json}"
    );
    for block in 0..2 {
        assert_eq!(
            at(&json, &format!("/data/code_blocks/{block}/format")),
            "not_run",
            "block {block}: {json}"
        );
        let reason = at(&json, &format!("/data/code_blocks/{block}/reason"));
        assert!(
            reason
                .as_str()
                .is_some_and(|reason| reason.contains("format(off: <reason>)")),
            "block {block}: {json}"
        );
    }
}

#[test]
fn prettier_untrusted_is_format_not_run_naming_trust() {
    let project = prettier_project("const   a = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(outside.path(), "/bin/cat");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert!(
        prettier_runs(outside.path()).is_empty(),
        "prettier never starts without trust: {json}"
    );
    assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
    let message = at(&json, "/findings/0/message");
    assert!(
        message
            .as_str()
            .is_some_and(|message| message.contains("prettier didn't run on TypeScript blocks")),
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason.as_str().is_some_and(|reason| reason.starts_with(
            "prettier runs on TypeScript blocks only in the project tier, which needs `supdoc trust`"
        )),
        "{json}"
    );
}

#[test]
fn prettier_the_real_one_passes_a_formatted_block_and_fails_another_in_ts_and_js() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_passes_a_formatted_block_and_fails_another_in_ts_and_js",
    ) else {
        return;
    };
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for (table, tag) in [("typescript", "ts"), ("javascript", "js")] {
        let formatted = project_with_block(&prettier_tables(table), tag, "const a = 1;");
        let unformatted = project_with_block(&prettier_tables(table), tag, "const   a = 1");

        let passes = trusted_prettier_check(&formatted, &dir, cache.path(), 0);
        let fails = trusted_prettier_check(&unformatted, &dir, cache.path(), 1);

        assert_eq!(
            at(&passes, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {passes}"
        );
        assert_eq!(
            finding_kinds(&fails),
            ["code-block-unformatted"],
            "{tag}: {fails}"
        );
    }
}

#[test]
fn prettier_the_real_ones_syntax_error_is_a_parse_error_at_its_doc_line() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_ones_syntax_error_is_a_parse_error_at_its_doc_line",
    ) else {
        return;
    };
    let project = prettier_project("const a = {\n  b: 1,,\n};");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_prettier_check(&project, &dir, cache.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn prettier_the_real_one_fails_a_misformatted_block_the_projects_ignore_files_cover() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_fails_a_misformatted_block_the_projects_ignore_files_cover",
    ) else {
        return;
    };
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for ignore in [".gitignore", ".prettierignore"] {
        let project = prettier_project("const   a = 1");
        // Covers the block's virtual path, never the doc itself, which supdoc would then skip.
        project.write(ignore, "docs/blocks.md.*\n");

        let json = trusted_prettier_check(&project, &dir, cache.path(), 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{ignore} never turns the block into a pass: {json}"
        );
    }
}

#[test]
fn prettier_the_real_one_fails_a_misformatted_block_whatever_the_configs_pragma_options() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_fails_a_misformatted_block_whatever_the_configs_pragma_options",
    ) else {
        return;
    };
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for (config, body) in [
        ("{\"requirePragma\": true}", "const   a = 1"),
        (
            "{\"checkIgnorePragma\": true}",
            "/** @noprettier */\nconst   a = 1",
        ),
        (
            "{\"checkIgnorePragma\": true}",
            "/** @NoFormat */\nconst   a = 1",
        ),
    ] {
        let project = prettier_project(body);
        project.write(".prettierrc", config);

        let json = trusted_prettier_check(&project, &dir, cache.path(), 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{config} never turns {body:?} into a pass: {json}"
        );
    }
}

#[test]
fn prettier_the_real_one_fails_a_misformatted_block_whatever_the_configs_range_or_parser() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_fails_a_misformatted_block_whatever_the_configs_range_or_parser",
    ) else {
        return;
    };
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for (table, tag, config) in [
        ("typescript", "ts", "{\"rangeEnd\": 0}"),
        (
            "typescript",
            "ts",
            "{\"overrides\": [{\"files\": \"*.md.*.ts\", \"options\": {\"parser\": \"markdown\"}}]}",
        ),
        (
            "javascript",
            "js",
            "{\"overrides\": [{\"files\": \"*.md.*.js\", \"options\": {\"parser\": \"markdown\"}}]}",
        ),
    ] {
        let project = project_with_block(&prettier_tables(table), tag, "const a=1;");
        project.write(".prettierrc", config);

        let json = trusted_prettier_check(&project, &dir, cache.path(), 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{config} never turns a {tag} block into a pass: {json}"
        );
    }
}

#[test]
fn prettier_older_than_the_floor_or_unreadable_is_format_not_run_with_one_warning() {
    for (version, named) in [
        ("echo 3.5.3", "prettier 3.5.3 isn't 3.6.0 or later"),
        (
            "echo 'garbage out'",
            "prettier (whose version supdoc couldn't read) isn't 3.6.0 or later",
        ),
    ] {
        let project = prettier_project("const   a = 1");
        project.write(
            "docs/blocks.md",
            "# Blocks\n\n```ts\nconst   a = 1\n```\n\n```ts\nconst   b = 2\n```\n",
        );
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        prettier_stub_versioned(outside.path(), version, "/bin/cat");

        let json = trusted_prettier_check(&project, outside.path(), cache.path(), 0);

        assert!(
            prettier_runs(outside.path()).is_empty(),
            "{version}: prettier never formats a block: {json}"
        );
        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{version}: one warning for the run: {json}"
        );
        let message = at(&json, "/findings/0/message");
        assert!(
            message
                .as_str()
                .is_some_and(|message| message.starts_with(named)),
            "{version}: {json}"
        );
        for block in 0..2 {
            assert_eq!(
                at(&json, &format!("/data/code_blocks/{block}/format")),
                "not_run",
                "{version}, block {block}: {json}"
            );
        }
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
    }
}

#[test]
fn prettier_stderr_reaches_the_finding_without_its_ansi_escapes() {
    let project = prettier_project("const a = 1;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(
        outside.path(),
        "/bin/cat >/dev/null\nprintf '[error] \\033[31mboom\\033[0m in the config\\n' >&2\nexit 2",
    );

    let json = trusted_prettier_check(&project, outside.path(), cache.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
    let message = at(&json, "/findings/0/message");
    assert!(
        message.as_str().is_some_and(|message| {
            message.contains("[error] boom in the config") && !message.contains('\u{1b}')
        }),
        "{json}"
    );
}

#[test]
fn prettier_the_real_one_fails_a_misformatted_block_in_a_doc_under_node_modules() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_fails_a_misformatted_block_in_a_doc_under_node_modules",
    ) else {
        return;
    };
    let project = prettier_project("const a = 1;");
    project.write(
        "node_modules/pkg/README.md",
        "# Pkg\n\n```ts\nconst   a = 1\n```\n",
    );
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_prettier_check(&project, &dir, cache.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/findings/0/location/file"),
        "node_modules/pkg/README.md",
        "{json}"
    );
}

#[test]
fn prettier_the_real_one_passes_a_formatted_block_whatever_the_configs_insert_pragma() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_passes_a_formatted_block_whatever_the_configs_insert_pragma",
    ) else {
        return;
    };
    let project = prettier_project("const a = 1;");
    project.write(".prettierrc", "{\"insertPragma\": true}");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_prettier_check(&project, &dir, cache.path(), 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}
