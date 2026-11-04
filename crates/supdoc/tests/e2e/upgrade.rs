//! `supdoc upgrade`: every case runs a copy of the built binary placed where the case needs it,
//! with a fake `cargo` first on `PATH` that records its argument vector and working directory.
//! No test reaches the network.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use tempfile::TempDir;

use super::support::Run;

/// The version of the binary under test, which a copy reports as the one running.
const RUNNING: &str = env!("CARGO_PKG_VERSION");

/// A version far above any release, so installing it is an upgrade.
const NEWER: &str = "99.0.0";

/// A version below any release, so installing it is a downgrade.
const OLDER: &str = "0.0.1";

/// What cargo records for a crates.io install of the running version.
fn crates_io_record(version: &str) -> String {
    format!(
        r#"{{"installs":{{"supdoc {version} (registry+https://github.com/rust-lang/crates.io-index)":{{"version_req":null,"bins":["supdoc"],"features":[],"all_features":false,"no_default_features":false,"profile":"release","target":"x","rustc":"r"}}}}}}"#
    )
}

/// A scratch directory holding a cargo home, a fake `cargo` and a working directory.
struct Fixture {
    /// Keeps the directory alive.
    dir: TempDir,
}

impl Fixture {
    /// A cargo home whose `bin/supdoc` is a copy of the binary and whose record says the running
    /// version was installed from crates.io, and a fake `cargo` that installs successfully and
    /// finds `search_result` as the latest version.
    fn installed(search_result: &str) -> Self {
        let fixture = Self::empty();
        fixture.place_binary("cargo-home/bin/supdoc");
        fixture.write("cargo-home/.crates2.json", &crates_io_record(RUNNING));
        fixture.fake_cargo(search_result, "exit 0");
        fixture
    }

    /// A scratch directory with only a working directory.
    fn empty() -> Self {
        let dir = TempDir::new().expect("a scratch directory should be creatable");
        let fixture = Self { dir };
        std::fs::create_dir_all(fixture.path("work")).expect("the working directory should exist");
        fixture
    }

    /// The absolute path of `relative` in the scratch directory.
    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// Writes `text` to `relative`, creating directories.
    fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().expect("a fixture file has a directory"))
            .expect("the fixture directory should be creatable");
        std::fs::write(path, text).expect("the fixture file should be writable");
    }

    /// Copies the built binary to `relative`, executable.
    fn place_binary(&self, relative: &str) {
        let target = self.path(relative);
        std::fs::create_dir_all(target.parent().expect("a binary has a directory"))
            .expect("the binary's directory should be creatable");
        let _bytes =
            std::fs::copy(env!("CARGO_BIN_EXE_supdoc"), &target).expect("the binary should copy");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("the copy should be executable");
    }

    /// Installs a fake `cargo` that logs `<subcommand>: cwd=<dir> argv=<args>` to `cargo.log`,
    /// answers `search` with `search_result` as the latest version, and runs `install_body` for
    /// `install`.
    fn fake_cargo(&self, search_result: &str, install_body: &str) {
        let log = self.path("cargo.log");
        let script = format!(
            "#!/bin/sh\n\
             echo \"$1: cwd=$(pwd -P) argv=$*\" >> '{log}'\n\
             case \"$1\" in\n\
             search) echo 'supdoc = \"{search_result}\"    # Track links' ;;\n\
             install) {install_body} ;;\n\
             esac\n",
            log = log.display()
        );
        self.write("fakebin/cargo", &script);
        std::fs::set_permissions(
            self.path("fakebin/cargo"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("the fake cargo should be executable");
    }

    /// What the fake cargo logged, one line per call.
    fn cargo_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("cargo.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Runs `supdoc args` using the binary at `relative` with the fake cargo first on `PATH`.
    fn run_at(&self, relative: &str, args: &[&str]) -> Run {
        let output = Command::new(self.path(relative))
            .args(args)
            .current_dir(self.path("work"))
            .env_clear()
            .env("PATH", self.path("fakebin"))
            .env("CARGO_HOME", self.path("cargo-home"))
            .env("HOME", self.path("home"))
            .stdin(Stdio::null())
            .output()
            .expect("the supdoc binary should start");
        Run {
            args: args.join(" "),
            stdout: String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
            code: output.status.code(),
        }
    }

    /// Runs `supdoc args` as the cargo-installed binary.
    fn run(&self, args: &[&str]) -> Run {
        self.run_at("cargo-home/bin/supdoc", args)
    }

    /// The real path of the cargo home, which is the directory cargo should run in.
    fn cargo_home(&self) -> PathBuf {
        std::fs::canonicalize(self.path("cargo-home")).expect("the cargo home should exist")
    }
}

#[test]
fn preview_prints_current_target_and_the_exact_command_and_does_not_run_cargo_install() {
    let fixture = Fixture::installed("0.0.0");

    let run = fixture.run(&["upgrade", NEWER]);

    let _ = run.assert_exit(0);
    assert!(
        run.stdout.contains(&format!("current  {RUNNING}")),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!("target   {NEWER} (upgrade)")),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!(
            "command  cargo install supdoc --locked --version ={NEWER} --force"
        )),
        "{}",
        run.stdout
    );
    assert_eq!(
        fixture.cargo_calls(),
        Vec::<String>::new(),
        "a preview must not run cargo"
    );
    assert!(
        run.stderr
            .contains(&format!("supdoc upgrade {NEWER} --yes")),
        "the preview should say how to apply it: {}",
        run.stderr
    );
}

