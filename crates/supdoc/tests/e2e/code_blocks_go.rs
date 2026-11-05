//! Checking Go code blocks with gofmt and go vet.

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

/// A recorded `basic` project with the go table, go vet off, and one block tagged `tag` holding
/// `body` in `docs/blocks.md`, its fence on line 3 and its body from line 4.
fn project_with_block(tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.go]\n\
         [features.code-blocks.go.go-vet]\nlint = \"off\"\n",
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

/// A recorded `basic` project with the go table, gofmt off, and one `go` block holding `body` in
/// `docs/blocks.md`, its fence on line 3 and its body from line 4.
fn project_vetting(body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.go]\n\
         [features.code-blocks.go.gofmt]\nformat = \"off\"\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```go\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `go` running `script` in `dir`.
fn go_stub(dir: &Path, script: &str) {
    let path = dir.join("go");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// `supdoc check --json` in `project` with `PATH` set to `dir` and `HOME` to `home`, expecting
/// `code`.
fn check_at_home(project: &Project, dir: &Path, home: &Path, code: i32) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", dir), ("HOME", home)])
        .assert_exit(code)
        .json()
}

#[test]
fn vet_runs_offline_in_a_private_module_outside_the_root_and_home_and_places_its_report() {
    let project = project_vetting("package p\n\nfunc f() {}");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let root = project
        .path(".")
        .canonicalize()
        .expect("the project root should resolve");
    let given_home = home
        .path()
        .canonicalize()
        .expect("the home directory should resolve");
    go_stub(
        outside.path(),
        &format!(
            "[ \"$*\" = 'vet -json .' ] || exit 3\n\
             [ \"$CGO_ENABLED$GOTOOLCHAIN$GOPROXY$GOWORK$GOENV$GOFLAGS\" = '0localoffoffoff-mod=mod' ] || exit 4\n\
             here=\"$(pwd -P)\"\n\
             for dir in \"$here\" \"$HOME\" \"$GOPATH\" \"$GOCACHE\"; do\n\
               case \"$dir\" in '{root}'*|'{home}'|'') exit 5;; esac\n\
             done\n\
             [ \"$(/bin/cat go.mod)\" = \"$(printf 'module supdoc.invalid/block\\n\\ngo 1.23')\" ] || exit 6\n\
             /usr/bin/grep -qx 'func f() {{}}' main.go || exit 7\n\
             [ \"$(/bin/ls)\" = \"$(printf 'go.mod\\nmain.go')\" ] || exit 8\n\
             printf '{{\"supdoc.invalid/block\": {{\"printf\": [{{\"posn\": \"%s/main.go:3:6\", \"end\": \"%s/main.go:3:8\", \"message\": \"f is in %s\"}}]}}}}' \"$here\" \"$here\" \"$here\"",
            root = root.display(),
            home = given_home.display(),
        ),
    );

    let json = check_at_home(&project, outside.path(), home.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    assert_eq!(at(&json, "/findings/0/location/column"), 6, "{json}");
    let message = message_of(&json);
    assert!(message.contains("`go-vet` reports `printf`"), "{json}");
    assert!(
        !message.contains("supdoc/code-blocks") && message.contains("f is in ."),
        "the module's private path should be taken out: {json}"
    );
}

#[test]
fn vet_fails_a_block_with_no_package_clause_naming_the_clause_and_lint_off() {
    let project = project_vetting("x := 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    go_stub(
        outside.path(),
        "echo \"main.go:1:1: expected 'package', found x\" >&2\nexit 1",
    );

    let json = check(&project, outside.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    assert!(message_of(&json).contains("no `package` clause"), "{json}");
    let help = at(&json, "/findings/0/help")
        .as_str()
        .expect("the help should be a string");
    assert!(help.contains("lint(off: <reason>)"), "{json}");
}

#[test]
fn vet_leaves_blocks_importing_outside_the_standard_library_not_run_with_one_warning() {
    let project = project_vetting("package p\n\nimport \"example.com/x\"\n\nvar _ = x.Y");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```go\npackage p\n\nimport \"example.com/x\"\n```\n\n\
         ```go\npackage q\n\nimport \"example.com/y\"\n```\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    go_stub(
        outside.path(),
        "echo 'main.go:3:8: cannot find module providing package example.com/x: module lookup \
         disabled by GOPROXY=off' >&2\nexit 1",
    );

    let json = check(&project, outside.path(), 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    for block in 0..2 {
        assert_eq!(
            at(&json, &format!("/data/code_blocks/{block}/state")),
            "not_checked",
            "{json}"
        );
        assert_eq!(
            at(&json, &format!("/data/code_blocks/{block}/lint")),
            "not_run",
            "{json}"
        );
    }
    assert!(
        message_of(&json).contains("outside Go's standard library"),
        "{json}"
    );
}

/// The real go's `PATH` directory, or `None` to skip `test`.
fn real_go(test: &str) -> Option<std::path::PathBuf> {
    real_tool_dir("go", test)
}

#[test]
fn vet_the_real_go_finds_a_printf_verb_mistake_at_its_doc_line_and_passes_a_clean_file() {
    let Some(dir) = real_go(
        "vet_the_real_go_finds_a_printf_verb_mistake_at_its_doc_line_and_passes_a_clean_file",
    ) else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let wrong = project_vetting(
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Printf(\"%d\\n\", \"x\")\n}",
    );
    let right = project_vetting(
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Printf(\"%d\\n\", 1)\n}",
    );

    let fails = check_at_home(&wrong, &dir, home.path(), 1);
    let passes = check_at_home(&right, &dir, home.path(), 0);

    assert_eq!(finding_kinds(&fails), ["code-block-lint"], "{fails}");
    assert_eq!(at(&fails, "/findings/0/location/line"), 9, "{fails}");
    let message = message_of(&fails);
    assert!(
        message.contains("`go-vet` reports `printf`") && message.contains("%d"),
        "{fails}"
    );
    assert!(finding_kinds(&passes).is_empty(), "{passes}");
    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
}

#[test]
fn vet_the_real_go_reads_a_syntax_error_as_a_parse_error_at_its_doc_line() {
    let Some(dir) =
        real_go("vet_the_real_go_reads_a_syntax_error_as_a_parse_error_at_its_doc_line")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_vetting("package p\n\nfunc f() {\n\tx := <\n}");

    let json = check_at_home(&project, &dir, home.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 7, "{json}");
    let message = message_of(&json);
    assert!(message.contains("4:7: expected operand"), "{json}");
    assert!(!message.contains("supdoc/code-blocks"), "{json}");
}

#[test]
fn vet_the_real_go_fetches_nothing_for_an_import_outside_the_standard_library() {
    let Some(dir) =
        real_go("vet_the_real_go_fetches_nothing_for_an_import_outside_the_standard_library")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_vetting("package p\n\nimport \"example.com/x\"\n\nfunc F() { x.Do() }");

    let json = check_at_home(&project, &dir, home.path(), 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    let cache = home.path().join(".cache/supdoc/code-blocks");
    assert!(
        !cache.join("go-path/pkg/mod/cache/download").exists()
            && !cache.join("go-path/pkg/mod/example.com").exists(),
        "go vet should download nothing into its module cache"
    );
    let left: Vec<_> = std::fs::read_dir(cache.join("go"))
        .expect("the module directory's parent should exist")
        .collect();
    assert!(
        left.is_empty(),
        "the block's module should be removed: {left:?}"
    );
}

#[test]
fn vet_the_real_go_leaves_a_block_its_build_constraints_exclude_not_run() {
    let Some(dir) = real_go("vet_the_real_go_leaves_a_block_its_build_constraints_exclude_not_run")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_vetting("//go:build ignore\n\npackage p");

    let json = check_at_home(&project, &dir, home.path(), 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    assert!(message_of(&json).contains("build constraints"), "{json}");
}

#[test]
fn vet_the_real_go_applies_no_go_mod_go_work_or_go_env_planted_in_the_project_home_or_above_the_cache()
 {
    let Some(dir) = real_go(
        "vet_the_real_go_applies_no_go_mod_go_work_or_go_env_planted_in_the_project_home_or_above_the_cache",
    ) else {
        return;
    };
    let project = project_vetting("package p\n\nfunc F() int { return 1 }");
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let env = "GOFLAGS=-mod=vendor\nGOPROXY=https://127.0.0.1:9\nGOTOOLCHAIN=go9.99.0\nGOWORK=/nowhere/go.work\n";
    let plant = |base: &Path| {
        for config in [".config/go", "Library/Application Support/go"] {
            std::fs::create_dir_all(base.join(config))
                .expect("the planted config directory should be creatable");
            std::fs::write(base.join(config).join("env"), env)
                .expect("the plant should be writable");
        }
        std::fs::write(base.join("go.mod"), "module planted\n\ngo 9.99\n")
            .expect("the plant should be writable");
        std::fs::write(base.join("go.work"), "go 9.99\n\nuse ./nowhere\n")
            .expect("the plant should be writable");
    };
    let cache = home.path().join(".cache/supdoc/code-blocks");
    std::fs::create_dir_all(&cache).expect("the cache directory should be creatable");
    plant(home.path());
    plant(&home.path().join(".cache"));
    plant(&home.path().join(".cache/supdoc"));
    plant(&project.path("."));

    let json = check_at_home(&project, &dir, home.path(), 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}
