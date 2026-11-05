//! Custom `{ command = [...] }` formatters and linters: held back in an untrusted checkout, run
//! as written from the root in a trusted one.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::support::{Project, array_at, at, real_tool_dir};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// The first loose finding with `kind`'s `field`, as text.
fn finding_text(json: &serde_json::Value, kind: &str, field: &str) -> String {
    array_at(json, "/findings")
        .iter()
        .find(|finding| finding["kind"] == kind)
        .and_then(|finding| finding[field].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// A recorded `basic` project whose Python blocks are formatted by `command` (a TOML array) with
/// ruff's lint step off, and one `py` block, marked `info`, holding `body` in `docs/blocks.md`.
fn project_with(command: &str, info: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!(
            "[features.code-blocks]\n[features.code-blocks.python]\n\
             formatter = {{ command = {command} }}\n\
             [features.code-blocks.python.ruff]\nlint = \"off\"\n"
        ),
    );
    project.write(
        "docs/blocks.md",
        &format!(
            "# Blocks\n\n```{}\n{body}\n```\n",
            ["py", info].join(" ").trim_end()
        ),
    );
    project
}

/// Writes an executable `#!/bin/sh` script running `script` at `path`.
fn script(path: &Path, script: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{script}\n")).expect("the script should be writable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("the script should be made executable");
}

/// A directory outside the project holding the stub formatter `fmt-stub`, which appends each
/// call's arguments, working directory and environment to `log`, keeps its stdin in `stdin`,
/// then runs `then` with the stdin at `"$stdin"`.
struct Stub {
    /// Keeps the directory alive.
    dir: TempDir,
}

impl Stub {
    /// A stub that, after recording the call, runs `then`.
    fn new(then: &str) -> Self {
        let dir = TempDir::new().expect("a temporary directory should be creatable");
        let base = dir.path().display();
        script(
            &dir.path().join("fmt-stub"),
            &format!(
                "log='{base}/log'; stdin='{base}/stdin'\n\
                 printf 'call\\n' >> \"$log\"\n\
                 for arg in \"$@\"; do printf 'arg:%s\\n' \"$arg\" >> \"$log\"; done\n\
                 printf 'cwd:%s\\n' \"$(pwd -P)\" >> \"$log\"\n\
                 /usr/bin/env | /usr/bin/sed 's/^/env:/' >> \"$log\"\n\
                 /bin/cat > \"$stdin\"\n\
                 {then}"
            ),
        );
        Self { dir }
    }

    /// The stub's directory, to put on `PATH`.
    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The log, or `None` when the stub never ran.
    fn log(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join("log")).ok()
    }

    /// Each logged line with `prefix`, the prefix removed.
    fn logged(&self, prefix: &str) -> Vec<String> {
        self.log()
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(prefix).map(str::to_owned))
            .collect()
    }
}

/// `check --json` on `project` with `stub` on `PATH` and `vars`, expecting `exit`.
fn check(project: &Project, stub: &Stub, vars: &[(&str, &Path)], exit: i32) -> serde_json::Value {
    let mut all = vec![("PATH", stub.path())];
    all.extend_from_slice(vars);
    project
        .supdoc_in_with(".", &["check", "--json"], &all)
        .assert_exit(exit)
        .json()
}

/// The variable that trusts one run.
fn trusted() -> (&'static str, &'static Path) {
    ("SUPDOC_TRUST", Path::new("1"))
}

/// The stub's command, as config writes it.
const STUB: &str = "[\"fmt-stub\", \"--name\", \"{path}\", \"x{path}\"]";

#[test]
fn format_by_a_custom_command_is_held_without_trust_and_never_spawns() {
    for (vars, cause) in [
        (&[][..], "`supdoc trust`"),
        (&[("SUPDOC_TRUST", Path::new("0"))][..], "`SUPDOC_TRUST=0`"),
    ] {
        let stub = Stub::new("/bin/cat \"$stdin\"");
        let project = project_with(STUB, "", "x = 1");

        let json = check(&project, &stub, vars, 0);

        let reason = at(&json, "/data/code_blocks/0/reason")
            .as_str()
            .unwrap_or_default();
        assert!(
            reason.contains("custom formatter command") && reason.contains(cause),
            "the reason should name the custom formatter command and {cause}: {json}"
        );
        assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
        assert!(
            finding_text(&json, "code-block-not-trusted", "message").contains("`fmt-stub`"),
            "the trust warning should name the held program: {json}"
        );
        assert_eq!(stub.log(), None, "an untrusted command must never spawn");
    }
}

