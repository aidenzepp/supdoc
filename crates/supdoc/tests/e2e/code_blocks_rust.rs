//! Checking Rust code blocks, in Markdown and in doc comments.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::support::{Project, array_at, at, real_tool_dir};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// A recorded `basic` project with the Rust table on, its linter off so only rustfmt runs, and
/// `doc` at `docs/blocks.md`.
fn project_with_rust_doc(doc: &str) -> Project {
    project_with_rust_config("[features.code-blocks.rust.clippy]\nlint = \"off\"\n", doc)
}

/// A recorded `basic` project whose Rust table holds `rust`, with `doc` at `docs/blocks.md`.
fn project_with_rust_config(rust: &str, doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.rust]\n{rust}"),
    );
    project.write("docs/blocks.md", doc);
    project
}

/// A `rustfmt` stub outside the checkout that appends one line per run to `runs`, writes its
/// arguments to `argv` and its environment to `env` in the same directory, and prints its stdin.
struct Stub {
    /// The directory holding the stub and its records, the only `PATH` entry.
    dir: tempfile::TempDir,
}

impl Stub {
    /// Writes the stub.
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let out = dir.path().display();
        let script = format!(
            "#!/bin/sh\necho run >> '{out}/runs'\nprintf '%s\\n' \"$@\" > '{out}/argv'\n\
             /usr/bin/env > '{out}/env'\nexec /bin/cat\n"
        );
        let path = dir.path().join("rustfmt");
        std::fs::write(&path, script).expect("the stub should be writable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        Self { dir }
    }

