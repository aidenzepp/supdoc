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

/// Writes a stub `rustup` into `dir` that records each run's working directory and arguments to
/// `rustup-runs`, its environment to `rustup-env`, and answers `printed`, as `rustup show
/// active-toolchain` does.
fn stub_rustup(dir: &Path, printed: &str) {
    let out = dir.display();
    let script = format!(
        "#!/bin/sh\necho \"$(pwd) $*\" >> '{out}/rustup-runs'\n/usr/bin/env > '{out}/rustup-env'\n\
         /bin/cat <<'ANSWER'\n{printed}ANSWER\n"
    );
    let path = dir.join("rustup");
    std::fs::write(&path, script).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

#[test]
fn a_root_without_a_toolchain_file_pins_the_channel_rustup_names_once_per_run() {
    let project = project_with_rust_doc(
        "# Blocks\n\n```rust\nfn main() {}\n```\n\n```rust\nfn f() {}\n```\n",
    );
    let stub = Stub::new();
    stub_rustup(
        stub.dir.path(),
        "1.85.0-x86_64-unknown-linux-gnu (overridden by '/w/rust-toolchain.toml')\n",
    );

    let json = stub.check(&project, 0);

    let root = project
        .path(".")
        .canonicalize()
        .expect("the project root should resolve");
    assert_eq!(
        stub.record("rustup-runs"),
        format!("{} show active-toolchain\n", root.display()),
        "rustup should run once, from the root"
    );
    assert!(
        stub.record("rustup-env")
            .lines()
            .any(|line| line == "RUSTUP_AUTO_INSTALL=0"),
        "rustup should never install: {}",
        stub.record("rustup-env")
    );
    let env = stub.record("env");
    assert!(
        env.lines()
            .any(|line| line == "RUSTUP_TOOLCHAIN=1.85.0-x86_64-unknown-linux-gnu"),
        "rustfmt should get rustup's channel: {env}"
    );
    assert!(
        env.lines().any(|line| line == "RUSTUP_AUTO_INSTALL=0"),
        "{env}"
    );
    assert_eq!(stub.record("runs"), "run\nrun\n", "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 2, "{json}");
}

#[test]
fn rustup_naming_a_path_toolchain_leaves_every_rust_block_unchecked_with_one_warning() {
    let project = project_with_rust_doc(
        "# Blocks\n\n```rust\nfn main() {}\n```\n\n```rust\nfn f() {}\n```\n",
    );
    let stub = Stub::new();
    stub_rustup(
        stub.dir.path(),
        "/w/evil (overridden by '/w/rust-toolchain.toml')\n",
    );

    let json = stub.check(&project, 0);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 3, "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        2,
        "{json}"
    );
    for block in ["/data/code_blocks/0", "/data/code_blocks/1"] {
        assert_eq!(
            at(&json, &format!("{block}/reason")),
            "toolchain unresolved",
            "{json}"
        );
    }
    assert_eq!(stub.record("runs"), "", "rustfmt should never run");
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
/// appending one line per run to its own record. `rustfmt` prints its stdin. `clippy-driver` and
/// `cargo` also append their `RUSTUP_TOOLCHAIN` to `<name>-toolchain`; clippy-driver passes, and
/// cargo builds the root's `proj` lib as the project build reads it: an rmeta in
/// `CARGO_TARGET_DIR` and its artifact line naming the root's `Cargo.toml`.
fn rust_stubs() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let records = dir.path().display();
    let toolchain =
        |name: &str| format!("echo \"$RUSTUP_TOOLCHAIN\" >> '{records}/{name}-toolchain'\n");
    let clippy = format!("{}/bin/cat > /dev/null\n", toolchain("clippy-driver"));
    let cargo = format!(
        "{}deps=\"$CARGO_TARGET_DIR/debug/deps\"\n/bin/mkdir -p \"$deps\"\n\
         echo meta > \"$deps/libproj.rmeta\"\n\
         printf '{{\"reason\":\"compiler-artifact\",\"manifest_path\":\"%s/Cargo.toml\",\
         \"target\":{{\"name\":\"proj\",\"kind\":[\"lib\"]}},\"filenames\":[\"%s\"]}}\\n' \
         \"$(pwd -P)\" \"$deps/libproj.rmeta\"\n",
        toolchain("cargo")
    );
    for (name, tail) in [
        ("rustfmt", "exec /bin/cat\n"),
        ("clippy-driver", clippy.as_str()),
        ("cargo", cargo.as_str()),
    ] {
        let script = format!("#!/bin/sh\necho run >> '{records}/{name}-runs'\n{tail}");
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

/// A root package with a lib (the `basic` fixture's `src/lib.rs`), built on its own, so a check
/// that runs clippy would build it with cargo.
const PROJ: &str = "[package]\nname = \"proj\"\n\n[workspace]\n";

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

/// A project whose config holds `config` after its Rust table, with [`CLIPPY_BAIT`] as its doc
/// and [`PROJ`] as its `Cargo.toml`, so whether cargo started is a real witness.
fn clippy_project(config: &str) -> Project {
    let project = project_with_rust_config(config, CLIPPY_BAIT);
    project.write("Cargo.toml", PROJ);
    project
}

/// Runs a trusted `check --json -v` on `project` with `stubs` as the only `PATH` entry and `vars`
/// besides, asserting it exits 0 with no finding, one project build, clippy run once per block
/// and every block passed.
fn trusted_check_with_stubs(project: &Project, stubs: &tempfile::TempDir, vars: &[(&str, &Path)]) {
    let mut all = vec![("PATH", stubs.path())];
    all.extend_from_slice(vars);
    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &all)
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(
        stub_record(stubs.path(), "clippy-driver-runs"),
        "run\nrun\n",
        "clippy-driver should run once per block: {json}"
    );
    assert_eq!(
        stub_record(stubs.path(), "cargo-runs"),
        "run\n",
        "cargo should build the project once: {json}"
    );
    for block in ["/data/code_blocks/0", "/data/code_blocks/1"] {
        assert_eq!(at(&json, &format!("{block}/state")), "passed", "{json}");
        assert_eq!(at(&json, &format!("{block}/lint")), "ran", "{json}");
    }
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
fn supdoc_trust_1_runs_clippy_without_a_warning() {
    let project = clippy_project("");
    let stubs = rust_stubs();

    trusted_check_with_stubs(&project, &stubs, &[("SUPDOC_TRUST", Path::new("1"))]);
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

    trusted_check_with_stubs(&project, &stubs, &[("XDG_STATE_HOME", state.path())]);
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
    project.write("Cargo.toml", PROJ);
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
    assert_eq!(stub_record(stubs.path(), "cargo-runs"), "");
}

#[test]
fn clippy_never_runs_with_the_formatter_off_either() {
    let project = project_with_rust_config(
        "[features.code-blocks.rust.rustfmt]\nformat = \"off\"\n",
        "# Blocks\n\n```rust\npub fn f() -> i32 {\n    missing()\n}\n```\n",
    );
    project.write("Cargo.toml", PROJ);
    let stubs = rust_stubs();

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", stubs.path())])
        .assert_exit(0)
        .json();

    assert_eq!(stub_record(stubs.path(), "clippy-driver-runs"), "");
    assert_eq!(stub_record(stubs.path(), "cargo-runs"), "");
    assert_eq!(stub_record(stubs.path(), "rustfmt-runs"), "");
    assert_eq!(finding_kinds(&json), ["code-block-not-trusted"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
}

/// The config that turns the Rust formatter off, so only the linter has a say.
const FORMAT_OFF: &str = "[features.code-blocks.rust.rustfmt]\nformat = \"off\"\n";

/// A trusted check of `doc` with the Rust formatter off and the real `clippy-driver`, asserting
/// it exits `exit` with clippy's lint step run on block 0. `None`, skipped with a note naming
/// `test`, when `clippy-driver` is not on `PATH`.
fn real_clippy_check(test: &str, doc: &str, exit: i32) -> Option<serde_json::Value> {
    let vars = real_rust_tool("clippy-driver", test)?;
    let mut vars = borrowed(&vars);
    vars.push(("SUPDOC_TRUST", Path::new("1")));
    let project = project_with_rust_config(FORMAT_OFF, doc);
    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &vars)
        .assert_exit(exit)
        .json();
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
    Some(json)
}

#[test]
fn clippy_reports_a_vec_reference_parameter_as_a_lint_error_at_its_doc_line() {
    let Some(json) = real_clippy_check(
        "clippy_reports_a_vec_reference_parameter",
        "# Blocks\n\n```rust\npub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n",
        1,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    assert_eq!(at(&json, "/findings/0/location/column"), 17, "{json}");
    let message = at(&json, "/findings/0/message");
    assert!(
        message
            .as_str()
            .is_some_and(|text| text.contains("clippy::ptr_arg")),
        "{json}"
    );
}

#[test]
fn clippy_passes_a_block_whose_visible_line_expects_the_lint() {
    // The `#[expect]` line is visible code (round 3 §4), not a `# ` hidden line.
    let Some(json) = real_clippy_check(
        "clippy_passes_a_block_whose_visible_line_expects_the_lint",
        "# Blocks\n\n```rust\n#[expect(clippy::ptr_arg, reason = \"a non-example\")]\n\
         pub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n",
        0,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn clippy_reports_an_unresolved_name_as_a_missing_name() {
    // Round 3 §2.
    let Some(json) = real_clippy_check(
        "clippy_reports_an_unresolved_name",
        "# Blocks\n\n```rust\npub fn f() -> i32 {\n    missing()\n}\n```\n",
        1,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), ["code-block-missing-name"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn clippy_gets_only_the_block_a_private_metadata_path_and_no_project_flags() {
    let project = project_with_rust_config(FORMAT_OFF, "# Blocks\n\n```rust\npub fn f() {}\n```\n");
    project.write(
        "Cargo.toml",
        "[package]\nname = \"proj\"\nedition = \"2021\"\n\n[workspace]\n\n[lints.clippy]\n\
         ptr_arg = \"deny\"\npedantic = { level = \"warn\", priority = -1 }\ncargo = \"warn\"\n",
    );
    let stubs = rust_stubs();
    let out = stubs.path().display();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{out}/argv'\n/usr/bin/env > '{out}/env'\n\
         for arg in \"$@\"; do case \"$arg\" in --emit=metadata=*) \
         emit=\"${{arg#--emit=metadata=}}\"; echo meta > \"$emit\"; echo \"$emit\" > '{out}/emit';; esac; done\n\
         pwd -P > '{out}/cwd'\n/bin/cat > /dev/null\n"
    );
    std::fs::write(stubs.path().join("clippy-driver"), script)
        .expect("the stub should be writable");
    let record = |name: &str| stub_record(stubs.path(), name);

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json", "-v"],
            &[
                ("PATH", stubs.path()),
                ("SUPDOC_TRUST", Path::new("1")),
                ("RUSTFLAGS", Path::new("--cfg=leak")),
                ("CLIPPY_ARGS", Path::new("--cfg=leak")),
            ],
        )
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(record("cargo-runs"), "run\n", "one project build");
    let argv = record("argv");
    let emit = record("emit");
    let emit = emit.trim();
    let rmeta = argv
        .lines()
        .find_map(|line| line.strip_prefix("proj="))
        .expect("clippy should get the project lib as an extern");
    assert!(rmeta.ends_with("/debug/deps/libproj.rmeta"), "{rmeta}");
    let deps = Path::new(rmeta)
        .parent()
        .expect("the rmeta should sit in a directory")
        .display();
    assert_eq!(
        argv,
        format!(
            "--edition\n2021\n--crate-type\nlib\n--emit=metadata={emit}\n--error-format=json\n\
             --extern\nproj={rmeta}\n-L\ndependency={deps}\n\
             -W\nclippy::pedantic\n-D\nclippy::ptr_arg\n-A\nunused\n-A\ndead_code\n-A\nmissing_docs\n\
             -A\nclippy::missing_docs_in_private_items\n-\n"
        ),
        "the project build's extern, then groups first, `clippy::cargo` left out, relaxations last"
    );
    let cwd = PathBuf::from(record("cwd").trim());
    assert!(cwd.ends_with("supdoc/code-blocks/cwd"), "{cwd:?}");
    assert!(
        !Path::new(emit).starts_with(&cwd),
        "metadata must not land in the cwd: {emit}"
    );
    assert!(
        !Path::new(emit).exists(),
        "the metadata should be deleted after the run: {emit}"
    );
    assert!(
        !Path::new(emit).parent().is_some_and(Path::exists),
        "the run's metadata directory should be deleted too: {emit}"
    );
    let env = record("env");
    for leaked in ["RUSTFLAGS", "CLIPPY_ARGS", "CLIPPY_CONF_DIR"] {
        assert!(
            !env.lines()
                .any(|line| line.starts_with(&format!("{leaked}="))),
            "{leaked} must not reach clippy:\n{env}"
        );
    }
    assert!(
        env.lines().any(|line| line == "RUSTUP_AUTO_INSTALL=0"),
        "{env}"
    );
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

    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "not_run", "{json}");
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason
            .as_str()
            .is_some_and(|reason| reason.contains("private working directory")),
        "{json}"
    );
}

#[test]
fn clippy_never_compiles_a_block_with_an_attribute_off_the_allowlist() {
    let project = project_with_rust_config(
        FORMAT_OFF,
        "# Blocks\n\n```rust\n#![debugger_visualizer(natvis_file = \"/etc/passwd\")]\n```\n",
    );
    project.write("Cargo.toml", PROJ);
    let stubs = rust_stubs();

    // The strict scan refuses the block before clippy or the project build can see it, trusted
    // or not (design §3).
    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[("SUPDOC_TRUST", Path::new("1"))],
        0,
    )
    .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "compile-time read: `debugger_visualizer` attribute",
        "{json}"
    );
}

#[test]
fn a_trusted_block_refused_by_the_scan_names_why() {
    let cases = [
        (
            "fn f() { my_macro!(); }",
            "macro `my_macro` isn't on supdoc's list",
        ),
        // A macro off the list reads as unlisted whether or not it's called through a path.
        (
            "fn f() -> i32 { proj::twice!(3) }",
            "macro `twice` isn't on supdoc's list",
        ),
        (
            "fn f() { proj::println!(\"x\"); }",
            "macro `println` called through a path isn't on supdoc's list",
        ),
        (
            "const S: &str = include_str!(\"/etc/passwd\");",
            "compile-time read",
        ),
    ];
    for (body, reason) in cases {
        let project =
            project_with_rust_config(FORMAT_OFF, &format!("# Blocks\n\n```rust\n{body}\n```\n"));
        project.write("Cargo.toml", PROJ);
        let stubs = rust_stubs();

        let json = check_with_stubs(
            &project,
            &stubs,
            &["check", "--json", "-v"],
            &[("SUPDOC_TRUST", Path::new("1"))],
            0,
        )
        .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body}: {json}"
        );
        assert_eq!(
            at(&json, "/data/code_blocks/0/reason"),
            reason,
            "{body}: {json}"
        );
    }
}

/// A doc of two Rust blocks clippy and cargo would see once trusted.
const TWO_BLOCKS: &str = "# Blocks\n\n```rust\npub fn a() {}\n```\n\n```rust\npub fn b() {}\n```\n";

#[test]
fn a_parent_channel_rustup_names_reaches_cargo_and_clippy_alike() {
    let project = project_with_rust_config(FORMAT_OFF, TWO_BLOCKS);
    project.write("Cargo.toml", PROJ);
    let stubs = rust_stubs();
    stub_rustup(
        stubs.path(),
        "1.85.0-x86_64-unknown-linux-gnu (overridden by '/w/rust-toolchain.toml')\n",
    );

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json", "-v"],
            &[("PATH", stubs.path()), ("SUPDOC_TRUST", Path::new("1"))],
        )
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 2, "{json}");
    let pinned = "1.85.0-x86_64-unknown-linux-gnu\n";
    assert_eq!(
        stub_record(stubs.path(), "cargo-toolchain"),
        pinned,
        "{json}"
    );
    assert_eq!(
        stub_record(stubs.path(), "clippy-driver-toolchain"),
        pinned.repeat(2),
        "cargo and clippy-driver get the same toolchain: {json}"
    );
}

