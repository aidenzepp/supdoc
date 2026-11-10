//! Checking CSS code blocks, tagged `css` or `scss`, formatted and linted by biome.

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

/// A recorded `basic` project with a `css` table and one block tagged `tag` holding `body` in
/// `docs/blocks.md`.
fn css_project(tag: &str, body: &str) -> Project {
    project_with("[features.code-blocks.css]\n", tag, body)
}

/// A recorded `basic` project with `tables` written under `[features.code-blocks]` and one block
/// tagged `tag` holding `body` in `docs/blocks.md`.
fn project_with(tables: &str, tag: &str, body: &str) -> Project {
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

/// Writes an executable `#!/bin/sh` stub named `biome` running `script` in `dir`.
fn stub_biome(dir: &Path, script: &str) {
    let path = dir.join("biome");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A stub whose `biome format` accepts only the stdin path `<doc>.1.<ext>`, then runs `then`, and
/// whose `biome lint` accepts only a block file ending `blocks.md.1.<ext>` and prints `report`.
fn stub_for(ext: &str, then: &str, report: &str) -> String {
    format!(
        "if [ \"$1\" = lint ]; then\n\
         for arg in \"$@\"; do file=\"$arg\"; done\n\
         case \"$file\" in */biome-lint/*/blocks.md.1.{ext}) ;; *) exit 6;; esac\n\
         echo '{report}'\nexit 0\nfi\n\
         [ \"$1 $2\" = 'format --colors=off' ] || exit 3\n\
         case \"$3\" in --stdin-file-path=*/docs/blocks.md.1.{ext}) ;; *) exit 4;; esac\n\
         {then}"
    )
}

/// biome lint's report for a block it checked and found nothing in.
const CLEAN: &str = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[]}"#;

/// biome lint's report when the project's config turns the CSS linter off: it checked no file.
const LINTED_NOTHING: &str =
    r#"{"summary":{"changed":0,"unchanged":0},"diagnostics":[],"command":"lint"}"#;

/// A `css` table with biome's lint step off, so the format step alone decides the block.
const FORMAT_ONLY: &str =
    "[features.code-blocks.css]\n[features.code-blocks.css.biome-lint]\nlint = \"off\"\n";

/// A `css` table with biome's format step off, so the lint step alone decides the block.
const LINT_ONLY: &str =
    "[features.code-blocks.css]\n[features.code-blocks.css.biome-format]\nformat = \"off\"\n";

/// What biome 2.6.5's format does with `css.formatter.enabled: false` (probe V6-PR5, S4): the
/// block back unchanged on stdout, the notice on stderr, exit 1.
const FORMATTER_OFF: &str = "/bin/cat\n\
     echo 'The content was not formatted because the formatter is currently disabled.' >&2\n\
     echo 'stdin ━━━━━━━━' >&2\n\
     echo \"  × The contents aren't fixed. Use the \\`--write\\` flag to fix them.\" >&2\nexit 1";

#[test]
fn css_each_tag_gets_its_own_extension_for_format_and_lint() {
    for (tag, ext) in [("css", "css"), ("scss", "scss")] {
        let project = css_project(tag, "a {\n\tcolor: red;\n}");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_biome(outside.path(), &stub_for(ext, "/bin/cat", CLEAN));

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        assert!(finding_kinds(&json).is_empty(), "{tag}: {json}");
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {json}"
        );
    }
}

#[test]
fn css_an_unformatted_block_fails_and_fix_writes_biomes_output() {
    let project = css_project("css", "a{color:red}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(
        outside.path(),
        &stub_for(
            "css",
            "/bin/cat >/dev/null\nprintf 'a {\\n\\tcolor: red;\\n}\\n'",
            CLEAN,
        ),
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();
    let _ = project
        .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())])
        .assert_exit(0);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```css\na {\n\tcolor: red;\n}\n```\n"
    );
}