    /// The text of the record `name`, empty when the stub never wrote it.
    fn record(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    /// Runs `check --json` on `project` with this stub as the only `PATH` entry, asserting it
    /// exits `exit`.
    fn check(&self, project: &Project, exit: i32) -> serde_json::Value {
        project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", self.dir.path())])
            .assert_exit(exit)
            .json()
    }
}

#[test]
fn rustfmt_gets_the_block_with_its_edition_config_and_pinned_toolchain() {
    let project = project_with_rust_doc("# Blocks\n\n```rust\nfn main() {}\n```\n");
    project.write(
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"1.85.0\"\ncomponents = [\"rustfmt\"]\n",
    );
    project.write("rustfmt.toml", "max_width = 80\n");
    let stub = Stub::new();

    let json = stub.check(&project, 0);

    let config = project
        .path("rustfmt.toml")
        .canonicalize()
        .expect("the config should resolve");
    assert_eq!(
        stub.record("argv").lines().collect::<Vec<_>>(),
        [
            "--edition",
            "2024",
            "--emit",
            "stdout",
            "--config-path",
            &config.display().to_string(),
        ],
        "{json}"
    );
    let env = stub.record("env");
    assert!(
        env.lines().any(|line| line == "RUSTUP_TOOLCHAIN=1.85.0"),
        "the channel should be pinned: {env}"
    );
    assert!(
        env.lines().any(|line| line == "RUSTUP_AUTO_INSTALL=0"),
        "auto-install should be off: {env}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn rustfmt_without_a_toolchain_file_still_never_auto_installs() {
    let project = project_with_rust_doc("```rust\nfn main() {}\n```\n");
    let stub = Stub::new();

    let json = stub.check(&project, 0);

    let env = stub.record("env");
    assert!(
        env.lines().any(|line| line == "RUSTUP_AUTO_INSTALL=0"),
        "{env}"
    );
    assert!(
        !env.contains("RUSTUP_TOOLCHAIN="),
        "nothing should be pinned: {env}"
    );
    assert_eq!(stub.record("argv").lines().count(), 4, "{json}");
}

#[test]
fn a_toolchain_path_leaves_every_rust_block_unchecked_with_one_warning() {
    let project = project_with_rust_doc(
        "# Blocks\n\n```rust\nfn main() {}\n```\n\n```rust\nfn f() {}\n```\n",
    );
    project.write(
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"stable\"\npath = \"/opt/evil\"\n",
    );
    let stub = Stub::new();

    let json = stub.check(&project, 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        2,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/code_blocks/1/reason"),
        "toolchain path",
        "{json}"
    );
    assert_eq!(stub.record("runs"), "", "rustfmt should never run");
}

#[test]
fn a_toolchain_file_linked_outside_the_checkout_is_refused() {
    let project = project_with_rust_doc("```rust\nfn main() {}\n```\n");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let target: PathBuf = outside.path().join("rust-toolchain");
    std::fs::write(&target, "stable\n").expect("the outside file should be writable");
    std::os::unix::fs::symlink(&target, project.path("rust-toolchain"))
        .expect("the link should be creatable");
    let stub = Stub::new();

    let json = stub.check(&project, 0);

    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "toolchain path",
        "{json}"
    );
    assert_eq!(stub.record("runs"), "", "rustfmt should never run");
}

#[test]
fn a_compile_time_read_is_not_checked_and_never_reaches_rustfmt() {
    let project = project_with_rust_doc(
        "# Blocks\n\n```rust\nconst S: &str = include_str!(\"/etc/passwd\");\n```\n\n\
         ```rust\nfn main() {}\n```\n",
    );
    let stub = Stub::new();

    let json = stub.check(&project, 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "compile-time read",
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(
        stub.record("runs"),
        "run\n",
        "only the second block should run"
    );
}

/// The year rustfmt's stub was given after `--edition`.
fn edition_run(stub: &Stub) -> String {
    let argv = stub.record("argv");
    let mut lines = argv.lines();
    assert_eq!(lines.next(), Some("--edition"), "{argv}");
    lines
        .next()
        .expect("`--edition` should be followed by a year")
        .to_owned()
}

#[test]
fn edition_defaults_to_2024_and_follows_the_root_cargo_toml() {
    let project = project_with_rust_doc("```rust\nfn main() {}\n```\n");
    let stub = Stub::new();
    let _json = stub.check(&project, 0);
    assert_eq!(edition_run(&stub), "2024");

    project.write(
        "Cargo.toml",
        "[package]\nname = \"x\"\nedition = \"2018\"\n",
    );
    let _json = stub.check(&project, 0);
    assert_eq!(edition_run(&stub), "2018");

    project.write(
        "Cargo.toml",
        "[package]\nedition = \"2018\"\n[workspace.package]\nedition = \"2021\"\n",
    );
    let _json = stub.check(&project, 0);
    assert_eq!(edition_run(&stub), "2021");
}

#[test]
fn edition_config_beats_cargo_toml_and_the_block_attribute_beats_both() {
    let project = project_with_rust_doc("```rust\nfn main() {}\n```\n");
    project.write("Cargo.toml", "[package]\nedition = \"2018\"\n");
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\nedition = \"2015\"\n",
    );
    let stub = Stub::new();
    let _json = stub.check(&project, 0);
    assert_eq!(edition_run(&stub), "2015");

    project.write("docs/blocks.md", "```rust edition2021\nfn main() {}\n```\n");
    let _json = stub.check(&project, 0);
    assert_eq!(edition_run(&stub), "2021");
}

#[test]
fn edition_config_outside_the_four_known_is_a_config_error() {
    let project = project_with_rust_doc("```rust\nfn main() {}\n```\n");
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\nedition = \"2030\"\n",
    );
    let stub = Stub::new();

    let run = project.supdoc_in_with(".", &["check", "--json"], &[("PATH", stub.dir.path())]);
    let _checked = run.assert_exit(78);
    let failed = &run.stderr;

    assert!(
        failed.contains("2030` isn't a Rust edition supdoc knows"),
        "the error should name the unknown edition, got:\n{failed}"
    );
    assert_eq!(stub.record("runs"), "", "rustfmt should never run");
}

#[test]
fn a_bare_number_edition_is_a_config_error_that_says_to_quote_it() {
    let project = project_with_rust_doc("```rust\nfn main() {}\n```\n");
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\nedition = 2021\n",
    );
    let stub = Stub::new();

    let run = project.supdoc_in_with(".", &["check", "--json"], &[("PATH", stub.dir.path())]);
    let _checked = run.assert_exit(78);
    let failed = &run.stderr;

    assert!(
        failed.contains("must be quoted"),
        "the error should say to quote the edition, got:\n{failed}"
    );
    assert_eq!(stub.record("runs"), "", "rustfmt should never run");
}

