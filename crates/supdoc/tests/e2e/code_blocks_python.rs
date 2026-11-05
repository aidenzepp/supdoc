//! Checking Python code blocks.

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

/// A recorded `basic` project with the python table (and `config` after it) and one `py` block
/// holding `body` in `docs/blocks.md`.
fn project_with_python_block(config: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.python]\n{config}"),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```py\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `name` running `script` in `dir`.
fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

#[test]
fn ruff_runs_with_no_cache_and_the_blocks_virtual_path_beside_the_doc() {
    let project = project_with_python_block("", "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "ruff",
        "[ \"$1\" = check ] && exit 0\n\
         [ \"$1 $2 $3 $5\" = 'format --no-cache --stdin-filename -' ] || exit 3\n\
         case \"$4\" in */docs/blocks.md.1.py) /bin/cat;; *) exit 4;; esac",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn black_when_selected_runs_quietly_and_names_itself_in_the_reason() {
    let project = project_with_python_block(
        "formatter = \"black\"\n[features.code-blocks.python.ruff]\nlint = \"off\"\n",
        "x=1",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "black",
        "[ \"$1 $2 $4\" = '--quiet --stdin-filename -' ] || exit 3\necho 'x = 1'",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`black` would format it differently",
        "{json}"
    );
}

#[test]
fn the_real_ruff_passes_a_formatted_block_and_fails_another() {
    let Some(dir) = real_tool_dir(
        "ruff",
        "the_real_ruff_passes_a_formatted_block_and_fails_another",
    ) else {
        return;
    };
    let formatted = project_with_python_block("", "x = 1");
    let unformatted = project_with_python_block("", "x=1");

    let passes = formatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = unformatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
}

#[test]
fn the_real_black_passes_a_formatted_block_and_fails_another() {
    let Some(dir) = real_tool_dir(
        "black",
        "the_real_black_passes_a_formatted_block_and_fails_another",
    ) else {
        return;
    };
    let config = "formatter = \"black\"\n[features.code-blocks.python.ruff]\nlint = \"off\"\n";
    let formatted = project_with_python_block(config, "x = 1");
    let unformatted = project_with_python_block(config, "x=1");

    let passes = formatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = unformatted
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    assert_eq!(
        at(&fails, "/data/code_blocks/0/reason"),
        "`black` would format it differently",
        "{fails}"
    );
}

/// A `ruff` stub that formats by echoing its input and lints by running `check`, a script that
/// sees ruff's `check` arguments as `$@`.
fn ruff_stub(dir: &Path, check: &str) {
    stub(
        dir,
        "ruff",
        &format!("if [ \"$1\" = format ]; then /bin/cat; exit 0; fi\nshift\n{check}"),
    );
}

#[test]
fn lint_diagnostics_from_ruff_are_lint_errors_at_their_doc_line_and_column() {
    let project = project_with_python_block("", "x = 1\nif x == None:\n    pass");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    ruff_stub(
        outside.path(),
        "[ \"$1 $2 $3 $5 $6 $7\" = '--no-cache --output-format=json --stdin-filename --ignore F821,F401,F841 -' ] || exit 3\n\
         case \"$4\" in */docs/blocks.md.1.py) ;; *) exit 4;; esac\n\
         echo '[{\"code\":\"E711\",\"message\":\"Comparison to `None`\",\"location\":{\"row\":2,\"column\":6}}]'\n\
         exit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert_eq!(at(&json, "/findings/0/location/column"), 6, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn lint_exit_other_than_diagnostics_is_a_tool_failure() {
    let project = project_with_python_block("", "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    ruff_stub(outside.path(), "echo 'ruff failed: bad config' >&2\nexit 2");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
}

#[test]
fn lint_does_not_run_on_a_block_its_formatter_cannot_parse() {
    let project = project_with_python_block("", "x = (");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "ruff",
        "if [ \"$1\" = check ]; then echo '[{\"code\":\"E999\",\"message\":\"m\",\"location\":{\"row\":1,\"column\":1}}]'; exit 1; fi\n\
         echo \"error: Failed to parse $4:1:5: Expected an expression\" >&2\nexit 2",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
}

#[test]
fn lint_with_the_formatter_off_still_runs() {
    let project = project_with_python_block(
        "[features.code-blocks.python.ruff-format]\nformat = \"off\"\n",
        "x = 1",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    ruff_stub(outside.path(), "exit 0");

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "off", "{json}");
}

#[test]
fn lint_with_the_real_ruff_reports_a_rule_the_project_selects() {
    let Some(dir) = real_tool_dir(
        "ruff",
        "lint_with_the_real_ruff_reports_a_rule_the_project_selects",
    ) else {
        return;
    };
    let config = "[lint]\nselect = [\"E711\", \"F401\"]\n";
    let clean = project_with_python_block("", "x = 1");
    let flagged = project_with_python_block("", "import os\n\nx = 1\nif x == None:\n    pass");
    clean.write("ruff.toml", config);
    flagged.write("ruff.toml", config);

    let passes = clean
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = flagged
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(at(&passes, "/data/code_blocks/0/lint"), "ran", "{passes}");
    assert_eq!(finding_kinds(&fails), ["code-block-lint"], "{fails}");
    assert_eq!(at(&fails, "/findings/0/location/line"), 7, "{fails}");
}

#[test]
fn ruff_format_and_ruff_missing_share_one_warning_naming_the_ruff_binary() {
    let project = project_with_python_block("", "x = 1");
    let empty = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", empty.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-missing"], "{json}");
    assert_eq!(
        at(&json, "/findings/0/message"),
        "`ruff` isn't on PATH, so its blocks aren't checked",
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`ruff` isn't on PATH",
        "{json}"
    );
}

/// A recorded `basic` project with a `py` docstring block in `src/blocks.py`, and a directory
/// holding a stub `ruff` that formats every block to `x = 1`.
fn docstring_project(source: &str) -> (Project, tempfile::TempDir) {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.python]\n[features.code-blocks.python.ruff]\nlint = \"off\"\n",
    );
    project.write("src/blocks.py", source);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(outside.path(), "ruff", "/bin/cat > /dev/null\necho 'x = 1'");
    (project, outside)
}