#[test]
fn yes_runs_exactly_cargo_install_locked_exact_version_force_from_cargo_home() {
    let fixture = Fixture::installed("0.0.0");

    let run = fixture.run(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        fixture.cargo_calls(),
        vec![format!(
            "install: cwd={} argv=install supdoc --locked --version ={NEWER} --force",
            fixture.cargo_home().display()
        )]
    );
}

#[test]
fn the_same_version_reports_already_on_and_runs_nothing() {
    let fixture = Fixture::installed("0.0.0");

    for args in [vec!["upgrade", RUNNING], vec!["upgrade", RUNNING, "--yes"]] {
        let run = fixture.run(&args);
        let _ = run.assert_exit(0);
        assert!(
            run.stdout.contains(&format!("already on {RUNNING}")),
            "`supdoc {}` should say so: {}",
            run.args,
            run.stdout
        );
    }
    assert_eq!(fixture.cargo_calls(), Vec::<String>::new());
}

#[test]
fn a_downgrade_preview_says_downgrade() {
    let fixture = Fixture::installed("0.0.0");

    let run = fixture.run(&["upgrade", OLDER]);

    let _ = run.assert_exit(0);
    assert!(
        run.stdout
            .contains(&format!("target   {OLDER} (downgrade)")),
        "{}",
        run.stdout
    );
    assert_eq!(fixture.cargo_calls(), Vec::<String>::new());
}

#[test]
fn no_version_reads_the_cargo_search_output() {
    let fixture = Fixture::installed("99.1.0");

    let run = fixture.run(&["upgrade"]);

    let _ = run.assert_exit(0);
    assert!(
        run.stdout.contains("target   99.1.0 (upgrade)"),
        "{}",
        run.stdout
    );
    assert_eq!(
        fixture.cargo_calls(),
        vec![format!(
            "search: cwd={} argv=search supdoc --limit 1",
            fixture.cargo_home().display()
        )],
        "only the search runs in a preview"
    );
}