/// The variables that run the real `rustfmt`, as [`real_rust_tool`] gives them.
fn real_rustfmt(test: &str) -> Option<Vec<(&'static str, PathBuf)>> {
    real_rust_tool("rustfmt", test)
}

/// The variables that run the real `binary`, a Rust tool: `PATH` holding only its directory, and
/// the real user's rustup and cargo homes, through which the rustup proxy finds its toolchains.
/// `None` when `binary` is not on `PATH`, as [`real_tool_dir`] decides it.
fn real_rust_tool(binary: &str, test: &str) -> Option<Vec<(&'static str, PathBuf)>> {
    let dir = real_tool_dir(binary, test)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".rustup")));
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".cargo")));
    let mut vars = vec![("PATH", dir)];
    vars.extend(rustup_home.map(|path| ("RUSTUP_HOME", path)));
    vars.extend(cargo_home.map(|path| ("CARGO_HOME", path)));
    Some(vars)
}

/// `vars` as the borrowed pairs a run takes.
fn borrowed<'vars>(vars: &'vars [(&'static str, PathBuf)]) -> Vec<(&'static str, &'vars Path)> {
    vars.iter()
        .map(|(name, path)| (*name, path.as_path()))
        .collect()
}

#[test]
fn rust_format_with_the_real_rustfmt_passes_formatted_blocks_and_fails_others() {
    let Some(vars) = real_rustfmt("rust_format_with_the_real_rustfmt") else {
        return;
    };
    let project = project_with_rust_doc(
        "# Blocks\n\n```rust\nfn main() {}\n```\n\n```rust\nfn main(){let x=1;}\n```\n",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &borrowed(&vars))
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/failed"), 1, "{json}");
}

#[test]
fn a_block_over_the_scan_cap_is_not_checked_with_one_warning() {
    let body = format!("fn main() {{}}\n{}", "// padding line\n".repeat(20_000));
    let project = project_with_rust_doc(&format!("# Blocks\n\n```rust\n{body}```\n"));
    let stub = Stub::new();

    let json = stub.check(&project, 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        1,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "too large to scan",
        "{json}"
    );
    assert_eq!(stub.record("runs"), "", "rustfmt should never run");
}

#[test]
fn a_hash_line_reaches_the_real_rustfmt_as_written_and_fails_to_parse() {
    let Some(vars) = real_rustfmt("a_hash_line_reaches_the_real_rustfmt_as_written") else {
        return;
    };
    let project = project_with_rust_doc("# Blocks\n\n```rust\n# use x;\nfn main() {}\n```\n");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &borrowed(&vars))
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/failed"), 1, "{json}");
}

#[test]
fn fix_never_deletes_a_plain_comment_line_inside_a_doc_comment_block() {
    let source =
        "/// ```rust\n/// fn  f() {}\n// keep me\n/// fn  g() {}\n/// ```\npub fn f() {}\n";
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\n[features.code-blocks.rust.clippy]\nlint = \"off\"\n",
    );
    project.write("src/blocks.rs", source);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let stub = outside.path().join("rustfmt");
    std::fs::write(
        &stub,
        "#!/bin/sh\n/bin/cat > /dev/null\nprintf 'fn f() {}\\nfn g() {}\\n'\n",
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    let path = [("PATH", outside.path())];

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &path)
        .assert_exit(0);
    let json = project
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(1)
        .json();

    assert_eq!(
        project.read("src/blocks.rs"),
        source,
        "the fix should leave the file byte-identical"
    );
    assert_eq!(
        finding_kinds(&json),
        ["code-block-unformatted", "code-block-fix-unsafe"],
        "{json}"
    );
}

/// A directory outside the checkout holding stub `rustfmt`, `clippy-driver` and `cargo`, each
/// appending one line per run to its own record and `rustfmt` printing its stdin.
fn rust_stubs() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    for (name, tail) in [
        ("rustfmt", "exec /bin/cat\n"),
        ("clippy-driver", ""),
        ("cargo", ""),
    ] {
        let script = format!(
            "#!/bin/sh\necho run >> '{}/{name}-runs'\n{tail}",
            dir.path().display()
        );
        let stub = dir.path().join(name);
        std::fs::write(&stub, script).expect("the stub should be writable");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
    }
    dir
}