#[test]
fn format_by_a_custom_command_doesnt_run_under_tier_static_and_doesnt_warn() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    let project = project_with(STUB, "", "x = 1");
    project.replace(
        "supdoc.config.toml",
        "[features.code-blocks]\n",
        "[features.code-blocks]\ntier = \"static\"\n",
    );

    let json = check(&project, &stub, &[trusted()], 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    assert_eq!(
        stub.log(),
        None,
        "a lowered tier must never spawn the command"
    );
}

#[test]
fn format_by_a_trusted_custom_command_runs_as_written_from_the_root() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    // A default formatter on PATH must never stand in for the configured command.
    script(
        &stub.path().join("ruff"),
        &format!("printf 'ruff\\n' >> '{}/ruff.log'", stub.path().display()),
    );
    let project = project_with(STUB, "", "x = 1");

    let json = check(&project, &stub, &[trusted()], 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(
        stub.logged("arg:"),
        ["--name", "docs/blocks.md.1.py", "xdocs/blocks.md.1.py"],
        "argv should be as written, `{{path}}` the root-relative virtual path"
    );
    let stdin = std::fs::read_to_string(stub.path().join("stdin"))
        .expect("the stub should have kept its stdin");
    assert_eq!(stdin, "x = 1\n", "stdin should be the block exactly");
    let root: PathBuf = project
        .path(".")
        .canonicalize()
        .expect("the project root should resolve");
    assert_eq!(
        stub.logged("cwd:"),
        [root.display().to_string()],
        "the command should run from the root's real path"
    );
    // The shell itself exports `PWD`, `SHLVL` and `_`; everything else came from supdoc.
    let env: Vec<String> = stub
        .logged("env:")
        .into_iter()
        .filter(|var| {
            !["PWD=", "SHLVL=", "_=", "OLDPWD="]
                .iter()
                .any(|shell| var.starts_with(shell))
        })
        .collect();
    let mut names: Vec<&str> = env
        .iter()
        .filter_map(|var| var.split_once('=').map(|(name, _)| name))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["HOME", "NO_COLOR", "PATH"],
        "the env should be exactly ToolEnv's variables and NO_COLOR: {env:?}"
    );
    assert!(
        env.contains(&"NO_COLOR=1".to_owned())
            && env.contains(&format!("PATH={}", stub.path().display())),
        "NO_COLOR should be 1 and PATH supdoc's own: {env:?}"
    );
    assert!(
        !stub.path().join("ruff.log").exists(),
        "the default formatter must not run in place of the configured command"
    );
}

#[test]
fn format_by_a_custom_command_fails_a_block_it_would_change_and_fix_applies_it() {
    let stub = Stub::new("printf 'x = 1\\n'");
    let project = project_with(STUB, "", "x=1");

    let json = check(&project, &stub, &[trusted()], 1);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "custom formatter command `fmt-stub` would format it differently",
        "the reason should name the custom command, never a bare tool name: {json}"
    );
    let path = [("PATH", stub.path()), trusted()];
    let _fixed = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```py\nx = 1\n```\n",
        "`fix --yes` should write the command's output into the block"
    );
    let rechecked = check(&project, &stub, &[trusted()], 0);
    assert!(finding_kinds(&rechecked).is_empty(), "{rechecked}");
}

#[test]
fn format_by_a_custom_command_reads_output_as_the_fix_writes_it() {
    for printed in ["printf 'x = 1'", "printf 'x = 1\\r\\n'"] {
        let stub = Stub::new(printed);
        let project = project_with(STUB, "", "x = 1");

        let json = check(&project, &stub, &[trusted()], 0);

        assert!(
            finding_kinds(&json).is_empty(),
            "`{printed}` prints the block without its final newline or with CRLF, which a fix \
             would write back unchanged, so the block is formatted: {json}"
        );
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    }
}

