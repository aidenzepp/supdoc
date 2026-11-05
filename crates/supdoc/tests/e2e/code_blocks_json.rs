//! Checking JSON code blocks, tagged `json` or `jsonc`, formatted and linted by biome.

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

/// A recorded `basic` project with a `json` table and one block tagged `tag` holding `body` in
/// `docs/blocks.md`.
fn json_project(tag: &str, body: &str) -> Project {
    project_with("[features.code-blocks.json]\n", tag, body)
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

/// A biome format that reports a syntax error at line 2 of the block, as it does for a comment
/// in strict JSON.
const PARSE_ERROR: &str = "echo \"${3#--stdin-file-path=}:2:3 parse ━━━━━━━━\" >&2\n\
     echo \"  × Expected a property but instead found '// c'.\" >&2\nexit 1";

#[test]
fn json_each_tag_gets_its_own_extension_for_format_and_lint() {
    for (tag, ext) in [("json", "json"), ("jsonc", "jsonc")] {
        let project = json_project(tag, "{ \"a\": 1 }");
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
fn json_a_comment_in_a_json_block_is_a_parse_error_whose_help_names_jsonc() {
    let project = json_project("json", "{\n  // c\n  \"a\": 1\n}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), &stub_for("json", PARSE_ERROR, CLEAN));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert!(
        at(&json, "/findings/0/help")
            .as_str()
            .is_some_and(|help| help.contains("tag it `jsonc`")),
        "{json}"
    );
}

#[test]
fn json_a_trailing_comma_is_a_parse_error_whose_help_never_names_jsonc() {
    let project = json_project("json", "{\n  \"a\": 1,\n}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), &stub_for("json", PARSE_ERROR, CLEAN));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert!(
        at(&json, "/findings/0/help")
            .as_str()
            .is_some_and(|help| !help.contains("jsonc")),
        "{json}"
    );
}

#[test]
fn json_a_parse_error_in_a_jsonc_block_keeps_the_usual_help() {
    let project = json_project("jsonc", "{\n  \"a\": \n}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), &stub_for("jsonc", PARSE_ERROR, CLEAN));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert!(
        at(&json, "/findings/0/help")
            .as_str()
            .is_some_and(|help| !help.contains("jsonc")),
        "{json}"
    );
}

#[test]
fn json_a_lint_diagnostic_is_a_code_block_lint_error_at_its_doc_line() {
    let project = json_project("json", "{ \"a\": 1, \"a\": 2 }");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let report = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[{"severity":"error","message":"The key a was already declared.","category":"lint/suspicious/noDuplicateObjectKeys","location":{"path":"x","start":{"line":1,"column":11},"end":{"line":1,"column":14}},"advices":[]}],"command":"lint"}"#;
    stub_biome(outside.path(), &stub_for("json", "/bin/cat", report));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("`biome-lint` reports `suspicious/noDuplicateObjectKeys`")),
        "{json}"
    );
}

/// A project whose `json` table turns biome's format step off, so only the lint step reads the
/// block tagged `json` holding `body`.
fn lint_only(body: &str) -> Project {
    project_with(
        "[features.code-blocks.json]\n[features.code-blocks.json.biome-format]\nformat = \"off\"\n",
        "json",
        body,
    )
}

/// biome lint's report of one syntax error, `message`, at line 2 column 3 of the block.
fn parse_report(message: &str) -> String {
    format!(
        r#"{{"summary":{{"changed":0,"unchanged":1}},"diagnostics":[{{"severity":"error","message":"{message}","category":"parse","location":{{"path":"x","start":{{"line":2,"column":3}}}},"advices":[]}}]}}"#
    )
}

#[test]
fn json_lint_a_syntax_error_is_a_parse_error_at_its_doc_line_and_names_jsonc_for_a_comment() {
    for (body, message, hint) in [
        (
            "{\n  // c\n  \"a\": 1\n}",
            "Expected a property but instead found '// c'.",
            true,
        ),
        (
            "{\n  \"a\": 1,\n}",
            "Expected a property but instead found '}'.",
            false,
        ),
    ] {
        let project = lint_only(body);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_biome(
            outside.path(),
            &stub_for("json", "exit 7", &parse_report(message)),
        );

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(1)
            .json();

        assert_eq!(finding_kinds(&json), ["code-block-parse"], "{body}: {json}");
        assert_eq!(at(&json, "/findings/0/location/line"), 5, "{body}: {json}");
        assert_eq!(
            at(&json, "/findings/0/help")
                .as_str()
                .is_some_and(|help| help.contains("`jsonc`")),
            hint,
            "{body}: {json}"
        );
    }
}