#[test]
fn docstring_block_is_checked_at_its_file_line() {
    let source = "class C:\n    def f(self):\n        \"\"\"F.\n\n        ```python\n        x=1\n        ```\n        \"\"\"\n";
    let (project, outside) = docstring_project(source);

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn docstring_fix_keeps_the_docstrings_indentation() {
    let source = "class C:\n    def f(self):\n        \"\"\"F.\n\n        ```python\n        x=1\n        ```\n        \"\"\"\n";
    let (project, outside) = docstring_project(source);
    let path = [("PATH", outside.path())];

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);

    assert_eq!(
        project.read("src/blocks.py"),
        source.replace("x=1", "x = 1"),
        "the fix should rewrite the block's line and keep its 8-space indent"
    );
}

#[test]
fn docstring_fix_that_would_close_the_docstring_is_never_written() {
    let source = "class C:\n    def f(self):\n        \"\"\"F.\n\n        ```python\n        x=1\n        ```\n        \"\"\"\n";
    let (project, outside) = docstring_project(source);
    stub(
        outside.path(),
        "ruff",
        "/bin/cat > /dev/null\necho 'x = \"\"\"'",
    );
    let path = [("PATH", outside.path())];

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);
    let json = project
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(1)
        .json();

    assert_eq!(
        project.read("src/blocks.py"),
        source,
        "a fix ending the docstring would move the closing fence out of it"
    );
    assert_eq!(
        finding_kinds(&json),
        ["code-block-unformatted", "code-block-fix-unsafe"],
        "{json}"
    );
}