#[test]
fn a_parent_path_toolchain_starts_neither_cargo_nor_clippy() {
    let project = project_with_rust_config(FORMAT_OFF, TWO_BLOCKS);
    project.write("Cargo.toml", PROJ);
    let stubs = rust_stubs();
    let evil = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    std::fs::create_dir(evil.path().join("bin")).expect("the path toolchain should be creatable");
    for name in ["cargo", "clippy-driver"] {
        let stub = evil.path().join("bin").join(name);
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho run >> '{}/evil-runs'\n",
                evil.path().display()
            ),
        )
        .expect("the stub should be writable");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
    }
    stub_rustup(
        stubs.path(),
        &format!(
            "{} (overridden by '/w/rust-toolchain.toml')\n",
            evil.path().display()
        ),
    );

    let json = check_with_stubs(
        &project,
        &stubs,
        &["check", "--json", "-v"],
        &[("SUPDOC_TRUST", Path::new("1"))],
        0,
    )
    .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    let message = at(&json, "/findings/0/message")
        .as_str()
        .expect("the warning should carry a message")
        .to_owned();
    assert!(
        message.contains("named a path or custom toolchain")
            && message.contains(&evil.path().display().to_string()),
        "the warning should name the cause and quote rustup: {message}"
    );
    for block in ["/data/code_blocks/0", "/data/code_blocks/1"] {
        assert_eq!(
            at(&json, &format!("{block}/state")),
            "not_checked",
            "{json}"
        );
        assert_eq!(
            at(&json, &format!("{block}/reason")),
            "toolchain unresolved",
            "{json}"
        );
    }
    assert_eq!(
        stub_record(evil.path(), "evil-runs"),
        "",
        "the path's tools never start"
    );
}

