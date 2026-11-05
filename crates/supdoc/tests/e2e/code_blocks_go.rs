//! Checking Go code blocks with gofmt.

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

/// A recorded `basic` project with the go table and one block tagged `tag` holding `body` in
/// `docs/blocks.md`, its fence on line 3 and its body from line 4.
fn project_with_block(tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.go]\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `gofmt` running `script` in `dir`.
fn gofmt_stub(dir: &Path, script: &str) {
    let path = dir.join("gofmt");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// `supdoc check --json` in `project` with `PATH` set to `dir` alone, expecting `code`.
fn check(project: &Project, dir: &Path, code: i32) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", dir)])
        .assert_exit(code)
        .json()
}

#[test]
fn gofmt_reads_the_block_on_stdin_with_no_arguments_outside_the_root_and_a_local_toolchain() {
    for tag in ["go", "golang"] {
        let project = project_with_block(tag, "x := 1");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = project.path(".");
        gofmt_stub(
            outside.path(),
            &format!(
                "[ $# -eq 0 ] || exit 3\n\
                 [ \"$GOTOOLCHAIN\" = local ] || exit 4\n\
                 case \"$(pwd -P)\" in '{}'*) exit 5;; esac\n\
                 /bin/cat",
                root.canonicalize()
                    .expect("the project root should resolve")
                    .display()
            ),
        );

        let json = check(&project, outside.path(), 0);

        assert!(finding_kinds(&json).is_empty(), "`{tag}`: {json}");
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "`{tag}`: {json}"
        );
    }
}

#[test]
fn a_block_gofmt_would_change_is_unformatted() {
    let project = project_with_block("go", "x:=1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    gofmt_stub(outside.path(), "/bin/cat >/dev/null\necho 'x := 1'");

    let json = check(&project, outside.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`gofmt` would format it differently",
        "{json}"
    );
}

#[test]
fn a_whole_files_syntax_error_sits_at_its_doc_line() {
    let project = project_with_block("go", "package p\n\nfunc f() {\n\tx := <\n}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    gofmt_stub(
        outside.path(),
        "/bin/cat >/dev/null\necho \"<standard input>:4:7: expected operand, found '<'\" >&2\nexit 2",
    );

    let json = check(&project, outside.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 7, "{json}");
    let message = at(&json, "/findings/0/message")
        .as_str()
        .expect("the message should be a string");
    assert!(message.contains("4:7: expected operand"), "{json}");
}

#[test]
fn a_fragments_syntax_error_sits_at_its_doc_line_without_the_wrappers_end() {
    let project = project_with_block("go", "x := <name>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    gofmt_stub(
        outside.path(),
        "/bin/cat >/dev/null\n\
         echo \"<standard input>:1:27: expected operand, found '<'\" >&2\n\
         echo \"<standard input>:4:2: expected ';', found 'EOF'\" >&2\nexit 2",
    );

    let json = check(&project, outside.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    let message = message_of(&json);
    assert!(message.contains("expected operand, found '<'"), "{json}");
    assert!(
        !message.contains("1:27") && !message.contains("expected ';'"),
        "a fragment's message should drop gofmt's wrapper column and the wrapper's end: {json}"
    );
}

/// The first finding's message.
fn message_of(json: &serde_json::Value) -> &str {
    at(json, "/findings/0/message")
        .as_str()
        .expect("the message should be a string")
}

#[test]
fn a_formatter_that_prints_nothing_is_not_run_with_a_warning_never_a_pass_or_a_fix() {
    let project = project_with_block("go", "x := 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    gofmt_stub(outside.path(), "/bin/cat >/dev/null\necho");

    let json = check(&project, outside.path(), 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
    let fixed = project
        .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())])
        .code;
    assert!(fixed.is_some(), "fix should exit");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```go\nx := 1\n```\n",
        "a formatter that printed nothing must never rewrite the block"
    );
}

/// The real gofmt's `PATH` directory, or `None` to skip `test`.
fn real_gofmt(test: &str) -> Option<std::path::PathBuf> {
    real_tool_dir("gofmt", test)
}

#[test]
fn the_real_gofmt_passes_a_formatted_file_and_fails_unformatted_statements() {
    let Some(dir) =
        real_gofmt("the_real_gofmt_passes_a_formatted_file_and_fails_unformatted_statements")
    else {
        return;
    };
    let formatted = project_with_block("go", "package p\n\nfunc f() {\n\tprintln(1)\n}");
    let unformatted = project_with_block("go", "x:=1\nfmt.Println( x)");

    let passes = check(&formatted, &dir, 0);
    let fails = check(&unformatted, &dir, 1);

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    let _ = unformatted
        .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)])
        .assert_exit(0);
    assert_eq!(
        unformatted.read("docs/blocks.md"),
        "# Blocks\n\n```go\nx := 1\nfmt.Println(x)\n```\n"
    );
}