#[test]
fn format_by_a_custom_command_gives_a_block_path_that_cant_read_as_an_option() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    let project = project_with(STUB, "", "x = 1");
    // The one block lives in `-x.md`, so the stub's single stdin file isn't shared.
    project.write("docs/blocks.md", "# Blocks\n");
    project.write("-x.md", "# Dash\n\n```py\ny = 2\n```\n");

    let _json = check(&project, &stub, &[trusted()], 0);

    assert!(
        stub.logged("arg:").contains(&"./-x.md.1.py".to_owned()),
        "a doc named `-x.md` at the root should be passed as `./-x.md.1.py`: {:?}",
        stub.logged("arg:")
    );
}

#[test]
fn format_by_a_custom_formatter_leaves_biome_lint_its_config() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    script(
        &stub.path().join("biome"),
        &format!(
            "for arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}/biome.log'; done",
            stub.path().display()
        ),
    );
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.json]\nformatter = {{ command = {STUB} }}\n"),
    );
    project.write("docs/blocks.md", "# Blocks\n\n```json\n{}\n```\n");

    let _json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", stub.path()), trusted()],
        )
        .json();

    let biome = std::fs::read_to_string(stub.path().join("biome.log")).unwrap_or_default();
    assert!(
        biome.lines().any(|arg| arg.starts_with("--config-path=")),
        "biome lint beside a custom formatter should still get its config: {biome:?}"
    );
}

#[test]
fn format_by_a_custom_command_that_prints_only_whitespace_doesnt_run() {
    let stub = Stub::new("printf '  \\n\\n'");
    let project = project_with(STUB, "", "x = 1");

    let json = check(&project, &stub, &[trusted()], 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
}

#[test]
fn format_by_a_custom_command_that_exits_non_zero_quotes_five_plain_stderr_lines() {
    let stub =
        Stub::new("printf '\\033[31mone\\033[0m\\ntwo\\nthree\\nfour\\nfive\\nsix\\n' >&2; exit 2");
    let project = project_with(STUB, "", "x = 1");

    let json = check(&project, &stub, &[trusted()], 1);

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
    let message = finding_text(&json, "code-block-tool-failed", "message");
    assert!(
        message.contains("one\ntwo\nthree\nfour\nfive")
            && !message.contains("six")
            && !message.contains('\u{1b}'),
        "the failure should quote five stderr lines without ANSI: {message:?}"
    );
}

#[test]
fn format_by_a_custom_command_past_the_timeout_is_a_timeout() {
    let stub = Stub::new("/bin/sleep 30");
    let project = project_with(STUB, "", "x = 1");

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json", "--code-block-timeout", "200ms"],
            &[("PATH", stub.path()), trusted()],
        )
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-timeout"], "{json}");
}

#[test]
fn format_by_a_missing_custom_script_says_it_doesnt_exist_under_the_root() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    let project = project_with("[\"scripts/none.sh\"]", "", "x = 1");

    let json = check(&project, &stub, &[trusted()], 0);

    let reason = at(&json, "/data/code_blocks/0/reason")
        .as_str()
        .unwrap_or_default();
    let message = finding_text(&json, "code-block-tool-missing", "message");
    for said in [reason, message.as_str()] {
        assert!(
            said.contains("custom formatter command `scripts/none.sh`")
                && said.contains("doesn't exist under the root")
                && !said.contains("PATH"),
            "a missing path program should be named as the custom command and not looked for on \
             PATH: {said:?}"
        );
    }
}

#[test]
fn format_by_a_missing_custom_program_warns_once_naming_it_and_its_key() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    let project = project_with("[\"no-such-fmt\", \"{path}\"]", "", "x = 1");
    project.write(
        "docs/more.md",
        "# More\n\n```py\nx = 1\n```\n\n```py\ny = 2\n```\n",
    );

    let json = check(&project, &stub, &[trusted()], 0);

    assert_eq!(finding_kinds(&json), ["code-block-tool-missing"], "{json}");
    let message = finding_text(&json, "code-block-tool-missing", "message");
    assert!(
        message.contains("`no-such-fmt`")
            && message.contains("`formatter` in `[features.code-blocks.python]`"),
        "the warning should name the program and the key that chose it: {message}"
    );
}