#[test]
fn json_the_real_biome_lint_reads_a_syntax_error_as_a_parse_error_naming_jsonc_for_a_comment() {
    let Some(dir) = real_tool_dir(
        "biome",
        "json_the_real_biome_lint_reads_a_syntax_error_as_a_parse_error_naming_jsonc_for_a_comment",
    ) else {
        return;
    };
    for (body, line, hint) in [
        ("{\n  // c\n  \"a\": 1\n}", 5, true),
        ("{\n  \"a\": 1,\n}", 6, false),
    ] {
        let json = lint_only(body)
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
            .assert_exit(1)
            .json();

        assert_eq!(finding_kinds(&json), ["code-block-parse"], "{body}: {json}");
        assert_eq!(
            at(&json, "/findings/0/location/line"),
            line,
            "biome names the comment, or the `}}` after a trailing comma: {body}: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/help")
                .as_str()
                .is_some_and(|help| help.contains("`jsonc`")),
            hint,
            "{body}: {json}"
        );
    }
}

#[test]
fn json_the_real_biome_format_names_jsonc_for_a_comment_and_not_for_a_trailing_comma() {
    let Some(dir) = real_tool_dir(
        "biome",
        "json_the_real_biome_format_names_jsonc_for_a_comment_and_not_for_a_trailing_comma",
    ) else {
        return;
    };
    for (body, hint) in [
        ("{\n  \"a\": 1 /* c */\n}", true),
        ("{\n  \"a\": 1,\n}", false),
    ] {
        let json = json_project("json", body)
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
            .assert_exit(1)
            .json();

        assert_eq!(finding_kinds(&json), ["code-block-parse"], "{body}: {json}");
        assert_eq!(
            at(&json, "/findings/0/help")
                .as_str()
                .is_some_and(|help| help.contains("`jsonc`")),
            hint,
            "{body}: {json}"
        );
    }
}

#[test]
fn json_the_real_biome_passes_jsonc_comments_and_fails_them_in_json() {
    let Some(dir) = real_tool_dir(
        "biome",
        "json_the_real_biome_passes_jsonc_comments_and_fails_them_in_json",
    ) else {
        return;
    };
    let commented = "{\n\t// c\n\t\"a\": 1\n}";

    let formatted = json_project("json", "{ \"a\": 1 }")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let unformatted = json_project("json", "{\"a\":1}")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();
    let jsonc = json_project("jsonc", commented)
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let json = json_project("json", commented)
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&formatted, "/data/summary/code_blocks/passed"),
        1,
        "{formatted}"
    );
    assert_eq!(
        finding_kinds(&unformatted),
        ["code-block-unformatted"],
        "{unformatted}"
    );
    assert_eq!(at(&jsonc, "/data/summary/code_blocks/passed"), 1, "{jsonc}");
    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert!(
        at(&json, "/findings/0/help")
            .as_str()
            .is_some_and(|help| help.contains("`jsonc`")),
        "{json}"
    );
}

#[test]
fn json_the_real_biome_lint_reports_a_duplicate_key() {
    let Some(dir) = real_tool_dir("biome", "json_the_real_biome_lint_reports_a_duplicate_key")
    else {
        return;
    };

    let json = json_project("json", "{ \"a\": 1, \"a\": 2 }")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("suspicious/noDuplicateObjectKeys")),
        "{json}"
    );
}

#[test]
fn frame_entries_pass_formatted_and_linted_inside_the_objects_braces() {
    let test = "frame_entries_pass_formatted_and_linted_inside_the_objects_braces";
    let Some(dir) = real_tool_dir("biome", test) else {
        return;
    };

    let json = json_project(
        "json fragment(entries)",
        "\"steward\": { \"enabled\": true }",
    )
    .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
    .assert_exit(0)
    .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn frame_entries_a_duplicate_key_is_found_at_its_doc_line() {
    let test = "frame_entries_a_duplicate_key_is_found_at_its_doc_line";
    let Some(dir) = real_tool_dir("biome", test) else {
        return;
    };

    let json = json_project("json fragment(entries)", "\"a\": 1,\n\"a\": 2")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("suspicious/noDuplicateObjectKeys")),
        "{json}"
    );
    assert_eq!(
        at(&json, "/findings/0/location/line"),
        4,
        "biome names the first `a`, at its doc line rather than a frame line or the attribute: {json}"
    );
}

/// Each JSON tag prettier reads, with the parser and virtual-path extension it gets.
const CASES: [(&str, &str, &str); 2] = [("json", "json", "json"), ("jsonc", "jsonc", "jsonc")];

/// A JSON block prettier leaves as it is.
const FORMATTED: &str = "{ \"a\": 1 }";

/// A JSON block prettier reformats.
const MISFORMATTED: &str = "{\"a\":1}";

/// A JSON block whose second line prettier can't parse.
const BROKEN: &str = "{\n  \"a\": 1,,\n}";

/// A JSON block holding `prettier-ignore`.
const IGNORED: &str = "{ \"prettier-ignore\": 1 }";

/// The config that selects prettier for json blocks, with biome-lint's step off so prettier is the
/// block's one tool.
const PRETTIER_CONFIG: &str = "[features.code-blocks]\n[features.code-blocks.json]\n\
    formatter = \"prettier\"\n[features.code-blocks.json.biome-lint]\nlint = \"off\"\n";