/// The text of the record `name` in `dir`, empty when it was never written.
fn stub_record(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// Two Rust blocks clippy would flag, the first at line 3.
const CLIPPY_BAIT: &str = "# Blocks\n\n```rust\npub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n\n\
                           ```rust\npub fn first(v: &Vec<i32>) -> i32 {\n    v[0]\n}\n```\n";

/// The reason a block gives when clippy was held because the checkout isn't trusted.
const NEEDS_TRUST: &str = "clippy runs only in the project tier, which needs `supdoc trust`";

/// The reason a block gives when clippy was held because `SUPDOC_TRUST=0` turned trust off.
const TRUST_OFF: &str =
    "clippy runs only in the project tier, and `SUPDOC_TRUST=0` turns trust off for this run";

/// The reason a block gives when clippy was held in a trusted checkout.
const AWAITS_AUDIT: &str = "clippy doesn't run yet, even in a trusted project";

/// Runs `supdoc args` on `project` with `stubs` as the only `PATH` entry and `vars` besides,
/// asserting it exits `exit` and that neither clippy nor cargo ever started.
fn check_with_stubs(
    project: &Project,
    stubs: &tempfile::TempDir,
    args: &[&str],
    vars: &[(&str, &Path)],
    exit: i32,
) -> crate::support::Run {
    let mut all = vec![("PATH", stubs.path())];
    all.extend_from_slice(vars);
    let run = project.supdoc_in_with(".", args, &all);
    let _checked = run.assert_exit(exit);
    assert_eq!(
        stub_record(stubs.path(), "clippy-driver-runs"),
        "",
        "no clippy-driver process may start: {}",
        run.stdout
    );
    assert_eq!(
        stub_record(stubs.path(), "cargo-runs"),
        "",
        "no cargo process may start: {}",
        run.stdout
    );
    run
}

/// A project whose config holds `config` after its Rust table, with [`CLIPPY_BAIT`] as its doc.
fn clippy_project(config: &str) -> Project {
    project_with_rust_config(config, CLIPPY_BAIT)
}

#[test]
fn clippy_in_an_untrusted_checkout_is_held_with_one_warning_at_the_first_block() {
    let project = clippy_project("");
    let stubs = rust_stubs();

    let run = check_with_stubs(&project, &stubs, &["check", "--json", "-v"], &[], 0);
    let json = run.json();

    assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
    assert_eq!(at(&json, "/findings/0/severity"), "warning", "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 3, "{json}");
    let help = at(&json, "/findings/0/help")
        .as_str()
        .expect("the warning should carry help");
    for named in [
        "supdoc trust",
        "SUPDOC_TRUST=1",
        "tier = \"static\"",
        "lint = \"off\"",
    ] {
        assert!(help.contains(named), "the help should name {named}: {help}");
    }
    for block in ["/data/code_blocks/0", "/data/code_blocks/1"] {
        assert_eq!(at(&json, &format!("{block}/state")), "passed", "{json}");
        assert_eq!(at(&json, &format!("{block}/lint")), "not_run", "{json}");
        assert_eq!(at(&json, &format!("{block}/reason")), NEEDS_TRUST, "{json}");
    }

    let text = check_with_stubs(&project, &stubs, &["check"], &[], 0);
    assert!(
        text.stdout.contains("2 lint not run (clippy needs trust)"),
        "the summary should count both held steps under one reason: {}",
        text.stdout
    );
}

#[test]
fn an_untrusted_checkout_fails_under_deny_warnings() {
    let project = clippy_project("");
    let stubs = rust_stubs();

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "--deny-warnings"],
        &[],
        1,
    )
    .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
}

#[test]
fn supdoc_trust_1_holds_clippy_for_its_audit_without_a_warning() {
    let project = clippy_project("");
    let stubs = rust_stubs();

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[("SUPDOC_TRUST", Path::new("1"))],
        0,
    )
    .json();

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        AWAITS_AUDIT,
        "{json}"
    );
}