#[test]
fn a_search_with_no_version_line_is_an_error_naming_the_command_and_its_output() {
    let fixture = Fixture::installed("0.0.0");
    fixture.write("fakebin/cargo", "#!/bin/sh\necho 'no crates found'\n");
    std::fs::set_permissions(
        fixture.path("fakebin/cargo"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("the fake cargo should be executable");

    let run = fixture.run(&["upgrade"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("cargo search supdoc --limit 1"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("no crates found"), "{}", run.stderr);
}

#[test]
fn a_bad_version_and_a_v_prefix_exit_2_before_cargo_runs() {
    let fixture = Fixture::installed("0.0.0");

    for bad in ["v0.3.1", "1.2", "latest", "01.2.3", "1.2.3.4"] {
        let run = fixture.run(&["upgrade", bad, "--yes"]);
        let _ = run.assert_exit(2);
    }
    let run = fixture.run(&["upgrade", "v0.3.1"]);
    assert!(
        run.stderr.contains("write `0.3.1`, not `v0.3.1`"),
        "{}",
        run.stderr
    );
    assert_eq!(fixture.cargo_calls(), Vec::<String>::new());
}

#[test]
fn a_version_missing_from_crates_io_gets_cargos_message_then_one_added_line() {
    let fixture = Fixture::installed("0.0.0");
    fixture.fake_cargo(
        "0.0.0",
        "echo 'error: could not find `supdoc` in registry `crates-io` with version `=99.0.0`' >&2; exit 101",
    );

    let run = fixture.run(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(1);
    let cargo = run
        .stderr
        .find("error: could not find `supdoc` in registry `crates-io` with version `=99.0.0`")
        .unwrap_or_else(|| panic!("cargo's own message should pass through: {}", run.stderr));
    let added = run
        .stderr
        .find("does not exist on crates.io")
        .unwrap_or_else(|| panic!("the added line should be present: {}", run.stderr));
    assert!(
        cargo < added,
        "the added line comes after cargo's: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("`supdoc upgrade` with no version"),
        "{}",
        run.stderr
    );
}

#[test]
fn another_cargo_failure_exits_1_without_the_added_line() {
    let fixture = Fixture::installed("0.0.0");
    fixture.fake_cargo("0.0.0", "echo 'error: linker failed' >&2; exit 101");

    let run = fixture.run(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("error: linker failed"),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("crates.io"), "{}", run.stderr);
}

#[test]
fn a_missing_cargo_exits_74() {
    let fixture = Fixture::installed("0.0.0");
    std::fs::remove_file(fixture.path("fakebin/cargo")).expect("the fake cargo should go");

    let run = fixture.run(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(74);
    assert!(run.stderr.contains("cannot run `cargo`"), "{}", run.stderr);
}

/// Runs `upgrade 99.0.0 --yes` with the binary at `relative` and checks it refuses with 78,
/// naming `saw` and `step`, without running cargo.
fn assert_refused(fixture: &Fixture, relative: &str, saw: &str, step: &str) {
    let run = fixture.run_at(relative, &["upgrade", NEWER, "--yes"]);
    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains(saw),
        "should name `{saw}`: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains(step),
        "should give `{step}`: {}",
        run.stderr
    );
    assert!(run.stderr.contains("cli-upgrade-not-cargo") || run.stderr.contains(saw));
    assert_eq!(
        fixture.cargo_calls(),
        Vec::<String>::new(),
        "a refusal must not run cargo"
    );
}

#[test]
fn a_homebrew_install_is_refused_with_brew_upgrade() {
    let fixture = Fixture::installed("0.0.0");
    fixture.place_binary("prefix/Cellar/supdoc/1.0.0/bin/supdoc");

    assert_refused(
        &fixture,
        "prefix/Cellar/supdoc/1.0.0/bin/supdoc",
        "installed by Homebrew",
        "brew upgrade supdoc",
    );
}

#[test]
fn a_build_in_a_target_directory_is_refused_with_pull_and_rebuild() {
    let fixture = Fixture::installed("0.0.0");
    fixture.place_binary("checkout/target/debug/supdoc");

    assert_refused(
        &fixture,
        "checkout/target/debug/supdoc",
        "build from source",
        "cargo install supdoc --locked",
    );
}

#[test]
fn a_binary_that_cargo_did_not_install_is_refused_as_unknown() {
    let fixture = Fixture::installed("0.0.0");
    fixture.place_binary("somewhere/bin/supdoc");

    assert_refused(
        &fixture,
        "somewhere/bin/supdoc",
        "can't tell how this copy",
        "cargo install supdoc --locked",
    );
}

#[test]
fn a_cargo_record_from_git_is_refused_naming_the_source() {
    let fixture = Fixture::installed("0.0.0");
    fixture.write(
        "cargo-home/.crates2.json",
        &crates_io_record(RUNNING).replace(
            "registry+https://github.com/rust-lang/crates.io-index",
            "git+https://example.com/supdoc#abc123",
        ),
    );

    assert_refused(
        &fixture,
        "cargo-home/bin/supdoc",
        "git+https://example.com/supdoc#abc123",
        "Reinstall it the same way",
    );
}

#[test]
fn a_missing_or_foreign_record_is_refused_as_unknown() {
    let fixture = Fixture::installed("0.0.0");
    fixture.write("cargo-home/.crates2.json", r#"{"installs":{}}"#);
    assert_refused(
        &fixture,
        "cargo-home/bin/supdoc",
        "can't tell how this copy",
        "cargo install supdoc --locked",
    );

    std::fs::remove_file(fixture.path("cargo-home/.crates2.json")).expect("the record should go");
    assert_refused(
        &fixture,
        "cargo-home/bin/supdoc",
        "can't tell how this copy",
        "cargo install supdoc --locked",
    );
}

#[test]
fn a_record_for_another_version_than_the_running_binary_is_refused() {
    let fixture = Fixture::installed("0.0.0");
    fixture.write("cargo-home/.crates2.json", &crates_io_record("98.0.0"));

    assert_refused(
        &fixture,
        "cargo-home/bin/supdoc",
        "cargo's install record says 98.0.0",
        "cargo install supdoc --locked",
    );
}

#[test]
fn json_reports_the_versions_the_command_and_whether_it_ran() {
    let fixture = Fixture::installed("0.0.0");

    let preview = fixture.run(&["upgrade", NEWER, "--json"]);
    let _ = preview.assert_exit(0);
    let preview = preview.json();
    let data = super::support::at(&preview, "/data");
    assert_eq!(data["current"], RUNNING);
    assert_eq!(data["target"], NEWER);
    assert_eq!(data["direction"], "upgrade");
    assert_eq!(
        data["command"],
        serde_json::json!([
            "cargo",
            "install",
            "supdoc",
            "--locked",
            "--version",
            format!("={NEWER}"),
            "--force"
        ])
    );
    assert_eq!(data["ran"], false);
    assert_eq!(data["outcome"], "previewed");

    let applied = fixture.run(&["upgrade", OLDER, "--yes", "--json"]);
    let _ = applied.assert_exit(0);
    let applied = applied.json();
    let data = super::support::at(&applied, "/data");
    assert_eq!(data["direction"], "downgrade");
    assert_eq!(data["ran"], true);
    assert_eq!(data["outcome"], "installed");

    let same = fixture.run(&["upgrade", RUNNING, "--json"]);
    let same = same.json();
    let data = super::support::at(&same, "/data");
    assert_eq!(data["direction"], "same");
    assert_eq!(data["command"], serde_json::json!([]));
    assert_eq!(data["outcome"], "already-current");
}