/// A recorded `basic` project selecting prettier for json, with one block tagged `tag` holding
/// `body` in `docs/blocks.md`, which starts on doc line 4.
fn prettier_project(tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", PRETTIER_CONFIG);
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
    );
    project
}

/// Writes a `prettier` stub in `dir` that appends its working directory and arguments to
/// `<dir>/prettier-runs`, one line per call, prints `3.9.10` for `--version` and echoes its input
/// for any other call.
fn prettier_stub(dir: &Path) {
    let path = dir.join("prettier");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$(pwd -P) $*\" >> '{}'\n\
             if [ \"$1\" = --version ]; then echo 3.9.10; exit 0; fi\n/bin/cat\n",
            dir.join("prettier-runs").display()
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// The calls the [`prettier_stub`] in `dir` recorded that format a block: every one but
/// `--version`.
fn prettier_formats(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("prettier-runs"))
        .unwrap_or_default()
        .lines()
        .filter(|run| !run.ends_with(" --version"))
        .map(str::to_owned)
        .collect()
}

/// Runs a `check --json` on `project` with `path` as the only `PATH` entry, `trust` as
/// `SUPDOC_TRUST` and its cache in a fresh directory, asserting it exits `exit`.
fn prettier_check(project: &Project, path: &Path, trust: &str, exit: i32) -> serde_json::Value {
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[
                ("PATH", path),
                ("SUPDOC_TRUST", Path::new(trust)),
                ("XDG_CACHE_HOME", cache.path()),
            ],
        )
        .assert_exit(exit)
        .json()
}

#[test]
fn prettier_formats_a_json_block_from_the_root_by_its_tags_parser() {
    for (tag, parser, extension) in CASES {
        let project = prettier_project(tag, FORMATTED);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        prettier_stub(outside.path());

        let json = prettier_check(&project, outside.path(), "1", 0);

        let root = project
            .path(".")
            .canonicalize()
            .expect("the project root should resolve");
        let runs = prettier_formats(outside.path());
        let [run] = runs.as_slice() else {
            panic!("{tag}: prettier formats the block once: {runs:?}");
        };
        let (cwd, args) = run
            .split_once(' ')
            .expect("the record names a cwd and args");
        assert_eq!(
            Path::new(cwd),
            root,
            "{tag}: prettier runs from the root: {run}"
        );
        let overrides = format!(
            "--ignore-path= --with-node-modules --no-require-pragma --no-check-ignore-pragma \
             --no-insert-pragma --range-start=0 --range-end=Infinity --parser={parser} \
             --stdin-filepath "
        );
        let path = args.strip_prefix(&overrides);
        assert!(
            path.is_some_and(|path| path.ends_with(&format!("/docs/blocks.md.1.{extension}"))),
            "{tag}: prettier gets its overrides, the tag's parser and the block's path: {run}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {json}"
        );
    }
}

#[test]
fn prettier_untrusted_leaves_a_json_block_format_not_run_naming_trust() {
    let project = prettier_project(CASES[0].0, FORMATTED);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(outside.path());

    let json = prettier_check(&project, outside.path(), "0", 0);

    assert!(prettier_formats(outside.path()).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason
            .as_str()
            .is_some_and(|reason| reason
                .starts_with("prettier runs on JSON blocks only in the project tier")),
        "{json}"
    );
}

#[test]
fn prettier_ignore_in_a_json_block_is_format_not_run_naming_format_off() {
    let project = prettier_project(CASES[0].0, IGNORED);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(outside.path());

    let json = prettier_check(&project, outside.path(), "1", 0);

    assert!(prettier_formats(outside.path()).is_empty(), "{json}");
    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
}

#[test]
fn prettier_the_real_one_passes_a_formatted_json_block_and_fails_a_misformatted_one() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_passes_a_formatted_json_block_and_fails_a_misformatted_one",
    ) else {
        return;
    };
    for (tag, _, _) in CASES {
        let passes = prettier_check(&prettier_project(tag, FORMATTED), &dir, "1", 0);
        let fails = prettier_check(&prettier_project(tag, MISFORMATTED), &dir, "1", 1);

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
fn prettier_the_real_one_fails_a_misformatted_json_block_the_projects_ignore_files_cover() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_fails_a_misformatted_json_block_the_projects_ignore_files_cover",
    ) else {
        return;
    };
    for ignore in [".gitignore", ".prettierignore"] {
        let project = prettier_project(CASES[0].0, MISFORMATTED);
        // Covers the block's virtual path, never the doc itself, which supdoc would then skip.
        project.write(ignore, "docs/blocks.md.*\n");

        let json = prettier_check(&project, &dir, "1", 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{ignore} never turns the block into a pass: {json}"
        );
    }
}

#[test]
fn prettier_the_real_ones_json_syntax_error_is_a_parse_error_at_its_doc_line() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_ones_json_syntax_error_is_a_parse_error_at_its_doc_line",
    ) else {
        return;
    };
    let json = prettier_check(&prettier_project(CASES[0].0, BROKEN), &dir, "1", 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}