#[test]
fn format_by_a_custom_script_inside_the_root_runs_when_trusted() {
    let stub = Stub::new("");
    let project = project_with("[\"scripts/fmt.sh\"]", "", "x = 1");
    let log = stub.path().join("log");
    std::fs::create_dir_all(project.path("scripts")).expect("the scripts dir should be creatable");
    script(
        &project.path("scripts/fmt.sh"),
        &format!("printf 'call\\n' >> '{}'\n/bin/cat", log.display()),
    );

    let json = check(&project, &stub, &[trusted()], 0);

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(stub.logged("call"), [""], "the script should have run once");
}

#[test]
fn format_by_a_custom_command_doesnt_run_on_a_fragment() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    let project = project_with(STUB, "fragment(statements)", "x = 1");

    let json = check(&project, &stub, &[trusted()], 0);

    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    assert_eq!(stub.log(), None, "a fragment must never reach the command");
}

#[test]
fn format_by_a_custom_command_is_never_cached() {
    let stub = Stub::new("/bin/cat \"$stdin\"");
    let project = project_with(STUB, "", "x = 1");

    let _first = check(&project, &stub, &[trusted()], 0);
    let _second = check(&project, &stub, &[trusted()], 0);

    assert_eq!(
        stub.logged("call").len(),
        2,
        "each check should spawn the command again"
    );
}

#[test]
fn format_by_the_real_ruff_as_a_custom_command_passes_and_fails() {
    let Some(dir) = real_tool_dir(
        "ruff",
        "format_by_the_real_ruff_as_a_custom_command_passes_and_fails",
    ) else {
        return;
    };
    let command = "[\"ruff\", \"format\", \"--no-cache\", \"--stdin-filename\", \"{path}\", \"-\"]";
    let formatted = project_with(command, "", "x = 1");
    let unformatted = project_with(command, "", "x=1");
    let vars = [("PATH", dir.as_path()), trusted()];

    let passes = formatted
        .supdoc_in_with(".", &["check", "--json"], &vars)
        .assert_exit(0)
        .json();
    let fails = unformatted
        .supdoc_in_with(".", &["check", "--json"], &vars)
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
}

/// A recorded `basic` project whose Python blocks are linted by `linter` (a TOML inline table)
/// and ruff-format's format step off, and one `py` block holding `body` in `docs/blocks.md`: its fence is doc
/// line 3, its first code line doc line 4.
fn lint_project(linter: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!(
            "[features.code-blocks]\n[features.code-blocks.python]\nlinter = {linter}\n\
             [features.code-blocks.python.ruff-format]\nformat = \"off\"\n"
        ),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```py\n{body}\n```\n"),
    );
    project
}

/// The stub's command as a GNU linter, as config writes it.
const GNU_STUB: &str = "{ command = [\"fmt-stub\", \"{path}\"], diagnostics = \"gnu\" }";

