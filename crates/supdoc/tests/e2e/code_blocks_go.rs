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

/// The `check` the verdict tests run: JSON, and a time limit a cold toolchain cache stays under.
const CHECK: [&str; 4] = ["check", "--json", "--code-block-timeout", "120s"];

/// `supdoc check --json` in `project` with `PATH` set to `dir` alone, expecting `code`. The
/// time limit is generous, so a real `gofmt` on a cold cache reaches its verdict.
fn check(project: &Project, dir: &Path, code: i32) -> serde_json::Value {
    project
        .supdoc_in_with(".", &CHECK, &[("PATH", dir)])
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
    project_vetting_as("go", body)
}

/// [`project_vetting`] with the block's info string `info`.
fn project_vetting_as(info: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.go]\n\
         [features.code-blocks.go.gofmt]\nformat = \"off\"\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{info}\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `go` in `dir` that answers `go env GOVERSION`
/// with `go1.27.3` and runs `script` for anything else.
fn go_stub(dir: &Path, script: &str) {
    go_stub_of(dir, "go1.27.3", script);
}

/// Writes an executable `#!/bin/sh` stub named `go` in `dir` that answers `go env GOVERSION`
/// with `release` and runs `script` for anything else.
fn go_stub_of(dir: &Path, release: &str, script: &str) {
    let path = dir.join("go");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             if [ \"$*\" = 'env GOVERSION' ]; then echo '{release}'; exit 0; fi\n\
             {script}\n"
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// `supdoc check --json` in `project` with `PATH` set to `dir` and `HOME` to `home`, expecting
/// `code`. The time limit is generous, so a real `go vet` on a cold build cache reaches its
/// verdict.
fn check_at_home(project: &Project, dir: &Path, home: &Path, code: i32) -> serde_json::Value {
    project
        .supdoc_in_with(".", &CHECK, &[("PATH", dir), ("HOME", home)])
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
             [ \"$CGO_ENABLED$GOTOOLCHAIN$GOPROXY$GOWORK$GOENV$GOFLAGS\" = '0localoffoffoff-mod=readonly' ] || exit 4\n\
             here=\"$(pwd -P)\"\n\
             [ \"$(cd \"$GOTMPDIR\" && pwd -P)\" = \"$here/tmp\" ] || exit 9\n\
             for dir in \"$here\" \"$HOME\" \"$GOPATH\" \"$GOCACHE\"; do\n\
               case \"$dir\" in '{root}'*|'{home}'|'') exit 5;; esac\n\
             done\n\
             [ \"$(/bin/cat go.mod)\" = \"$(printf 'module supdoc.invalid/block\\n\\ngo 1.27')\" ] || exit 6\n\
             /usr/bin/grep -qx 'func f() {{}}' main.go || exit 7\n\
             [ \"$(/bin/ls)\" = \"$(printf 'go.mod\\nmain.go\\ntmp')\" ] || exit 8\n\
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
        "path=\"$(/usr/bin/sed -n 's/^import \"\\(.*\\)\"$/\\1/p' main.go)\"\n\
         echo \"main.go:3:8: cannot find module providing package $path: import lookup disabled \
         by -mod=readonly\" >&2\nexit 1",
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

#[test]
fn vet_a_go_naming_no_release_supdoc_reads_leaves_every_go_block_not_run_with_one_warning() {
    for printed in [
        "devel go1.28-abc123",
        "go1.27rc1",
        "go1.27.3 X:boringcrypto",
    ] {
        let project = project_vetting("package p");
        project.write(
            "docs/blocks.md",
            "# Blocks\n\n```go\npackage p\n```\n\n```go\npackage q\n```\n",
        );
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        go_stub_of(outside.path(), printed, "exit 3");

        let json = check(&project, outside.path(), 0);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "`{printed}`: {json}"
        );
        for block in 0..2 {
            assert_eq!(
                at(&json, &format!("/data/code_blocks/{block}/lint")),
                "not_run",
                "`{printed}`: {json}"
            );
        }
        assert!(
            message_of(&json).contains(&format!("printed `{printed}`")),
            "the warning should name what `go env GOVERSION` printed: {json}"
        );
    }
}

#[test]
fn vet_a_timed_out_run_leaves_nothing_beside_the_module_and_its_work_files_were_private() {
    let project = project_vetting("package p");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let seen = outside.path().join("gotmpdir");
    go_stub(
        outside.path(),
        &format!(
            "cd \"$GOTMPDIR\" && pwd -P > '{seen}'\n\
             /bin/mkdir \"$GOTMPDIR/go-build1\" && /usr/bin/touch \"$GOTMPDIR/go-build1/x\"\n\
             /bin/sleep 30",
            seen = seen.display()
        ),
    );

    let _ = project
        .supdoc_in_with(
            ".",
            &["check", "--json", "--code-block-timeout", "500ms"],
            &[("PATH", outside.path()), ("HOME", home.path())],
        )
        .assert_exit(1);

    let modules = home
        .path()
        .join(".cache/supdoc/code-blocks/go")
        .canonicalize()
        .expect("the modules' directory should exist");
    let gotmpdir = std::fs::read_to_string(&seen).expect("the stub should record GOTMPDIR");
    let gotmpdir = Path::new(gotmpdir.trim_end());
    assert!(
        gotmpdir.starts_with(&modules) && gotmpdir.ends_with("tmp"),
        "GOTMPDIR should be the module's own tmp: {}",
        gotmpdir.display()
    );
    let left: Vec<_> = std::fs::read_dir(&modules)
        .expect("the modules' directory should be readable")
        .collect();
    assert!(
        left.is_empty(),
        "the timed-out module should be removed: {left:?}"
    );
}

#[test]
fn vet_the_real_go_reads_a_block_at_its_own_release_so_a_newer_feature_passes() {
    let Some(dir) =
        real_go("vet_the_real_go_reads_a_block_at_its_own_release_so_a_newer_feature_passes")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    // `new` with an expression needs Go 1.26.
    let project = project_vetting("package p\n\nvar p = new(42)");

    let json = check_at_home(&project, &dir, home.path(), 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn vet_the_real_go_fails_a_type_error_quoting_go_s_import_words_rather_than_not_running() {
    let Some(dir) = real_go(
        "vet_the_real_go_fails_a_type_error_quoting_go_s_import_words_rather_than_not_running",
    ) else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for words in [
        "is not in std",
        "cannot find module providing package x",
        "build constraints exclude all Go files in .",
    ] {
        let project = project_vetting(&format!("package p\n\nvar x int = \"{words}\""));

        let json = check_at_home(&project, &dir, home.path(), 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-lint"],
            "`{words}`: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/location/line"),
            6,
            "`{words}`: {json}"
        );
        assert!(
            message_of(&json).contains("`go-vet` reports `typecheck`"),
            "`{words}`: {json}"
        );
    }
}

#[test]
fn vet_the_real_go_places_a_type_error_after_a_line_directive_naming_any_file_at_the_fence() {
    let Some(dir) = real_go(
        "vet_the_real_go_places_a_type_error_after_a_line_directive_naming_any_file_at_the_fence",
    ) else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_vetting("package p\n\n//line /etc/passwd:1\nvar x int = \"s\"");

    let json = check_at_home(&project, &dir, home.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 3, "{json}");
    let message = message_of(&json);
    assert!(message.contains("`go-vet` reports `typecheck`"), "{json}");
    assert!(
        !message.contains("vet:") && !message.contains("/etc/passwd"),
        "the message should not be go's raw stderr: {json}"
    );
}

#[test]
fn vet_the_real_go_places_a_use_of_an_internal_package_at_its_doc_line() {
    let Some(dir) = real_go("vet_the_real_go_places_a_use_of_an_internal_package_at_its_doc_line")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_vetting("package p\n\nimport _ \"internal/abi\"");

    let json = check_at_home(&project, &dir, home.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    assert!(
        message_of(&json).contains("use of internal package"),
        "{json}"
    );
}

#[test]
fn vet_the_real_go_fails_a_block_whose_line_directive_names_gos_load_words() {
    let Some(dir) =
        real_go("vet_the_real_go_fails_a_block_whose_line_directive_names_gos_load_words")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_vetting(
        "package p\nimport (\n//line main.go:3:8: cannot find module providing package x:1\n  \
         \"fmt\" +\n)",
    );

    let json = check_at_home(&project, &dir, home.path(), 1);

    let kinds = finding_kinds(&json);
    assert!(
        kinds == ["code-block-parse"] || kinds == ["code-block-lint"],
        "the syntax error should fail the block, not leave it unchecked: {json}"
    );
    assert_ne!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
}

#[test]
fn vet_the_real_go_fails_a_block_whose_import_path_writes_gos_lines() {
    let Some(dir) = real_go("vet_the_real_go_fails_a_block_whose_import_path_writes_gos_lines")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for path in [
        "a\\nmain.go:1:1: cannot find module providing package b",
        "a\\n\\tmain.go:1:1: cannot find module providing package b",
        "a\\npackage supdoc.invalid/block: build constraints exclude all Go files in .",
    ] {
        let project = project_vetting(&format!("package p\n\nimport _ \"{path}\""));

        let json = check_at_home(&project, &dir, home.path(), 1);

        assert!(
            finding_kinds(&json)
                .iter()
                .all(|kind| kind == "code-block-lint"),
            "`{path}`: {json}"
        );
        assert_eq!(
            at(&json, "/findings/0/location/line"),
            6,
            "`{path}`: {json}"
        );
        assert!(
            message_of(&json).contains("invalid import path"),
            "`{path}`: {json}"
        );
        assert_eq!(
            at(&json, "/data/code_blocks/0/lint"),
            "ran",
            "`{path}`: {json}"
        );
    }
}

/// The names go vet's frames hold that no block writes: neither may show in any finding.
const FRAME_NAMES: [&str; 2] = ["supdocfragment", "supdocFragment"];

/// Asserts no finding of `json` holds a name of go vet's frames.
fn assert_no_frame_text(json: &serde_json::Value) {
    let findings = at(json, "/findings").to_string();
    for name in FRAME_NAMES {
        assert!(!findings.contains(name), "{name} in {json}");
    }
}

#[test]
fn frame_vets_a_declarations_block_with_no_package_clause_behind_the_frame() {
    let Some(dir) =
        real_go("frame_vets_a_declarations_block_with_no_package_clause_behind_the_frame")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let wrong = "import \"fmt\"\n\nfunc show() {\n\tfmt.Printf(\"%d\\n\", \"x\")\n}";
    let right = "import \"fmt\"\n\nfunc show() {\n\tfmt.Printf(\"%d\\n\", 1)\n}";
    let marked = |body| project_vetting_as("go fragment(declarations)", body);

    let fails = check_at_home(&marked(wrong), &dir, home.path(), 1);
    let passes = check_at_home(&marked(right), &dir, home.path(), 0);
    let unmarked = check_at_home(&project_vetting(right), &dir, home.path(), 1);

    assert_eq!(finding_kinds(&fails), ["code-block-lint"], "{fails}");
    assert_eq!(at(&fails, "/findings/0/location/line"), 7, "{fails}");
    assert!(
        message_of(&fails).contains("`go-vet` reports `printf`"),
        "{fails}"
    );
    assert_no_frame_text(&fails);
    assert!(finding_kinds(&passes).is_empty(), "{passes}");
    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&unmarked), ["code-block-lint"], "{unmarked}");
    assert!(
        message_of(&unmarked).contains("no `package` clause"),
        "{unmarked}"
    );
}

#[test]
fn frame_vets_a_statements_block_inside_a_function() {
    let Some(dir) = real_go("frame_vets_a_statements_block_inside_a_function") else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let marked = |body| project_vetting_as("go fragment(statements)", body);

    let fails = check_at_home(&marked("x := 1\nx = x"), &dir, home.path(), 1);
    let passes = check_at_home(&marked("x := 1\n_ = x"), &dir, home.path(), 0);

    assert_eq!(finding_kinds(&fails), ["code-block-lint"], "{fails}");
    assert_eq!(at(&fails, "/findings/0/location/line"), 5, "{fails}");
    assert!(
        message_of(&fails).contains("`go-vet` reports `assign`"),
        "{fails}"
    );
    assert_no_frame_text(&fails);
    assert!(finding_kinds(&passes).is_empty(), "{passes}");
}

#[test]
fn frame_reads_a_block_that_isnt_its_kind_as_one_parse_error_without_gos_message() {
    let Some(dir) =
        real_go("frame_reads_a_block_that_isnt_its_kind_as_one_parse_error_without_gos_message")
    else {
        return;
    };
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let project = project_vetting_as(
        "go fragment(declarations)",
        "package main\n\nfunc main() {}",
    );

    let json = check_at_home(&project, &dir, home.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    assert!(
        message_of(&json).contains("couldn't parse the block as `declarations`"),
        "{json}"
    );
    assert_no_frame_text(&json);
}

/// A recorded `basic` project with the go table, go vet off, and `source` at `src/p.go`.
fn doc_project(source: &str) -> Project {
    let project = project_with_block("go", "x := 1");
    project.write("docs/blocks.md", "# Blocks\n");
    project.write("src/p.go", source);
    project
}

/// A doc comment on a func holding one `go` block whose body is `body`, and an indented Go code
/// block that is text.
fn doc_source(body: &str) -> String {
    format!(
        "package p\n\n// Add adds.\n//\n//\tx:=1\n//\n// ```go\n// {body}\n// ```\nfunc Add() {{}}\n"
    )
}

#[test]
fn doc_comment_block_is_checked_at_its_file_line_and_fixed_behind_its_marker() {
    let project = doc_project(&doc_source("x:=1"));
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    gofmt_stub(outside.path(), "/bin/cat >/dev/null\necho 'x := 1'");

    let json = check(&project, outside.path(), 1);
    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())])
        .assert_exit(0);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 7, "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/failed"),
        1,
        "only the fenced block is a block, not the indented one: {json}"
    );
    assert_eq!(
        project.read("src/p.go"),
        doc_source("x := 1"),
        "the fix should rewrite the block's line behind its `// ` marker"
    );
}

#[test]
fn doc_a_comment_apart_from_its_declaration_holds_no_block() {
    let source = "package p\n\n// ```go\n// x:=1\n// ```\n\nfunc Add() {}\n";
    let project = doc_project(source);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    gofmt_stub(outside.path(), "/bin/cat >/dev/null\necho 'x := 1'");

    let json = check(&project, outside.path(), 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

#[test]
fn doc_the_real_gofmt_checks_and_fixes_a_doc_comments_block() {
    let Some(dir) = real_gofmt("doc_the_real_gofmt_checks_and_fixes_a_doc_comments_block") else {
        return;
    };
    let formatted = doc_project(&doc_source("x := 1"));
    let unformatted = doc_project(&doc_source("x:=1"));

    let passes = check(&formatted, &dir, 0);
    let fails = check(&unformatted, &dir, 1);
    let _fix = unformatted
        .supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)])
        .assert_exit(0);

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    assert_eq!(unformatted.read("src/p.go"), doc_source("x := 1"));
}