#[test]
fn css_a_lint_diagnostic_is_a_code_block_lint_error_at_its_doc_line() {
    let project = css_project("css", "a {\n\tcolor: red;\n\tcolor: blue;\n}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let report = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[{"severity":"error","message":"Duplicate properties can lead to unexpected behavior and may override previous declarations unintentionally.","category":"lint/suspicious/noDuplicateProperties","location":{"path":"x","start":{"line":3,"column":2},"end":{"line":3,"column":7}},"advices":[]}],"command":"lint"}"#;
    stub_biome(outside.path(), &stub_for("css", "/bin/cat", report));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("`biome-lint` reports `suspicious/noDuplicateProperties`")),
        "{json}"
    );
}

#[test]
fn css_a_formatter_the_projects_config_turns_off_is_not_run_never_passed_or_fixed() {
    for (tag, ext) in [("css", "css"), ("scss", "scss")] {
        let body = "a{color:red}";
        let project = project_with(FORMAT_ONLY, tag, body);
        project.write("biome.json", r#"{"css":{"formatter":{"enabled":false}}}"#);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_biome(outside.path(), &stub_for(ext, FORMATTER_OFF, CLEAN));

        let json = project
            .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();
        let fixed = project
            .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())])
            .code;

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{tag}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|m| m.contains("turns the CSS formatter off")),
            "{tag}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{tag}: {json}"
        );
        assert_eq!(
            at(&json, "/data/code_blocks/0/format"),
            "not_run",
            "{tag}: {json}"
        );
        assert!(fixed.is_some(), "{tag}: fix should exit");
        assert_eq!(
            project.read("docs/blocks.md"),
            format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
            "{tag}: a formatter that is off must never rewrite the block"
        );
    }
}

#[test]
fn css_a_failed_format_without_biomes_off_notice_stays_a_tool_failure() {
    let project = css_project("css", "a{color:red}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(
        outside.path(),
        &stub_for(
            "css",
            "/bin/cat\necho 'stdin ━━━━━━━━' >&2\necho '  × something else' >&2\nexit 1",
            CLEAN,
        ),
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
}

#[test]
fn css_the_off_notice_is_read_for_css_blocks_only_and_a_json_blocks_stays_a_tool_failure() {
    let project = project_with("[features.code-blocks.json]\n", "json", "{\"a\":1}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), &stub_for("json", FORMATTER_OFF, CLEAN));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
}

#[test]
fn css_a_linter_the_projects_config_turns_off_is_not_run_never_passed() {
    let project = project_with(LINT_ONLY, "css", "a {\n\tcolor: red;\n\tcolor: blue;\n}");
    project.write("biome.json", r#"{"css":{"linter":{"enabled":false}}}"#);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), &stub_for("css", "exit 7", LINTED_NOTHING));

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
    assert!(
        at(&json, "/data/code_blocks/0/reason")
            .as_str()
            .is_some_and(|reason| reason.contains("biome lint checked no file")),
        "{json}"
    );
}

#[test]
fn css_a_config_with_extends_or_plugins_makes_the_block_not_checked_with_its_own_help() {
    for (config, help) in [
        (r#"{"extends":["./base.json"]}"#, "remove `extends`"),
        (r#"{"plugins":["./x.grit"]}"#, "remove `plugins`"),
    ] {
        let project = css_project("css", "a {\n\tcolor: red;\n}");
        project.write("biome.json", config);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_biome(outside.path(), "exit 9");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{config}: {json}"
        );
        assert!(
            at(&json, "/findings/0/help")
                .as_str()
                .is_some_and(|h| h.contains(help)),
            "{config}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{config}: {json}"
        );
    }
}

#[test]
fn css_prettier_is_refused_as_a_formatter() {
    let project = project_with(
        "[features.code-blocks.css]\nformatter = \"prettier\"\n",
        "css",
        "a {\n\tcolor: red;\n}",
    );

    let run = project.supdoc(&["check", "--json"]);
    let _checked = run.assert_exit(78);

    assert!(
        run.stderr.contains("\"config-parse\"")
            && run
                .stderr
                .contains("`prettier` isn't a known css formatter"),
        "{}",
        run.stderr
    );
}

/// The real biome's `PATH` directory, or `None` to skip `test`.
fn real_biome(test: &str) -> Option<std::path::PathBuf> {
    real_tool_dir("biome", test)
}

/// The JSON report of checking `project` with the tools in `dir`, asserting the exit `code`.
fn check(project: &Project, dir: &Path, code: i32) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", dir)])
        .assert_exit(code)
        .json()
}

#[test]
fn css_the_real_biome_passes_formatted_css_and_scss_and_fix_formats_an_unformatted_block() {
    let Some(dir) = real_biome(
        "css_the_real_biome_passes_formatted_css_and_scss_and_fix_formats_an_unformatted_block",
    ) else {
        return;
    };
    let css = css_project("css", "a {\n\tcolor: red;\n}");
    let scss = css_project("scss", "$c: red;\n.a {\n\t.b {\n\t\tcolor: $c;\n\t}\n}");
    let unformatted = css_project("css", "a{color:red}");

    let css = check(&css, &dir, 0);
    let scss = check(&scss, &dir, 0);
    let fails = check(&unformatted, &dir, 1);
    let _ = unformatted
        .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)])
        .assert_exit(0);

    assert_eq!(at(&css, "/data/summary/code_blocks/passed"), 1, "{css}");
    assert_eq!(at(&scss, "/data/summary/code_blocks/passed"), 1, "{scss}");
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    assert_eq!(
        unformatted.read("docs/blocks.md"),
        "# Blocks\n\n```css\na {\n\tcolor: red;\n}\n```\n"
    );
}