#[test]
fn the_real_gofmts_syntax_errors_sit_at_their_doc_line_for_a_file_and_each_fragment_kind() {
    let Some(dir) = real_gofmt(
        "the_real_gofmts_syntax_errors_sit_at_their_doc_line_for_a_file_and_each_fragment_kind",
    ) else {
        return;
    };
    // (info string, body, doc line, text gofmt's wrapper end adds that must not show)
    let cases = [
        ("go", "package p\n\nfunc f() {\n\tx := <\n}", 7, None),
        (
            "go fragment(statements)",
            "a := 1\nb := 2\nc := <",
            6,
            Some("expected ';'"),
        ),
        (
            "go fragment(expression)",
            "fmt.Println(<)",
            4,
            Some("missing ','"),
        ),
        (
            "go fragment(declarations)",
            "func f() {}\nfunc g( {}",
            5,
            None,
        ),
    ];

    for (info, body, line, wrapper) in cases {
        let json = check(&project_with_block(info, body), &dir, 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-parse"],
            "`{info}`: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/location/line"),
            line,
            "`{info}`: {json}"
        );
        let message = message_of(&json);
        assert!(message.contains("expected"), "`{info}`: {json}");
        if let Some(wrapper) = wrapper {
            assert!(!message.contains(wrapper), "`{info}`: {json}");
        }
    }
}

#[test]
fn the_real_gofmts_error_after_a_line_directive_is_a_parse_error_at_the_fence() {
    let Some(dir) =
        real_gofmt("the_real_gofmts_error_after_a_line_directive_is_a_parse_error_at_the_fence")
    else {
        return;
    };

    for directive in ["//line foo.go:10", "//line :10"] {
        let project = project_with_block("go", &format!("x := 1\n{directive}\ny := ("));

        let json = check(&project, &dir, 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-parse"],
            "`{directive}`: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/location/line"),
            3,
            "`{directive}`: {json}"
        );
        let message = message_of(&json);
        assert!(
            message.contains("expected operand") && !message.contains("13:"),
            "`{directive}`: the renamed position should be dropped: {json}"
        );
    }
}

/// A recorded `basic` project with the go table and `doc` as `docs/blocks.md`.
fn project_with_doc(doc: &str) -> Project {
    let project = project_with_block("go", "");
    project.write("docs/blocks.md", doc);
    project
}

#[test]
fn the_real_gofmt_passes_and_fixes_an_expression_and_declarations_touching_only_their_lines() {
    let Some(dir) = real_gofmt(
        "the_real_gofmt_passes_and_fixes_an_expression_and_declarations_touching_only_their_lines",
    ) else {
        return;
    };
    let doc = |kind: &str, body: &str| {
        format!("# Blocks\n\nBefore.\n\n```go fragment({kind})\n{body}\n```\n\nAfter.\n")
    };
    // (kind, unformatted body, formatted body)
    let cases = [
        ("expression", "fmt.Println( x )", "fmt.Println(x)"),
        (
            "declarations",
            "func f(){}\n\nvar  x = 1",
            "func f() {}\n\nvar x = 1",
        ),
    ];

    for (kind, unformatted, formatted) in cases {
        let passes = check(&project_with_doc(&doc(kind, formatted)), &dir, 0);
        let failing = project_with_doc(&doc(kind, unformatted));
        let fails = check(&failing, &dir, 1);

        assert_eq!(
            at(&passes, "/data/summary/code_blocks/passed"),
            1,
            "`{kind}`: {passes}"
        );
        assert_eq!(
            finding_kinds(&fails),
            ["code-block-unformatted"],
            "`{kind}`: {fails}"
        );
        let _ = failing
            .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)])
            .assert_exit(0);
        assert_eq!(
            failing.read("docs/blocks.md"),
            doc(kind, formatted),
            "`{kind}`: the fix should rewrite only the block's own lines"
        );
    }
}

#[test]
fn the_real_gofmt_applies_no_config_planted_in_the_project_home_or_above_the_cache() {
    let Some(dir) = real_gofmt(
        "the_real_gofmt_applies_no_config_planted_in_the_project_home_or_above_the_cache",
    ) else {
        return;
    };
    let project = project_with_block("go", "func f() {\n\tprintln(1)\n}");
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let spaces = "root = true\n[*]\nindent_style = space\nindent_size = 2\n";
    let plant = |base: &Path| {
        std::fs::create_dir_all(base.join(".config/go"))
            .expect("the planted config directory should be creatable");
        std::fs::write(base.join(".editorconfig"), spaces).expect("the plant should be writable");
        std::fs::write(base.join("go.mod"), "module planted\n\ngo 1.10\n")
            .expect("the plant should be writable");
        std::fs::write(base.join(".config/go/env"), "GOFLAGS=-mod=vendor\n")
            .expect("the plant should be writable");
    };
    plant(home.path());
    plant(&home.path().join(".cache"));
    plant(&project.path("."));

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", &dir), ("HOME", home.path())],
        )
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}
