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

/// A recorded `basic` project with the Rust table on, its linter off so only rustfmt runs, and
/// `doc` at `docs/blocks.md`.
fn project_with_rust_doc(doc: &str) -> Project {
    project_with_rust_config("linter = false\n", doc)
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

/// The absolute `PATH` directory holding `binary`, when one does.
fn tool_dir(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .find(|dir| dir.is_absolute() && dir.join(binary).is_file())
}

/// The variables that run the real `rustfmt`, as [`real_rust_tool`] gives them.
fn real_rustfmt(test: &str) -> Option<Vec<(&'static str, PathBuf)>> {
    real_rust_tool("rustfmt", test)
}

/// The variables that run the real `binary`, a Rust tool: `PATH` holding only its directory, and
/// the real user's rustup and cargo homes, through which the rustup proxy finds its toolchains.
/// `None`, with a note on stderr naming `test`, when `binary` is not on `PATH`.
fn real_rust_tool(binary: &str, test: &str) -> Option<Vec<(&'static str, PathBuf)>> {
    let Some(dir) = tool_dir(binary) else {
        // Written to the process's stderr directly, which `cargo test` shows for a passing test.
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("SKIPPED {test}: {binary} is not on PATH\n").as_bytes(),
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

#[test]
fn fix_never_deletes_a_plain_comment_line_inside_a_doc_comment_block() {
    let source =
        "/// ```rust\n/// fn  f() {}\n// keep me\n/// fn  g() {}\n/// ```\npub fn f() {}\n";
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\nlinter = false\n",
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

/// A real `clippy-driver` check of `doc` with the Rust formatter off, asserting it exits `exit`.
/// `None`, skipped with a note naming `test`, when `clippy-driver` is not on `PATH`.
fn real_clippy_check(test: &str, doc: &str, exit: i32) -> Option<serde_json::Value> {
    let vars = real_rust_tool("clippy-driver", test)?;
    let project = project_with_rust_config("formatter = false\n", doc);
    Some(
        project
            .supdoc_in_with(".", &["check", "--json"], &borrowed(&vars))
            .assert_exit(exit)
            .json(),
    )
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
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn clippy_passes_a_block_whose_hidden_line_expects_the_lint() {
    let Some(json) = real_clippy_check(
        "clippy_passes_a_block_whose_hidden_line_expects_the_lint",
        "# Blocks\n\n```rust\n# #[expect(clippy::ptr_arg, reason = \"a non-example\")]\n\
         pub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n",
        0,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn clippy_reports_an_unresolved_name_as_a_lint_error() {
    let Some(json) = real_clippy_check(
        "clippy_reports_an_unresolved_name",
        "# Blocks\n\n```rust\npub fn f() -> i32 {\n    missing()\n}\n```\n",
        1,
    ) else {
        return;
    };

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn clippy_gets_only_the_block_a_private_metadata_path_and_no_project_flags() {
    let project = project_with_rust_config(
        "formatter = false\n",
        "# Blocks\n\n```rust\npub fn f() {}\n```\n",
    );
    project.write(
        "Cargo.toml",
        "[package]\nname = \"p\"\nedition = \"2021\"\n\n[lints.clippy]\nptr_arg = \"deny\"\n\
         pedantic = { level = \"warn\", priority = -1 }\ncargo = \"warn\"\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let out = outside.path().display();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{out}/argv'\n/usr/bin/env > '{out}/env'\n\
         for arg in \"$@\"; do case \"$arg\" in --emit=metadata=*) \
         emit=\"${{arg#--emit=metadata=}}\"; echo meta > \"$emit\"; echo \"$emit\" > '{out}/emit';; esac; done\n\
         pwd -P > '{out}/cwd'\n/bin/cat > /dev/null\n"
    );
    let stub = outside.path().join("clippy-driver");
    std::fs::write(&stub, script).expect("the stub should be writable");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    let record =
        |name: &str| std::fs::read_to_string(outside.path().join(name)).unwrap_or_default();

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[
                ("PATH", outside.path()),
                ("RUSTFLAGS", Path::new("--cfg=leak")),
                ("CLIPPY_ARGS", Path::new("--cfg=leak")),
            ],
        )
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    let argv = record("argv");
    let emit = record("emit");
    let emit = emit.trim();
    assert_eq!(
        argv,
        format!(
            "--edition\n2021\n--crate-type\nlib\n--emit=metadata={emit}\n--error-format=json\n\
             -W\nclippy::pedantic\n-D\nclippy::ptr_arg\n-A\nunused\n-A\ndead_code\n-A\nmissing_docs\n\
             -A\nclippy::missing_docs_in_private_items\n-\n"
        ),
        "groups first, `clippy::cargo` left out, relaxations last"
    );
    let cwd = PathBuf::from(record("cwd").trim());
    assert!(cwd.ends_with("supdoc/code-blocks/cwd"), "{cwd:?}");
    assert!(
        !Path::new(emit).starts_with(&cwd),
        "the metadata must not land in the working directory: {emit}"
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
    let project = project_with_rust_config(
        "formatter = false\n",
        "# Blocks\n\n```rust\npub fn f() {}\n```\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let runs = outside.path().join("runs");
    let stub = outside.path().join("clippy-driver");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\necho run >> '{}'\n", runs.display()),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    let home = project.path("home");

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", outside.path()), ("HOME", &home)],
        )
        .assert_exit(0)
        .json();

    assert!(!runs.exists(), "clippy should never run from `/`");
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