/// The probe crate's lib (clippy-5b §"Probe"): a function and an exported macro.
const PROBE_LIB: &str = "pub fn total(v: &[i32]) -> i32 { v.iter().sum() }\n#[macro_export]\n\
                         macro_rules! twice { ($e:expr) => { $e * 2 } }\n";

/// The probe crate's `Cargo.toml`: a package with a lib, built on its own.
const PROBE_MANIFEST: &str = "[package]\nname = \"proj\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                              [dependencies]\n\n[workspace]\n";

/// The probe crate's `Cargo.lock`, which `--locked` needs and never rewrites.
const PROBE_LOCK: &str = "# This file is automatically @generated by Cargo.\n\
                          # It is not intended for manual editing.\nversion = 4\n\n\
                          [[package]]\nname = \"proj\"\nversion = \"0.1.0\"\n";

/// Where a trusted real-toolchain project's root sits inside its temporary directory, so a test
/// can pin a toolchain in the directory above it.
const INNER: &str = "proj";

/// Every file under `dir` with its bytes, by path relative to `dir`, sorted.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).expect("the project directory should be readable") {
            let path = entry.expect("a directory entry should be readable").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).expect("a project file should be readable");
                let relative = path
                    .strip_prefix(dir)
                    .expect("a walked file should be under the root")
                    .to_path_buf();
                files.push((relative, bytes));
            }
        }
    }
    files.sort();
    files
}