#[test]
fn a_root_in_the_trust_store_is_trusted_like_supdoc_trust_1() {
    let project = clippy_project("");
    let stubs = rust_stubs();
    let state = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let root = project
        .path(".")
        .canonicalize()
        .expect("the project root should resolve");
    std::fs::create_dir(state.path().join("supdoc")).expect("the store dir should be creatable");
    std::fs::write(
        state.path().join("supdoc/trusted-roots"),
        format!("{}\n", root.display()),
    )
    .expect("the store should be writable");

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[("XDG_STATE_HOME", state.path())],
        0,
    )
    .json();

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        AWAITS_AUDIT,
        "{json}"
    );
}

#[test]
fn supdoc_trust_0_turns_off_a_listed_roots_trust_and_says_so_in_the_reason_and_warning() {
    let project = clippy_project("");
    let stubs = rust_stubs();
    let state = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let root = project
        .path(".")
        .canonicalize()
        .expect("the project root should resolve");
    std::fs::create_dir(state.path().join("supdoc")).expect("the store dir should be creatable");
    std::fs::write(
        state.path().join("supdoc/trusted-roots"),
        format!("{}\n", root.display()),
    )
    .expect("the store should be writable");

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[
            ("XDG_STATE_HOME", state.path()),
            ("SUPDOC_TRUST", Path::new("0")),
        ],
        0,
    )
    .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
    for block in ["/data/code_blocks/0", "/data/code_blocks/1"] {
        assert_eq!(at(&json, &format!("{block}/lint")), "not_run", "{json}");
        assert_eq!(at(&json, &format!("{block}/reason")), TRUST_OFF, "{json}");
    }
    let message = at(&json, "/findings/0/message")
        .as_str()
        .expect("the warning should carry a message");
    let help = at(&json, "/findings/0/help")
        .as_str()
        .expect("the warning should carry help");
    assert!(
        message.contains("`SUPDOC_TRUST=0` turns trust off for this run"),
        "the warning should name SUPDOC_TRUST=0 as the cause: {message}"
    );
    assert!(
        help.contains("unset `SUPDOC_TRUST`") && !help.contains("run `supdoc trust`"),
        "the help should say to unset SUPDOC_TRUST, not to run `supdoc trust`: {help}"
    );
    let text = check_with_stubs(
        &project,
        &stubs,
        &["check"],
        &[
            ("XDG_STATE_HOME", state.path()),
            ("SUPDOC_TRUST", Path::new("0")),
        ],
        0,
    );
    assert!(
        text.stdout
            .contains("2 lint not run (`SUPDOC_TRUST=0` turned trust off)"),
        "the summary should count both held steps under the off reason: {}",
        text.stdout
    );
}

#[test]
fn tier_static_holds_clippy_with_its_own_reason_and_no_warning() {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\ntier = \"static\"\n[features.code-blocks.rust]\n",
    );
    project.write("docs/blocks.md", CLIPPY_BAIT);
    let stubs = rust_stubs();

    let json = check_with_stubs(&project, &stubs, &["check", "--json", "-v"], &[], 0).json();

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`tier = \"static\"` in `[features.code-blocks]`",
        "{json}"
    );
    let text = check_with_stubs(&project, &stubs, &["check"], &[], 0);
    assert!(
        text.stdout
            .contains("2 lint not run (`tier = \"static\"` in `[features.code-blocks]`)"),
        "{}",
        text.stdout
    );
}

#[test]
fn a_bad_supdoc_trust_is_a_usage_error_before_any_block_is_checked() {
    let project = clippy_project("");
    let stubs = rust_stubs();

    let run = project.supdoc_in_with(
        ".",
        &["check"],
        &[("PATH", stubs.path()), ("SUPDOC_TRUST", Path::new("yes"))],
    );
    let _checked = run.assert_exit(2);

    assert!(
        run.stderr.contains("`SUPDOC_TRUST` is `yes`")
            && run.stderr.contains("`1`")
            && run.stderr.contains("`0` to turn trust off"),
        "the error should name the value and the accepted ones: {}",
        run.stderr
    );
    assert_eq!(stub_record(stubs.path(), "rustfmt-runs"), "");
    assert_eq!(stub_record(stubs.path(), "clippy-driver-runs"), "");
}