/// The `(line, column, message)` of each loose `code-block-lint` finding.
fn lints(json: &serde_json::Value) -> Vec<(u64, u64, String)> {
    array_at(json, "/findings")
        .iter()
        .filter(|finding| finding["kind"] == "code-block-lint")
        .map(|finding| {
            (
                finding
                    .pointer("/location/line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                finding
                    .pointer("/location/column")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                finding["message"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[test]
fn lint_by_a_custom_command_is_held_without_trust_and_never_spawns() {
    for (vars, cause) in [
        (&[][..], "`supdoc trust`"),
        (&[("SUPDOC_TRUST", Path::new("0"))][..], "`SUPDOC_TRUST=0`"),
    ] {
        let stub = Stub::new("exit 1");
        let project = lint_project(GNU_STUB, "x = 1");

        let json = check(&project, &stub, vars, 0);

        let reason = at(&json, "/data/code_blocks/0/reason")
            .as_str()
            .unwrap_or_default();
        assert!(
            reason.contains("custom linter command") && reason.contains(cause),
            "the reason should name the custom linter command and {cause}: {json}"
        );
        assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
        assert_eq!(stub.log(), None, "an untrusted command must never spawn");
    }
}

#[test]
fn lint_and_format_by_custom_commands_without_trust_is_not_checked_needing_trust() {
    let stub = Stub::new("exit 1");
    let project = lint_project(GNU_STUB, "x = 1");
    project.write(
        "supdoc.config.toml",
        &format!(
            "[features.code-blocks]\n[features.code-blocks.python]\n\
             formatter = {{ command = {STUB} }}\nlinter = {GNU_STUB}\n"
        ),
    );

    let json = check(&project, &stub, &[], 0);

    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    let reason = at(&json, "/data/code_blocks/0/reason")
        .as_str()
        .unwrap_or_default();
    assert!(
        [
            "custom formatter command",
            "custom linter command",
            "`supdoc trust`"
        ]
        .iter()
        .all(|said| reason.contains(said)),
        "a block whose only due steps are custom should say each needs trust: {reason:?}"
    );
    assert_eq!(stub.log(), None, "an untrusted command must never spawn");
}

#[test]
fn lint_by_a_custom_command_doesnt_run_under_tier_static() {
    let stub = Stub::new("exit 1");
    let project = lint_project(GNU_STUB, "x = 1");
    project.replace(
        "supdoc.config.toml",
        "[features.code-blocks]\n",
        "[features.code-blocks]\ntier = \"static\"\n",
    );

    let json = check(&project, &stub, &[trusted()], 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    assert_eq!(
        stub.log(),
        None,
        "a lowered tier must never spawn the command"
    );
}

#[test]
fn lint_by_a_custom_command_that_exits_zero_passes_whatever_it_prints() {
    let stub = Stub::new("printf 'docs/blocks.md.1.py:1:1: error: x\\n'; printf 'warn\\n' >&2");
    let project = lint_project(GNU_STUB, "x = 1");

    let json = check(&project, &stub, &[trusted()], 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(
        stub.logged("arg:"),
        ["docs/blocks.md.1.py"],
        "the linter should get the root-relative virtual path"
    );
}

#[test]
fn lint_gnu_lines_land_at_their_block_lines_doc_line_and_column() {
    let cases = [
        ("printf 'docs/blocks.md.1.py:2:5: error: x\\n'", (5, 5)),
        (
            "printf '%s/docs/blocks.md.1.py:2:5: error: x\\n' \"$(pwd -P)\"",
            (5, 5),
        ),
        ("printf '<stdin>:1:1: error: x\\n'", (4, 1)),
        ("printf -- '-:2:3: error: x\\n' >&2", (5, 3)),
        ("printf 'stdin:1:2: error: x\\n'", (4, 2)),
        ("printf 'docs/blocks.md.1.py:2: error: x\\n'", (5, 1)),
    ];

    for (print, (line, column)) in cases {
        let stub = Stub::new(&format!("{print}; exit 1"));
        let project = lint_project(GNU_STUB, "x = 1\ny = 2");

        let json = check(&project, &stub, &[trusted()], 1);

        assert_eq!(
            lints(&json),
            [(
                line,
                column,
                "the custom linter command `fmt-stub` reports: error: x".to_owned()
            )],
            "`{print}` should give one lint at doc line {line}, column {column}: {json}"
        );
        assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
        assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
    }
}

#[test]
fn lint_gnu_lines_fail_the_block_whatever_severity_word_they_carry() {
    let stub = Stub::new(
        "printf '<stdin>:1:1: note: a\\n<stdin>:1:2: warning: b\\n<stdin>:1:3: info c\\n'; exit 1",
    );
    let project = lint_project(GNU_STUB, "x = 1");

    let json = check(&project, &stub, &[trusted()], 1);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-lint", "code-block-lint", "code-block-lint"],
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
}

#[test]
fn lint_lines_off_the_block_or_for_another_file_fall_back_to_one_lint_at_the_fence() {
    let printed = "docs/blocks.md.1.py:9:1: error: far\nother.py:1:1: error: o\ndocs/blocks.md.1.py:1:1:tight";
    let stub = Stub::new(&format!(
        "printf '{}\\n'; exit 1",
        printed.replace('\n', "\\n")
    ));
    let project = lint_project(GNU_STUB, "x = 1");

    let json = check(&project, &stub, &[trusted()], 1);

    let found = lints(&json);
    assert_eq!(found.len(), 1, "{json}");
    let (line, _, message) = found.first().expect("there should be one lint");
    assert_eq!(*line, 3, "the fallback should be at the fence: {json}");
    assert!(
        message.ends_with(&format!(":\n{printed}")),
        "the fallback should quote what the linter printed: {message:?}"
    );
}

#[test]
fn lint_without_diagnostics_quotes_twenty_plain_lines_stdout_then_stderr() {
    let stub = Stub::new(
        "i=1; while [ $i -le 15 ]; do echo \"out $i\"; i=$((i+1)); done; \
         i=1; while [ $i -le 10 ]; do printf '\\033[31merr %s\\033[0m\\n' $i >&2; i=$((i+1)); done; exit 1",
    );
    let project = lint_project("{ command = [\"fmt-stub\"] }", "x = 1");

    let json = check(&project, &stub, &[trusted()], 1);

    let found = lints(&json);
    assert_eq!(found.len(), 1, "{json}");
    let (line, _, message) = found.first().expect("there should be one lint");
    let expected: Vec<String> = (1..=15)
        .map(|n| format!("out {n}"))
        .chain((1..=5).map(|n| format!("err {n}")))
        .collect();
    assert_eq!(*line, 3, "the fallback should be at the fence: {json}");
    assert!(
        message.ends_with(&format!(":\n{}", expected.join("\n"))) && !message.contains('\u{1b}'),
        "the fallback should quote 15 stdout then 5 stderr lines without ANSI: {message:?}"
    );
}

#[test]
fn lint_by_a_custom_command_that_fails_printing_nothing_says_so() {
    let stub = Stub::new("exit 3");
    let project = lint_project(GNU_STUB, "x = 1");

    let json = check(&project, &stub, &[trusted()], 1);

    assert_eq!(
        lints(&json),
        [(
            3,
            1,
            "the custom linter command `fmt-stub` exited with a non-zero status on the block and \
             printed nothing"
                .to_owned()
        )],
        "{json}"
    );
}

#[test]
fn lint_by_a_custom_command_is_never_cached() {
    let stub = Stub::new("exit 0");
    let project = lint_project(GNU_STUB, "x = 1");

    let _first = check(&project, &stub, &[trusted()], 0);
    let _second = check(&project, &stub, &[trusted()], 0);

    assert_eq!(
        stub.logged("call").len(),
        2,
        "each check should spawn the linter again"
    );
}

#[test]
fn lint_by_the_real_ruff_as_a_custom_command_finds_an_unused_import() {
    let Some(dir) = real_tool_dir(
        "ruff",
        "lint_by_the_real_ruff_as_a_custom_command_finds_an_unused_import",
    ) else {
        return;
    };
    let project = lint_project(
        "{ command = [\"ruff\", \"check\", \"--no-cache\", \"--output-format=concise\", \
         \"--stdin-filename\", \"{path}\", \"-\"], diagnostics = \"gnu\" }",
        "x = 1\nimport os",
    );

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", dir.as_path()), trusted()],
        )
        .assert_exit(1)
        .json();

    let found = lints(&json);
    assert_eq!(found.len(), 1, "{json}");
    let (line, column, message) = found.first().expect("there should be one lint");
    assert_eq!((*line, *column), (5, 8), "{json}");
    assert!(message.contains("F401"), "{message:?}");
}

#[test]
fn lint_by_the_real_shellcheck_as_a_custom_command_maps_gcc_lines() {
    let Some(dir) = real_tool_dir(
        "shellcheck",
        "lint_by_the_real_shellcheck_as_a_custom_command_maps_gcc_lines",
    ) else {
        return;
    };
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.shell]\n\
         linter = { command = [\"shellcheck\", \"--format=gcc\", \"-\"], diagnostics = \"gnu\" }\n\
         [features.code-blocks.shell.shfmt]\nformat = \"off\"\n",
    );
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```sh\n#!/bin/sh\necho $1\n```\n",
    );

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", dir.as_path()), trusted()],
        )
        .assert_exit(1)
        .json();

    let found = lints(&json);
    assert_eq!(found.len(), 1, "{json}");
    let (line, column, message) = found.first().expect("there should be one lint");
    assert_eq!((*line, *column), (5, 6), "{json}");
    assert!(message.contains("SC2086"), "{message:?}");
}
