//! Custom `{ command = [...] }` formatters: held back in an untrusted checkout, run as written
//! from the root in a trusted one.

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