#[test]
fn clippy_never_runs_with_the_formatter_off_either() {
    let project = project_with_rust_config(
        "[features.code-blocks.rust.rustfmt]\nformat = \"off\"\n",
        "# Blocks\n\n```rust\npub fn f() -> i32 {\n    missing()\n}\n```\n",
    );
    let stubs = rust_stubs();

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", stubs.path())])
        .assert_exit(0)
        .json();

    assert_eq!(stub_record(stubs.path(), "clippy-driver-runs"), "");
    assert_eq!(stub_record(stubs.path(), "rustfmt-runs"), "");
    assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
}

/// The config that turns the Rust formatter off, so only the linter has a say.
const FORMAT_OFF: &str = "[features.code-blocks.rust.rustfmt]\nformat = \"off\"\n";

/// Asserts block 0's lint step was held for the audit, whatever else its reason says.
fn assert_awaits_audit(json: &serde_json::Value) {
    assert_eq!(at(json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    assert!(
        at(json, "/data/code_blocks/0/reason")
            .as_str()
            .is_some_and(|reason| reason.ends_with(AWAITS_AUDIT)),
        "{json}"
    );
}

/// A trusted check of `doc` with the Rust formatter off and the real `clippy-driver`, asserting
/// it exits 0 with the lint step held for the audit. `None`, skipped with a note naming `test`,
/// when `clippy-driver` is not on `PATH`.
fn held_real_clippy_check(test: &str, doc: &str) -> Option<serde_json::Value> {
    let vars = real_rust_tool("clippy-driver", test)?;
    let mut vars = borrowed(&vars);
    vars.push(("SUPDOC_TRUST", Path::new("1")));
    let project = project_with_rust_config(FORMAT_OFF, doc);
    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &vars)
        .assert_exit(0)
        .json();
    assert_awaits_audit(&json);
    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    Some(json)
}

#[test]
fn clippy_reports_a_vec_reference_parameter_as_a_lint_error_at_its_doc_line() {
    let Some(_json) = held_real_clippy_check(
        "clippy_reports_a_vec_reference_parameter",
        "# Blocks\n\n```rust\npub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n",
    ) else {
        return;
    };
    // After the lift (P5-07K1), swap in, with exit 1 and `SUPDOC_TRUST=1`:
    //
    //     assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    //     assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    //     assert_eq!(at(&json, "/findings/0/location/column"), 17, "{json}");
    //     let message = at(&json, "/findings/0/message");
    //     assert!(
    //         message
    //             .as_str()
    //             .is_some_and(|text| text.contains("clippy::ptr_arg")),
    //         "{json}"
    //     );
    //     assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn clippy_passes_a_block_whose_visible_line_expects_the_lint() {
    let Some(_json) = held_real_clippy_check(
        "clippy_passes_a_block_whose_visible_line_expects_the_lint",
        "# Blocks\n\n```rust\n#[expect(clippy::ptr_arg, reason = \"a non-example\")]\n\
         pub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n",
    ) else {
        return;
    };
    // After the lift (P5-07K1), swap in, with exit 0 and `SUPDOC_TRUST=1`. The `#[expect]` line
    // is visible code now (round 3 §4), not a `# ` hidden line:
    //
    //     assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    //     assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    //     assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn clippy_reports_an_unresolved_name_as_a_missing_name() {
    let Some(_json) = held_real_clippy_check(
        "clippy_reports_an_unresolved_name",
        "# Blocks\n\n```rust\npub fn f() -> i32 {\n    missing()\n}\n```\n",
    ) else {
        return;
    };
    // After the lift (P5-07K1), swap in, with exit 1 and `SUPDOC_TRUST=1` (round 3 §2):
    //
    //     assert_eq!(finding_kinds(&json), ["code-block-missing-name"], "{json}");
    //     assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn clippy_gets_only_the_block_a_private_metadata_path_and_no_project_flags() {
    let project = project_with_rust_config(FORMAT_OFF, "# Blocks\n\n```rust\npub fn f() {}\n```\n");
    project.write(
        "Cargo.toml",
        "[package]\nname = \"p\"\nedition = \"2021\"\n\n[lints.clippy]\nptr_arg = \"deny\"\n\
         pedantic = { level = \"warn\", priority = -1 }\ncargo = \"warn\"\n",
    );
    let stubs = rust_stubs();

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[
            ("SUPDOC_TRUST", Path::new("1")),
            ("RUSTFLAGS", Path::new("--cfg=leak")),
            ("CLIPPY_ARGS", Path::new("--cfg=leak")),
        ],
        0,
    )
    .json();

    assert_awaits_audit(&json);
    // After the lift (P5-07K1), the stub records its argv, env, cwd and `--emit=metadata=` path
    // (as `d77bfd21f^` did) and these hold. A root with no `Cargo.toml` `[package]` gains no
    // argv; this root has one named `p`, so its argv also carries the project-build `--extern`
    // and `-L` of P5-07H:
    //
    //     assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    //     assert_eq!(
    //         argv,
    //         format!(
    //             "--edition\n2021\n--crate-type\nlib\n--emit=metadata={emit}\n--error-format=json\n\
    //              -W\nclippy::pedantic\n-D\nclippy::ptr_arg\n-A\nunused\n-A\ndead_code\n-A\nmissing_docs\n\
    //              -A\nclippy::missing_docs_in_private_items\n-\n"
    //         ),
    //         "groups first, `clippy::cargo` left out, relaxations last"
    //     );
    //     assert!(cwd.ends_with("supdoc/code-blocks/cwd"), "{cwd:?}");
    //     assert!(!Path::new(emit).starts_with(&cwd), "metadata must not land in the cwd: {emit}");
    //     assert!(!Path::new(emit).exists(), "the metadata should be deleted after the run: {emit}");
    //     assert!(
    //         !Path::new(emit).parent().is_some_and(Path::exists),
    //         "the run's metadata directory should be deleted too: {emit}"
    //     );
    //     for leaked in ["RUSTFLAGS", "CLIPPY_ARGS", "CLIPPY_CONF_DIR"] {
    //         assert!(
    //             !env.lines().any(|line| line.starts_with(&format!("{leaked}="))),
    //             "{leaked} must not reach clippy:\n{env}"
    //         );
    //     }
    //     assert!(env.lines().any(|line| line == "RUSTUP_AUTO_INSTALL=0"), "{env}");
}