/// The variables that run the real `cargo` and `clippy-driver` together, as [`real_rust_tool`]
/// gives them, with `PATH` holding both directories. `None` when either is missing.
fn real_cargo_and_clippy(test: &str) -> Option<Vec<(&'static str, PathBuf)>> {
    let cargo = real_tool_dir("cargo", test)?;
    let mut vars = real_rust_tool("clippy-driver", test)?;
    for (name, value) in &mut vars {
        if *name == "PATH" && *value != cargo {
            *value = std::env::join_paths([value.clone(), cargo.clone()])
                .expect("the tool directories should join into a PATH")
                .into();
        }
    }
    Some(vars)
}

/// A trusted `check --json -v` with the real cargo and clippy-driver of a project at [`INNER`]
/// whose crate is the probe's with `lib` as its `src/lib.rs` and whose doc is `doc`, the
/// formatter off and the cache in the test's scratch. `parent` is written to the directory above
/// the root as `rust-toolchain.toml` when given. Asserts it exits `exit` and leaves the checkout
/// unchanged, with no `target/`. `None`, skipped with a note naming `test`, when either tool is
/// missing.
fn trusted_probe_check(
    test: &str,
    lib: &str,
    doc: &str,
    parent: Option<&str>,
    exit: i32,
) -> Option<serde_json::Value> {
    let vars = real_cargo_and_clippy(test)?;
    let project = Project::fixture("basic");
    let file = |name: &str| format!("{INNER}/{name}");
    project.write(
        &file("supdoc.config.toml"),
        &format!("[features.code-blocks]\n[features.code-blocks.rust]\n{FORMAT_OFF}"),
    );
    project.write(&file("Cargo.toml"), PROBE_MANIFEST);
    project.write(&file("Cargo.lock"), PROBE_LOCK);
    project.write(&file("src/lib.rs"), lib);
    project.write(&file("docs/blocks.md"), doc);
    if let Some(toolchain) = parent {
        project.write("rust-toolchain.toml", toolchain);
    }
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let mut vars = borrowed(&vars);
    vars.push(("SUPDOC_TRUST", Path::new("1")));
    vars.push(("XDG_CACHE_HOME", cache.path()));
    let root = project.path(INNER);
    let before = snapshot(&root);

    let json = project
        .supdoc_in_with(INNER, &["check", "--json", "-v"], &vars)
        .assert_exit(exit)
        .json();

    assert!(
        !root.join("target").exists(),
        "no build may land in the checkout"
    );
    assert!(
        snapshot(&root) == before,
        "the check must leave the checkout unchanged: {json}"
    );
    Some(json)
}