#[test]
fn css_the_real_biome_lint_reports_a_duplicate_property_at_its_doc_line() {
    let Some(dir) =
        real_biome("css_the_real_biome_lint_reports_a_duplicate_property_at_its_doc_line")
    else {
        return;
    };
    for tag in ["css", "scss"] {
        let project = css_project(tag, "a {\n\tcolor: red;\n\tcolor: blue;\n}");

        let json = check(&project, &dir, 1);

        assert_eq!(finding_kinds(&json), ["code-block-lint"], "{tag}: {json}");
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|m| m.contains("suspicious/noDuplicateProperties")),
            "{tag}: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/location/line"),
            6,
            "{tag}: biome names the second `color`: {json}"
        );
    }
}

#[test]
fn css_the_real_biomes_syntax_error_is_a_parse_error_at_its_doc_line() {
    let Some(dir) = real_biome("css_the_real_biomes_syntax_error_is_a_parse_error_at_its_doc_line")
    else {
        return;
    };

    let json = check(&css_project("css", "a {\n\tcolor: red"), &dir, 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
}

#[test]
fn css_the_real_biome_with_the_css_formatter_or_linter_off_never_passes_a_block() {
    let Some(dir) =
        real_biome("css_the_real_biome_with_the_css_formatter_or_linter_off_never_passes_a_block")
    else {
        return;
    };
    for (config, tables, step) in [
        (
            r#"{"css":{"formatter":{"enabled":false}}}"#,
            FORMAT_ONLY,
            "format",
        ),
        (r#"{"css":{"linter":{"enabled":false}}}"#, LINT_ONLY, "lint"),
    ] {
        for tag in ["css", "scss"] {
            let project = project_with(tables, tag, "a{color:red;color:blue}");
            project.write("biome.json", config);

            let json = check(&project, &dir, 0);

            assert_eq!(
                at(&json, &format!("/data/code_blocks/0/{step}")),
                "not_run",
                "{config} {tag}: {json}"
            );
            assert_eq!(
                finding_kinds(&json),
                ["code-block-not-checked"],
                "{config} {tag}: {json}"
            );
            assert_eq!(
                at(&json, "/data/summary/code_blocks/passed"),
                0,
                "{config} {tag}: {json}"
            );
        }
    }
}
