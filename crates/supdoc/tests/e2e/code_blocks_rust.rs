//! Checking Rust code blocks, in Markdown and in doc comments.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::support::{Project, array_at, at};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// A recorded `basic` project with the Rust table on and `doc` at `docs/blocks.md`.
fn project_with_rust_doc(doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\n",
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

    let _failed = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", stub.dir.path())])
        .assert_exit(78);

    assert_eq!(stub.record("runs"), "", "rustfmt should never run");
}

/// The absolute `PATH` directory holding `rustfmt`, when one does.
fn rustfmt_dir() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .find(|dir| dir.is_absolute() && dir.join("rustfmt").is_file())
}

/// The variables that run the real `rustfmt`: `PATH` holding only its directory, and the real
/// user's rustup and cargo homes, through which the rustup proxy finds its toolchains. `None`,
/// with a note on stderr naming `test`, when `rustfmt` is not on `PATH`.
fn real_rustfmt(test: &str) -> Option<Vec<(&'static str, PathBuf)>> {
    let Some(dir) = rustfmt_dir() else {
        // Written to the process's stderr directly, which `cargo test` shows for a passing test.
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("SKIPPED {test}: rustfmt is not on PATH\n").as_bytes(),
        );
        return None;
    };
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
fn hidden_lines_in_a_formatted_block_pass_with_the_real_rustfmt() {
    let Some(vars) = real_rustfmt("hidden_lines_in_a_formatted_block_pass") else {
        return;
    };
    let project = project_with_rust_doc(
        "# Blocks\n\n```rust\n# use std::fmt::Write as _;\n# fn main() {\n    # let x = 1;\n    let _y = x;\n# }\n```\n",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &borrowed(&vars))
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn hidden_lines_keep_their_prefix_when_the_real_rustfmt_fixes_the_block() {
    let Some(vars) = real_rustfmt("hidden_lines_keep_their_prefix") else {
        return;
    };
    let project = project_with_rust_doc(
        "# Blocks\n\n```rust\n# fn main() {\n    # let x = 1;\nlet y=x;\n# }\n```\n",
    );
    let vars = borrowed(&vars);

    let _fix = project
        .supdoc_in_with(".", &["fix", "--yes"], &vars)
        .assert_exit(0);
    let json = project
        .supdoc_in_with(".", &["check", "--json"], &vars)
        .assert_exit(0)
        .json();

    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```rust\n# fn main() {\n    # let x = 1;\n    let y = x;\n# }\n```\n",
        "the fix should format the shown line and keep each hidden line's `# `"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}