/// The messages of `json`'s findings with the code `kind`, in order.
fn messages_of(json: &serde_json::Value, kind: &str) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter(|finding| finding["kind"] == kind)
        .filter_map(|finding| finding["message"].as_str().map(str::to_owned))
        .collect()
}

/// The probe's `f` block without its `proj::twice!(3)`: the strict scan refuses a macro called
/// through a path, trusted or not (clippy-5b §3, an accepted consequence).
const PROBE_F: &str = "# Probe\n\n```rust\npub fn f() -> i32 {\n    proj::total(&[1, 2])\n}\n```\n";

#[test]
fn trusted_block_naming_the_crate_passes() {
    let Some(json) = trusted_probe_check(
        "trusted_block_naming_the_crate_passes",
        PROBE_LIB,
        PROBE_F,
        None,
        0,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn trusted_block_passing_a_vec_reference_is_a_ptr_arg_lint_at_its_doc_line() {
    let Some(json) = trusted_probe_check(
        "trusted_block_passing_a_vec_reference_is_a_ptr_arg_lint",
        PROBE_LIB,
        "# Probe\n\n```rust\npub fn g(v: &Vec<i32>) -> i32 {\n    proj::total(v)\n}\n```\n",
        None,
        1,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    assert!(
        messages_of(&json, "code-block-lint")
            .iter()
            .all(|message| message.contains("clippy::ptr_arg")),
        "{json}"
    );
}

#[test]
fn trusted_block_naming_two_missing_functions_gives_two_missing_names() {
    let Some(json) = trusted_probe_check(
        "trusted_block_naming_two_missing_functions",
        PROBE_LIB,
        "# Probe\n\n```rust\npub fn h() -> i32 {\n    missing() + proj::nope()\n}\n```\n",
        None,
        1,
    ) else {
        return;
    };

    assert_eq!(
        finding_kinds(&json),
        ["code-block-missing-name", "code-block-missing-name"],
        "{json}"
    );
    let messages = messages_of(&json, "code-block-missing-name");
    for name in ["`nope`", "`missing`"] {
        assert!(
            messages.iter().any(|message| message.contains(name)),
            "a finding should name {name}: {messages:?}"
        );
    }
}

#[test]
fn trusted_project_that_does_not_build_fails_the_tool_and_passes_no_block() {
    let Some(json) = trusted_probe_check(
        "trusted_project_that_does_not_build",
        "pub fn broken( {\n",
        "# Probe\n\n```rust\npub fn f() -> i32 {\n    proj::total(&[1, 2])\n}\n```\n",
        None,
        1,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
    assert_ne!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
}

/// The toolchain a parent pins for [`trusted_crate_below_a_parent_channel_pin_builds_and_lints_with_one_toolchain`]:
/// not the usual default, so cargo and clippy-driver disagreeing would show as E0514.
const PARENT_PIN: &str = "nightly";

/// Whether the real rustup in `vars`' `PATH` has [`PARENT_PIN`] with clippy installed, asked
/// without letting it install anything.
fn parent_pin_installed(vars: &[(&'static str, PathBuf)]) -> bool {
    let mut command = std::process::Command::new("rustup");
    let _command = command
        .args(["run", PARENT_PIN, "clippy-driver", "--version"])
        .env_clear()
        .env("RUSTUP_AUTO_INSTALL", "0")
        .envs(vars.iter().map(|(name, value)| (*name, value.as_os_str())));
    command.output().is_ok_and(|output| output.status.success())
}

#[test]
fn trusted_crate_below_a_parent_channel_pin_builds_and_lints_with_one_toolchain() {
    let test = "trusted_crate_below_a_parent_channel_pin";
    let Some(vars) = real_cargo_and_clippy(test) else {
        return;
    };
    if real_tool_dir("rustup", test).is_none() || !parent_pin_installed(&vars) {
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("SKIPPED {test}: the `{PARENT_PIN}` toolchain with clippy isn't installed\n")
                .as_bytes(),
        );
        return;
    }

    let Some(json) = trusted_probe_check(
        test,
        PROBE_LIB,
        PROBE_F,
        Some(&format!("[toolchain]\nchannel = \"{PARENT_PIN}\"\n")),
        0,
    ) else {
        return;
    };

    assert_eq!(
        finding_kinds(&json),
        Vec::<String>::new(),
        "no E0514: {json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
}

/// Every line and name of clippy's frames that a block doesn't write.
const CLIPPY_FRAME: [&str; 5] = [
    "fn supdoc_fragment() {",
    "-> impl Sized {",
    "struct SupdocFragment;",
    "supdoc_fragment",
    "SupdocFragment",
];

/// Asserts that no finding of `json` shows a line or name of clippy's frames in its message,
/// help, location or fix.
fn assert_no_clippy_frame_text(json: &serde_json::Value) {
    for finding in array_at(json, "/findings") {
        for field in ["message", "help", "location", "fix"] {
            let text = finding[field].to_string();
            for frame in CLIPPY_FRAME {
                assert!(!text.contains(frame), "{field} shows {frame:?}: {finding}");
            }
        }
    }
}

/// Each finding of `json` as its code and doc line, in order.
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

#[test]
fn frame_lint_real_clippy_passes_statements_an_expression_and_members_naming_the_crate() {
    let doc = "# Probe\n\n\
               ```rust fragment(statements)\nlet total = proj::total(&[1, 2]);\n\
               assert_eq!(total, 3);\n```\n\n\
               ```rust fragment(expression)\nproj::total(&[1, 2]) * 2\n```\n\n\
               ```rust fragment(members)\nfn doubled(&self, v: &[i32]) -> i32 {\n    \
               proj::total(v) * 2\n}\n```\n";
    let Some(json) = trusted_probe_check(
        "frame_lint_real_clippy_passes_statements_an_expression_and_members",
        PROBE_LIB,
        doc,
        None,
        0,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    for block in 0..3 {
        assert_eq!(
            at(&json, &format!("/data/code_blocks/{block}/lint")),
            "ran",
            "{json}"
        );
        assert_eq!(
            at(&json, &format!("/data/code_blocks/{block}/state")),
            "passed",
            "{json}"
        );
    }
}

#[test]
fn frame_lint_real_clippy_reports_each_framed_block_at_its_own_line_without_frame_text() {
    let doc = "# Probe\n\n\
               ```rust fragment(statements)\nlet v: Vec<i32> = Vec::new();\n\
               let empty = v.len() == 0;\nassert!(empty);\n```\n\n\
               ```rust fragment(members)\nfn area(&self) -> i32 {\n    self.w\n}\n```\n\n\
               ```rust fragment(statements)\nlet n: i32 = \"1\".parse()?;\nassert_eq!(n, 1);\n```\n\n\
               ```rust fragment(statements)\nlet x = ;\n```\n\n\
               ```rust fragment(statements)\nlet a = 1;\nmissing(a);\n```\n";
    let Some(json) = trusted_probe_check(
        "frame_lint_real_clippy_reports_each_framed_block",
        PROBE_LIB,
        doc,
        None,
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [
            ("code-block-lint".to_owned(), 5),
            ("code-block-lint".to_owned(), 11),
            ("code-block-lint".to_owned(), 16),
            ("code-block-parse".to_owned(), 21),
            ("code-block-missing-name".to_owned(), 26),
        ],
        "{json}"
    );
    let messages = messages_of(&json, "code-block-lint");
    assert!(
        messages
            .first()
            .is_some_and(|message| message.contains("clippy::len_zero")),
        "{messages:?}"
    );
    assert!(
        messages
            .get(1)
            .is_some_and(|message| message.contains("no field `w` on type `&Self`")),
        "a members block's methods know the frame's struct as `Self`: {messages:?}"
    );
    assert!(
        messages
            .get(2)
            .is_some_and(|message| message.contains("`E0277`")),
        "a `?` in statements is a true error, the frame returning `()`: {messages:?}"
    );
    assert_eq!(
        messages_of(&json, "code-block-parse"),
        ["`clippy` couldn't parse the block as `statements`"],
        "{json}"
    );
    assert_eq!(
        messages_of(&json, "code-block-missing-name"),
        ["`missing` isn't defined in this block or the project's crate"],
        "{json}"
    );
    assert_no_clippy_frame_text(&json);
}

#[test]
fn frame_lint_real_clippy_keeps_author_names_that_hold_or_match_another_frames_name() {
    let doc = "# Probe\n\n\
               ```rust fragment(statements)\nlet n = supdoc_fragment_total + 1;\n```\n\n\
               ```rust fragment(statements)\nlet c = SupdocFragmentConfig::new();\n```\n\n\
               ```rust fragment(expression)\nSupdocFragment\n```\n";
    let Some(json) = trusted_probe_check(
        "frame_lint_real_clippy_keeps_author_names",
        PROBE_LIB,
        doc,
        None,
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [
            ("code-block-missing-name".to_owned(), 4),
            ("code-block-missing-name".to_owned(), 8),
            ("code-block-missing-name".to_owned(), 12),
        ],
        "{json}"
    );
    let named: Vec<(String, String)> = array_at(&json, "/findings")
        .iter()
        .map(|finding| {
            (
                finding["message"].as_str().unwrap_or_default().to_owned(),
                finding["help"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    for ((message, help), name) in named.iter().zip([
        "`supdoc_fragment_total`",
        "`SupdocFragmentConfig`",
        "`SupdocFragment`",
    ]) {
        assert!(
            message.starts_with(name) && help.contains(name),
            "only the frame in use's own name is rewritten, and only as a whole name: \
             {message} / {help}"
        );
    }
}

#[test]
fn frame_lint_real_clippy_reports_a_syntax_error_alone_over_the_names_rustc_recovers_into() {
    let doc = "# Probe\n\n```rust fragment(statements)\nlet x = 1\nmissing(x);\n```\n";
    let Some(json) = trusted_probe_check(
        "frame_lint_real_clippy_reports_a_syntax_error_alone",
        PROBE_LIB,
        doc,
        None,
        1,
    ) else {
        return;
    };

    assert_eq!(
        kinds_and_lines(&json),
        [("code-block-parse".to_owned(), 4)],
        "{json}"
    );
    assert_eq!(
        messages_of(&json, "code-block-parse"),
        ["`clippy` couldn't parse the block as `statements`"],
        "{json}"
    );
    assert_no_clippy_frame_text(&json);
}