#[test]
fn docstring_doctest_session_and_literal_block_are_not_checked() {
    let source = "def f():\n    \"\"\"F::\n\n        x=1\n\n    >>> x=1\n    \"\"\"\n";
    let (project, outside) = docstring_project(source);

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

/// The config that selects pylint, with ruff format's step off so pylint is the block's one tool.
const PYLINT: &str =
    "linter = \"pylint\"\n[features.code-blocks.python.ruff-format]\nformat = \"off\"\n";

/// The reason a block gives when pylint was held because the checkout isn't trusted.
const PYLINT_NEEDS_TRUST: &str = "pylint runs only in the project tier, which needs `supdoc trust`";

/// A pylint report with no message that linted `modules` modules, as a shell `printf` argument.
fn empty_report(modules: usize) -> String {
    format!("'{{\"messages\": [], \"statistics\": {{\"modulesLinted\": {modules}}}}}'")
}

/// Writes a `pylint` stub in `dir` that appends its working directory and arguments to
/// `<dir>/pylint-runs`, one line per run, then runs `script`.
fn pylint_stub(dir: &Path, script: &str) {
    let record = dir.join("pylint-runs");
    stub(
        dir,
        "pylint",
        &format!("echo \"$(pwd -P) $*\" >> '{}'\n{script}", record.display()),
    );
}

/// The lines `pylint_stub` in `dir` recorded: one per run.
fn pylint_runs(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("pylint-runs"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Runs a trusted `check --json` on `project` with `path` as the only `PATH` entry and its cache
/// in `cache`, asserting it exits `exit`.
fn trusted_pylint_check(
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
fn pylint_runs_from_the_root_with_its_argv_and_spawns_again_on_a_second_run() {
    let project = project_with_python_block(PYLINT, "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    pylint_stub(outside.path(), &format!("printf {}", empty_report(1)));

    let first = trusted_pylint_check(&project, outside.path(), cache.path(), 0);
    let second = trusted_pylint_check(&project, outside.path(), cache.path(), 0);

    let root = project
        .path(".")
        .canonicalize()
        .expect("the project root should resolve");
    let runs = pylint_runs(outside.path());
    assert_eq!(
        runs.len(),
        2,
        "pylint is never cached, so each run spawns it: {runs:?}"
    );
    for run in &runs {
        let (cwd, args) = run
            .split_once(' ')
            .expect("the record names a cwd and args");
        assert_eq!(Path::new(cwd), root, "pylint runs from the root: {run}");
        let args: Vec<&str> = args.split(' ').collect();
        let [
            "--from-stdin",
            path,
            "--output-format=json2",
            "--persistent=n",
            "--disable=W0611,W0612,C0114,E0602",
        ] = args.as_slice()
        else {
            panic!("pylint should get its five arguments, `--persistent=n` among them: {run}");
        };
        assert!(path.ends_with("/docs/blocks.md.1.py"), "{run}");
    }
    for json in [first, second] {
        assert!(finding_kinds(&json).is_empty(), "{json}");
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    }
}

#[test]
fn pylint_in_the_projects_venv_runs_before_one_on_path() {
    let project = project_with_python_block(PYLINT, "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let venv = project.path(".venv/bin");
    std::fs::create_dir_all(&venv).expect("the venv bin directory should be creatable");
    pylint_stub(&venv, &format!("printf {}", empty_report(1)));
    pylint_stub(outside.path(), "exit 9");

    let json = trusted_pylint_check(&project, outside.path(), cache.path(), 0);

    assert_eq!(
        pylint_runs(&venv).len(),
        1,
        "the root's pylint runs: {json}"
    );
    assert!(
        pylint_runs(outside.path()).is_empty(),
        "the pylint of the same name on PATH never runs: {json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn pylint_skip_file_in_the_block_is_lint_not_run_naming_lint_off() {
    let project = project_with_python_block(PYLINT, "# pylint: skip-file\nx = 1");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```py\n# pylint: skip-file\nx = 1\n```\n\n```py\ny = 2  # pylint: disable=all\n```\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    pylint_stub(outside.path(), &format!("printf {}", empty_report(1)));

    let json = trusted_pylint_check(&project, outside.path(), cache.path(), 0);

    assert_eq!(
        at(&json, "/data/code_blocks/1/lint"),
        "not_run",
        "the second block is switched off too, under the run's one warning: {json}"
    );

    assert!(
        pylint_runs(outside.path()).is_empty(),
        "pylint never starts: {json}"
    );
    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/findings/0/severity"), "warning", "{json}");
    let help = at(&json, "/findings/0/help");
    assert!(
        help.as_str()
            .is_some_and(|help| help.contains("lint(off: <reason>)")),
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason
            .as_str()
            .is_some_and(|reason| reason.contains("lint(off: <reason>)")),
        "{json}"
    );
}

#[test]
fn pylint_that_linted_no_module_is_lint_not_run() {
    let project = project_with_python_block(PYLINT, "x = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    pylint_stub(outside.path(), &format!("printf {}", empty_report(0)));

    let json = trusted_pylint_check(&project, outside.path(), cache.path(), 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason.as_str().is_some_and(|reason| reason.ends_with(
            "pylint linted no module: the project's pylint config leaves the block out"
        )),
        "{json}"
    );
}

#[test]
fn pylint_trusted_reports_a_finding_at_its_doc_line() {
    let Some(dir) = real_tool_dir("pylint", "pylint_trusted_reports_a_finding_at_its_doc_line")
    else {
        return;
    };
    let project = project_with_python_block(PYLINT, "X = 1\nif X == None:\n    pass");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_pylint_check(&project, &dir, cache.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert_eq!(at(&json, "/findings/0/location/column"), 4, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn pylint_syntax_error_is_a_parse_error_at_its_doc_line() {
    let Some(dir) = real_tool_dir(
        "pylint",
        "pylint_syntax_error_is_a_parse_error_at_its_doc_line",
    ) else {
        return;
    };
    let project = project_with_python_block(PYLINT, "X = 1\ndef f(:\n    pass");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_pylint_check(&project, &dir, cache.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn pylint_untrusted_is_lint_not_run_naming_trust() {
    let Some(dir) = real_tool_dir("pylint", "pylint_untrusted_is_lint_not_run_naming_trust") else {
        return;
    };
    let project = project_with_python_block(PYLINT, "X = 1\nif X == None:\n    pass");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
    let message = at(&json, "/findings/0/message");
    assert!(
        message
            .as_str()
            .is_some_and(|message| message.contains("pylint didn't run")),
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason
            .as_str()
            .is_some_and(|reason| reason.ends_with(PYLINT_NEEDS_TRUST)),
        "{json}"
    );
}

/// A pylint project with two clean blocks in `docs/blocks.md`.
fn two_block_pylint_project() -> Project {
    let project = project_with_python_block(PYLINT, "X = 1");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```py\nX = 1\n```\n\n```py\nY = 2\n```\n",
    );
    project
}

/// Asserts `json` holds exactly one finding, the run's warning about the project's `.pylintrc`
/// naming `E0015`, and that both blocks passed.
fn assert_one_config_warning(json: &serde_json::Value) {
    assert_eq!(finding_kinds(json), ["code-block-not-checked"], "{json}");
    let message = at(json, "/findings/0/message");
    assert!(
        message
            .as_str()
            .is_some_and(|message| message.contains("`E0015` in `.pylintrc`")),
        "{json}"
    );
    assert_eq!(at(json, "/data/summary/code_blocks/passed"), 2, "{json}");
}

#[test]
fn pylint_messages_about_the_projects_rcfile_are_one_warning_never_block_findings() {
    let project = two_block_pylint_project();
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    pylint_stub(
        outside.path(),
        "printf '{\"messages\": [{\"type\": \"error\", \"symbol\": \"unrecognized-option\", \
         \"message\": \"Unrecognized option found: bogus-option\", \"messageId\": \"E0015\", \
         \"module\": \"x\", \"obj\": \"\", \"line\": 1, \"column\": 0, \"endLine\": null, \
         \"path\": \".pylintrc\", \"absolutePath\": \"'\"$(pwd -P)\"'/.pylintrc\"}], \
         \"statistics\": {\"modulesLinted\": 1}}'\nexit 2",
    );

    let json = trusted_pylint_check(&project, outside.path(), cache.path(), 0);

    assert_eq!(pylint_runs(outside.path()).len(), 2, "{json}");
    assert_one_config_warning(&json);
}

#[test]
fn pylint_real_rcfile_with_an_unknown_option_is_one_warning_across_two_blocks() {
    let Some(dir) = real_tool_dir(
        "pylint",
        "pylint_real_rcfile_with_an_unknown_option_is_one_warning_across_two_blocks",
    ) else {
        return;
    };
    let project = two_block_pylint_project();
    project.write(".pylintrc", "[MAIN]\nbogus-option = 1\n");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_pylint_check(&project, &dir, cache.path(), 0);

    assert_one_config_warning(&json);
}

#[test]
fn pylint_exiting_fatal_or_usage_with_no_message_is_a_tool_failure() {
    for code in [1, 32] {
        let project = project_with_python_block(PYLINT, "X = 1");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        pylint_stub(
            outside.path(),
            &format!("printf {}\nexit {code}", empty_report(1)),
        );

        let json = trusted_pylint_check(&project, outside.path(), cache.path(), 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-tool-failed"],
            "exit {code}: {json}"
        );
        assert_eq!(
            at(&json, "/data/code_blocks/0/state"),
            "failed",
            "exit {code}: {json}"
        );
    }
}

#[test]
fn pylint_missing_names_the_venv_bin_dir_it_searched() {
    let project = project_with_python_block(PYLINT, "X = 1");
    let empty = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_pylint_check(&project, empty.path(), cache.path(), 0);

    assert_eq!(finding_kinds(&json), ["code-block-tool-missing"], "{json}");
    assert_eq!(
        at(&json, "/findings/0/message"),
        "`pylint` isn't in .venv/bin or on PATH, so its blocks aren't checked",
        "{json}"
    );
    let help = at(&json, "/findings/0/help");
    assert!(
        help.as_str().is_some_and(|help| help.contains(".venv/bin")),
        "{json}"
    );
}

#[test]
fn pylint_comment_naming_more_than_messages_is_not_run_with_wording_true_for_each() {
    for body in [
        "# pylint: disable=fixme\nx = 1",
        "x = 1  # pylint: disable=W",
        "# pylint: skip-file\nx = 1",
    ] {
        let project = project_with_python_block(PYLINT, body);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        pylint_stub(outside.path(), &format!("printf {}", empty_report(1)));

        let json = trusted_pylint_check(&project, outside.path(), cache.path(), 0);

        assert!(
            pylint_runs(outside.path()).is_empty(),
            "{body:?}: pylint never starts: {json}"
        );
        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        let message = at(&json, "/findings/0/message")
            .as_str()
            .map(str::to_owned)
            .unwrap_or_default();
        assert!(
            message.contains("can't confirm pylint still lints the block")
                && !message.contains("whole block"),
            "{body:?}: the warning claims only what supdoc checks: {message}"
        );
        let help = at(&json, "/findings/0/help")
            .as_str()
            .map(str::to_owned)
            .unwrap_or_default();
        assert!(
            help.contains("`W0511` for `fixme`") && help.contains("lint(off: <reason>)"),
            "{body:?}: {help}"
        );
        assert_eq!(
            at(&json, "/data/code_blocks/0/lint"),
            "not_run",
            "{body:?}: {json}"
        );
        let reason = at(&json, "/data/code_blocks/0/reason");
        assert!(
            reason
                .as_str()
                .is_some_and(|reason| reason.contains("can't confirm pylint still lints it")),
            "{body:?}: {json}"
        );
    }
}

#[test]
fn pylint_missing_block_reason_names_the_venv_bin_dir_too() {
    let project = project_with_python_block(PYLINT, "X = 1");
    let empty = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    let json = trusted_pylint_check(&project, empty.path(), cache.path(), 0);

    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason
            .as_str()
            .is_some_and(|reason| reason.contains("`pylint` isn't in .venv/bin or on PATH")),
        "{json}"
    );
}