#[test]
fn clippy_is_not_run_without_a_private_working_directory() {
    let project = project_with_rust_config(FORMAT_OFF, "# Blocks\n\n```rust\npub fn f() {}\n```\n");
    let stubs = rust_stubs();
    let home = project.path("home");

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[("SUPDOC_TRUST", Path::new("1")), ("HOME", &home)],
        0,
    )
    .json();

    assert_awaits_audit(&json);
    // After the lift (P5-07K1), swap in (`HOME` unusable, so no private working directory):
    //
    //     assert_eq!(at(&json, "/data/code_blocks/0/state"), "not_checked", "{json}");
    //     assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    //     let reason = at(&json, "/data/code_blocks/0/reason");
    //     assert!(
    //         reason.as_str().is_some_and(|reason| reason.contains("private working directory")),
    //         "{json}"
    //     );
}

#[test]
fn clippy_never_compiles_a_block_with_an_attribute_off_the_allowlist() {
    let project = project_with_rust_config(
        FORMAT_OFF,
        "# Blocks\n\n```rust\n#![debugger_visualizer(natvis_file = \"/etc/passwd\")]\n```\n",
    );
    let stubs = rust_stubs();

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[("SUPDOC_TRUST", Path::new("1"))],
        0,
    )
    .json();

    assert_awaits_audit(&json);
    // After the lift (P5-07K1), swap in (the strict scan refuses the block before clippy can see
    // it, trusted or not, design §3; the stub's run record stays empty):
    //
    //     assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    //     assert_eq!(
    //         at(&json, "/data/code_blocks/0/reason"),
    //         "compile-time read: `debugger_visualizer` attribute",
    //         "{json}"
    //     );
}
