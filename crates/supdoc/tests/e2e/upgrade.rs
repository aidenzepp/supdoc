//! `supdoc upgrade`: every case runs a copy of the built binary placed where the case needs it,
//! with a fake `cargo` first on `PATH` that records its argument vector and working directory.
//! No test reaches the network.

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

use super::support::Run;

/// How many times a spawn is tried when the copied binary is busy.
const BUSY_ATTEMPTS: u32 = 5;

/// Runs `spawn`, retrying while it fails with `ETXTBSY`. On Linux a binary just copied can refuse to
/// start while another test's fork still holds the write descriptor, so a busy spawn is retried a
/// few times, 20 ms apart; any other result is returned at once.
fn retry_busy<T>(mut spawn: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut attempt = 1;
    loop {
        match spawn() {
            Err(error)
                if error.kind() == ErrorKind::ExecutableFileBusy && attempt < BUSY_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

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
        self.custom_cargo(&format!(
            "case \"$1\" in\n\
             search) echo 'supdoc = \"{search_result}\"    # Track links' ;;\n\
             install) {install_body} ;;\n\
             esac\n"
        ));
    }

    /// Installs a fake `cargo` that logs each call to `cargo.log`, records whether its standard
    /// input was `eof` or `data` to `stdin.log`, then runs `body`.
    fn custom_cargo(&self, body: &str) {
        let log = self.path("cargo.log");
        let stdin_log = self.path("stdin.log");
        let script = format!(
            "#!/bin/sh\n\
             echo \"$1: cwd=$(pwd -P) argv=$*\" >> '{log}'\n\
             if read -r _line; then state=data; else state=eof; fi\n\
             echo \"$1: $state\" >> '{stdin_log}'\n\
             {body}",
            log = log.display(),
            stdin_log = stdin_log.display()
        );
        self.write("fakebin/cargo", &script);
        std::fs::set_permissions(
            self.path("fakebin/cargo"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("the fake cargo should be executable");
    }

    /// What each fake cargo call saw on its standard input, one `<subcommand>: eof|data` per call.
    fn stdin_states(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("stdin.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The cargo home as supdoc is given it, which is what `--root` names.
    fn cargo_home_given(&self) -> PathBuf {
        self.path("cargo-home")
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
        self.run_env_at(relative, args, &[])
    }

    /// [`Self::run_at`] with `vars` set after the defaults, so they override `CARGO_HOME` and the
    /// rest. Standard input holds a line of data, so a cargo that inherited it would read it.
    fn run_env_at(&self, relative: &str, args: &[&str], vars: &[(&str, &str)]) -> Run {
        self.write("stdin.txt", "data\n");
        let stdin = std::fs::File::open(self.path("stdin.txt")).expect("stdin.txt should open");
        let mut command = Command::new(self.path(relative));
        let _builder = command
            .args(args)
            .current_dir(self.path("work"))
            .env_clear()
            .env("PATH", self.path("fakebin"))
            .env("CARGO_HOME", self.path("cargo-home"))
            .env("HOME", self.path("home"))
            .envs(vars.iter().copied())
            .stdin(Stdio::from(stdin));
        let output = retry_busy(|| command.output()).expect("the supdoc binary should start");
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
            "command  cargo install supdoc --locked --version ={NEWER} --force --root {} --color never",
            fixture.cargo_home_given().display()
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
            "install: cwd={} argv=install supdoc --locked --version ={NEWER} --force --root {} --color never",
            fixture.cargo_home().display(),
            fixture.cargo_home_given().display()
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
            "search: cwd={} argv=search supdoc --limit 1 --color never",
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

/// Where npm puts the platform package's binary in a project.
const NPM_BINARY: &str = "proj/node_modules/@supdoc/linux-x64/bin/supdoc";

/// Where pnpm puts it: in its content store, under a second `node_modules`.
const PNPM_BINARY: &str =
    "proj/node_modules/.pnpm/@supdoc+linux-x64@0.6.0/node_modules/@supdoc/linux-x64/bin/supdoc";

/// What an npm refusal names: the owner, and the npm command for the version asked for.
const NPM_SAW: &str = "installed from npm";
const NPM_STEP: &str = "npm install supdoc@99.0.0";

#[test]
fn a_binary_in_an_npm_package_is_refused_with_npm_install() {
    let fixture = Fixture::installed("0.0.0");
    fixture.place_binary(NPM_BINARY);

    assert_refused(&fixture, NPM_BINARY, NPM_SAW, NPM_STEP);
}

#[test]
fn a_binary_in_a_pnpm_store_is_refused_with_npm_install() {
    let fixture = Fixture::installed("0.0.0");
    fixture.place_binary(PNPM_BINARY);

    assert_refused(&fixture, PNPM_BINARY, NPM_SAW, NPM_STEP);
}

#[test]
fn a_global_npm_install_under_a_homebrew_node_is_refused_as_npm_not_homebrew() {
    let fixture = Fixture::installed("0.0.0");
    let binary = "prefix/Cellar/node/24.0.0/lib/node_modules/@supdoc/linux-x64/bin/supdoc";
    fixture.place_binary(binary);

    assert_refused(&fixture, binary, NPM_SAW, NPM_STEP);
}

#[test]
fn an_npm_refusal_without_a_version_names_the_latest_package() {
    let fixture = Fixture::installed("0.0.0");
    fixture.place_binary(NPM_BINARY);

    let run = fixture.run_at(NPM_BINARY, &["upgrade", "--yes"]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains("npm install supdoc@latest"),
        "{}",
        run.stderr
    );
    assert_eq!(fixture.cargo_calls(), Vec::<String>::new());
}

#[test]
fn an_npm_refusal_drops_build_metadata_from_the_npm_command() {
    let fixture = Fixture::installed("0.0.0");
    fixture.place_binary(NPM_BINARY);

    let run = fixture.run_at(NPM_BINARY, &["upgrade", &format!("{NEWER}+build"), "--yes"]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains(&format!("npm install supdoc@{NEWER}`")),
        "the npm command should name the release without `+build`: {}",
        run.stderr
    );
    assert!(!run.stderr.contains("+build"), "{}", run.stderr);
    assert_eq!(fixture.cargo_calls(), Vec::<String>::new());
}

#[test]
fn a_directory_only_named_like_node_modules_is_not_taken_for_npm() {
    for relative in [
        "proj/my_node_modules/@supdoc/linux-x64/bin/supdoc",
        "proj/node_modules.bak/@supdoc/linux-x64/bin/supdoc",
    ] {
        let fixture = Fixture::installed("0.0.0");
        fixture.place_binary(relative);

        let run = fixture.run_at(relative, &["upgrade", NEWER, "--yes"]);

        let _ = run.assert_exit(78);
        assert!(
            !run.stderr.contains(NPM_SAW),
            "`{relative}` has no `node_modules` component: {}",
            run.stderr
        );
        assert!(
            run.stderr.contains("can't tell how this copy"),
            "`{relative}` should be refused as unknown: {}",
            run.stderr
        );
    }
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
            "--force",
            "--root",
            fixture.cargo_home_given().to_string_lossy(),
            "--color",
            "never"
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

#[test]
fn a_relative_cargo_home_is_refused_naming_cargo_home_not_telling_to_reinstall() {
    let fixture = Fixture::installed("0.0.0");

    let run = fixture.run_env_at(
        "cargo-home/bin/supdoc",
        &["upgrade", NEWER, "--yes"],
        &[("CARGO_HOME", "../cargo-home")],
    );

    let _ = run.assert_exit(78);
    assert!(
        run.stderr
            .contains("CARGO_HOME is set but is not an absolute path"),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("Reinstall"), "{}", run.stderr);
    assert_eq!(fixture.cargo_calls(), Vec::<String>::new());
}

#[test]
fn supdoc_yes_in_the_environment_applies_like_yes() {
    let fixture = Fixture::installed("0.0.0");

    let run = fixture.run_env_at(
        "cargo-home/bin/supdoc",
        &["upgrade", NEWER],
        &[("SUPDOC_YES", "1")],
    );

    let _ = run.assert_exit(0);
    assert_eq!(
        fixture.cargo_calls().len(),
        1,
        "SUPDOC_YES=1 should install"
    );
}

#[test]
fn colored_cargo_output_is_not_a_problem_because_both_calls_say_color_never() {
    let fixture = Fixture::installed("0.0.0");
    fixture.custom_cargo(
        "case \" $* \" in *\" --color never \"*) c=''; r='' ;; *) c=$(printf '\\033[1m'); r=$(printf '\\033[0m') ;; esac\n\
         case \"$1\" in\n\
         search) echo \"${c}supdoc${r} = \\\"99.1.0\\\"\" ;;\n\
         install) echo \"error: could not find \\`${c}supdoc${r}\\` in registry\" >&2; exit 101 ;;\n\
         esac\n",
    );

    let latest = fixture.run(&["upgrade"]);
    let _ = latest.assert_exit(0);
    assert!(
        latest.stdout.contains("target   99.1.0"),
        "{}",
        latest.stdout
    );

    let missing = fixture.run(&["upgrade", NEWER, "--yes"]);
    let _ = missing.assert_exit(1);
    assert!(
        missing.stderr.contains("does not exist on crates.io"),
        "{}",
        missing.stderr
    );
}

#[test]
fn cargo_gets_closed_standard_input_for_the_search_and_the_install() {
    let fixture = Fixture::installed("99.1.0");

    let run = fixture.run(&["upgrade", "--yes"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        fixture.stdin_states(),
        vec!["search: eof".to_owned(), "install: eof".to_owned()],
        "supdoc was given open input with data; cargo must not see it"
    );
}

#[test]
fn json_with_yes_keeps_standard_output_to_one_document_and_sends_cargos_to_standard_error() {
    let fixture = Fixture::installed("0.0.0");
    fixture.fake_cargo("0.0.0", "echo 'cargo-said-this-on-stdout'");

    let run = fixture.run(&["upgrade", NEWER, "--yes", "--json"]);

    let _ = run.assert_exit(0);
    let report = run.json();
    assert_eq!(super::support::at(&report, "/data/outcome"), "installed");
    assert!(
        run.stderr.contains("cargo-said-this-on-stdout"),
        "cargo's stdout should arrive on stderr: {}",
        run.stderr
    );
}

#[test]
fn a_closed_standard_error_does_not_stop_the_install_under_json() {
    use std::io::Read as _;
    let fixture = Fixture::installed("0.0.0");
    fixture.custom_cargo(&format!(
        "case \"$1\" in install)\n\
         echo A >&2; sleep 0.5; echo OUT-1; sleep 0.5; echo OUT-2\n\
         : > '{}' ;; esac\n",
        fixture.path("finished").display()
    ));
    let mut command = Command::new(fixture.path("cargo-home/bin/supdoc"));
    let _builder = command
        .args(["upgrade", NEWER, "--yes", "--json"])
        .current_dir(fixture.path("work"))
        .env_clear()
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", fixture.path("fakebin").display()),
        )
        .env("CARGO_HOME", fixture.path("cargo-home"))
        .env("HOME", fixture.path("home"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = retry_busy(|| command.spawn()).expect("the supdoc binary should start");
    let mut stderr = child.stderr.take().expect("stderr should be piped");
    let mut first = [0_u8; 1];
    stderr
        .read_exact(&mut first)
        .expect("cargo's first stderr byte should arrive");
    drop(stderr);

    let output = child.wait_with_output().expect("supdoc should finish");

    assert_eq!(
        output.status.code(),
        Some(0),
        "the install should complete; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        fixture.path("finished").exists(),
        "cargo should have run to its end instead of dying on a closed pipe"
    );
}

/// The eight release platforms, in the order the fixture lists them.
const TRIPLES: [&str; 8] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

/// The release platform of the binary under test.
fn host_triple() -> &'static str {
    if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

/// The digest the fixture gives `triple`, the same for every version: a different one per
/// platform, so a wrong pick shows.
fn digest_of(triple: &str) -> String {
    let index = TRIPLES
        .iter()
        .position(|candidate| *candidate == triple)
        .expect("the triple should be one of the eight");
    format!("{:064x}", index + 1)
}

/// A `SHA256SUMS` listing every platform for each of `versions`, the host's platform last, so a
/// pick of the first line shows.
fn sums_for(versions: &[&str]) -> String {
    let host = host_triple();
    let ordered = TRIPLES
        .iter()
        .filter(|triple| **triple != host)
        .chain(TRIPLES.iter().filter(|triple| **triple == host));
    ordered
        .flat_map(|triple| {
            versions.iter().map(move |version| {
                format!("{}  supdoc-{version}-{triple}.tar.gz\n", digest_of(triple))
            })
        })
        .collect::<Vec<_>>()
        .concat()
}

/// A binary outside any cargo home, and a release directory with one folder per version, each
/// holding a `SHA256SUMS` that lists that version for every platform. A version with no folder
/// answers 404, as a version with no release does. `latest` lists [`NEWER`].
fn released() -> Fixture {
    let fixture = Fixture::empty();
    fixture.place_binary("dist/supdoc");
    for version in [NEWER, OLDER] {
        fixture.write(
            &format!("releases/{version}/SHA256SUMS"),
            &sums_for(&[version]),
        );
    }
    fixture.write("releases/latest/SHA256SUMS", &sums_for(&[NEWER]));
    fixture
}

impl Fixture {
    /// Runs `supdoc args` as the downloaded binary, with the test release directory set.
    fn run_released(&self, args: &[&str]) -> Run {
        let releases = self.path("releases");
        self.run_env_at(
            "dist/supdoc",
            args,
            &[(
                "SUPDOC_TEST_RELEASE_DIR",
                releases.to_str().expect("the path should be UTF-8"),
            )],
        )
    }
}

#[test]
fn a_release_binary_previews_the_archive_and_digest_without_cargo() {
    let fixture = released();

    let run = fixture.run_released(&["upgrade", NEWER]);

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
    let triple = host_triple();
    assert!(
        run.stdout.contains(&format!(
            "asset    https://github.com/aidenzepp/supdoc/releases/download/v{NEWER}/supdoc-{NEWER}-{triple}.tar.gz"
        )),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(&format!("sha256   {}", digest_of(triple))),
        "the digest should be the host platform's line: {}",
        run.stdout
    );
    assert!(!run.stdout.contains("command"), "{}", run.stdout);
    assert!(
        run.stderr
            .contains(&format!("supdoc upgrade {NEWER} --yes")),
        "the preview should say how to apply it: {}",
        run.stderr
    );
}

#[test]
fn a_release_binary_downgrade_preview_says_downgrade() {
    let fixture = released();

    let run = fixture.run_released(&["upgrade", OLDER]);

    let _ = run.assert_exit(0);
    assert!(
        run.stdout
            .contains(&format!("target   {OLDER} (downgrade)")),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains(&format!("supdoc-{OLDER}-{}.tar.gz", host_triple())),
        "{}",
        run.stdout
    );
}

#[test]
fn a_release_binary_with_no_version_takes_the_newest_in_sha256sums() {
    let fixture = released();

    let run = fixture.run_released(&["upgrade", "--json"]);

    let _ = run.assert_exit(0);
    let json = run.json();
    assert_eq!(super::support::at(&json, "/data/target"), NEWER);
}

#[test]
fn json_for_a_release_binary_names_the_method_asset_and_digest() {
    let fixture = released();
    let triple = host_triple();

    let upgrade = fixture.run_released(&["upgrade", NEWER, "--json"]);
    let _ = upgrade.assert_exit(0);
    let upgrade = upgrade.json();
    let data = super::support::at(&upgrade, "/data");
    assert_eq!(data["method"], "release-binary");
    assert_eq!(data["current"], RUNNING);
    assert_eq!(data["target"], NEWER);
    assert_eq!(data["direction"], "upgrade");
    assert_eq!(data["command"], serde_json::json!([]));
    assert_eq!(data["ran"], false);
    assert_eq!(data["outcome"], "previewed");
    assert_eq!(
        data["asset"],
        format!(
            "https://github.com/aidenzepp/supdoc/releases/download/v{NEWER}/supdoc-{NEWER}-{triple}.tar.gz"
        )
    );
    assert_eq!(data["sha256"], digest_of(triple));

    let downgrade = fixture.run_released(&["upgrade", OLDER, "--json"]);
    let downgrade = downgrade.json();
    assert_eq!(
        super::support::at(&downgrade, "/data/direction"),
        "downgrade"
    );

    let same = fixture.run_released(&["upgrade", RUNNING, "--json"]);
    let _ = same.assert_exit(0);
    let same = same.json();
    let data = super::support::at(&same, "/data");
    assert_eq!(data["direction"], "same");
    assert_eq!(data["outcome"], "already-current");
    assert_eq!(data["method"], "release-binary");
    assert!(data.get("asset").is_none(), "{data}");
    assert!(data.get("sha256").is_none(), "{data}");
}

#[test]
fn a_version_with_no_release_is_no_asset_not_a_fetch_failure() {
    let fixture = released();

    let run = fixture.run_released(&["upgrade", "0.0.2"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("cli-upgrade-no-asset"),
        "{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("cli-upgrade-fetch-failed"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("release 0.0.2 has no SHA256SUMS"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("Download an archive by hand from the supdoc releases page"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("cargo install supdoc --locked --version =0.0.2"),
        "{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "{}", run.stdout);
}

#[test]
fn an_old_release_without_the_host_platform_names_the_first_release_that_has_binaries() {
    let fixture = released();
    fixture.write(
        "releases/0.0.2/SHA256SUMS",
        &format!(
            "{}  supdoc-0.0.2-x86_64-pc-windows-msvc.tar.gz\n",
            digest_of("x86_64-pc-windows-msvc")
        ),
    );

    let run = fixture.run_released(&["upgrade", "0.0.2"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("release 0.0.2 lists no binary for"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("Releases before 0.5.0 have no binaries"),
        "{}",
        run.stderr
    );
}

#[test]
fn json_for_a_version_with_no_release_is_the_no_asset_refusal() {
    let fixture = released();

    let run = fixture.run_released(&["upgrade", "0.0.2", "--json"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stdout.contains("\"kind\":\"cli-upgrade-no-asset\"")
            || run.stderr.contains("\"kind\":\"cli-upgrade-no-asset\""),
        "stdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn an_explicit_running_version_is_already_current_without_any_fetch() {
    let fixture = released();
    std::fs::remove_dir_all(fixture.path("releases")).expect("the releases should be removable");

    let run = fixture.run_released(&["upgrade", RUNNING]);

    let _ = run.assert_exit(0);
    assert_eq!(run.stdout, format!("already on {RUNNING}\n"));
}

#[test]
fn build_metadata_does_not_name_a_release() {
    let fixture = released();

    let run = fixture.run_released(&["upgrade", &format!("{NEWER}+build")]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr
            .contains("build metadata, which does not name a release"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains(&format!("Write the version as `{NEWER}`")),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("pre-release"), "{}", run.stderr);

    let pre = fixture.run_released(&["upgrade", &format!("{NEWER}-rc.1")]);
    let _ = pre.assert_exit(1);
    assert!(pre.stderr.contains("is a pre-release"), "{}", pre.stderr);
}

/// A marked binary at `relative` that another tool owns is refused as before.
fn assert_marked_refused(relative: &str, saw: &str, step: &str) {
    assert_marked_refused_beside(relative, &[], saw, step);
}

/// [`assert_marked_refused`] with each of `beside` written as an empty file in the binary's
/// directory, and no cargo call allowed.
fn assert_marked_refused_beside(relative: &str, beside: &[&str], saw: &str, step: &str) {
    let fixture = Fixture::empty();
    fixture.place_binary(relative);
    let dir = relative.rsplit_once('/').map_or("", |(dir, _name)| dir);
    for name in beside {
        fixture.write(&format!("{dir}/{name}"), "");
    }
    fixture.write("releases/99.0.0/SHA256SUMS", &sums_for(&[NEWER]));
    let releases = fixture.path("releases");

    let run = fixture.run_env_at(
        relative,
        &["upgrade", NEWER],
        &[(
            "SUPDOC_TEST_RELEASE_DIR",
            releases.to_str().expect("the path should be UTF-8"),
        )],
    );

    let _ = run.assert_exit(78);
    assert!(run.stderr.contains(saw), "{}", run.stderr);
    assert!(run.stderr.contains(step), "{}", run.stderr);
    assert_eq!(
        fixture.cargo_calls(),
        Vec::<String>::new(),
        "a refusal must not run cargo"
    );
}

#[test]
fn a_marked_binary_under_homebrew_is_still_refused() {
    assert_marked_refused(
        "prefix/Cellar/supdoc/1.0.0/bin/supdoc",
        "installed by Homebrew",
        "brew upgrade supdoc",
    );
}

#[test]
fn a_marked_binary_under_target_is_still_refused() {
    assert_marked_refused(
        "checkout/target/debug/supdoc",
        "build from source",
        "cargo install supdoc --locked",
    );
}

#[test]
fn a_marked_binary_in_an_npm_package_is_still_refused() {
    assert_marked_refused(NPM_BINARY, NPM_SAW, NPM_STEP);
}

#[test]
fn a_marked_binary_in_a_pnpm_store_is_still_refused() {
    assert_marked_refused(PNPM_BINARY, NPM_SAW, NPM_STEP);
}

#[test]
fn a_marked_binary_beside_the_npm_marker_is_refused_outside_node_modules() {
    assert_marked_refused_beside(
        "deno/npm/registry.npmjs.org/@supdoc/linux-x64/0.6.0/bin/supdoc",
        &["installed-by-npm"],
        NPM_SAW,
        NPM_STEP,
    );
}

#[test]
fn a_release_without_the_host_platform_is_refused_as_no_asset() {
    let fixture = released();
    let others = sums_for(&[NEWER])
        .lines()
        .filter(|line| !line.contains(host_triple()))
        .map(|line| format!("{line}\n"))
        .collect::<Vec<_>>()
        .concat();
    fixture.write("releases/99.0.0/SHA256SUMS", &others);

    let run = fixture.run_released(&["upgrade", NEWER]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("cli-upgrade-no-asset"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains(host_triple()), "{}", run.stderr);
    assert!(
        !run.stderr.contains("Releases before"),
        "a new release should not blame the old ones: {}",
        run.stderr
    );
}

/// What the stub binary inside a fixture archive prints, so a run shows which file is installed.
const STUB: &[u8] = b"#!/bin/sh\necho stub-installed\n";

/// A `.tar.gz` holding each `(path, bytes)` as a regular file.
fn archive_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len()).expect("the entry size should fit"));
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, *bytes)
            .expect("the entry should append");
    }
    let tarball = tar.into_inner().expect("the tar should finish");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gzip, &tarball).expect("the tar should compress");
    gzip.finish().expect("the gzip should finish")
}

/// The SHA-256 of `bytes` as 64 lowercase hex digits.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The names in `dir`, sorted.
fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the directory should list")
        .map(|entry| {
            entry
                .expect("the entry should read")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

impl Fixture {
    /// Publishes `archive` as the host's archive of [`NEWER`], with `digest` as its line in
    /// `SHA256SUMS`.
    fn publish(&self, archive: &[u8], digest: &str) {
        let host = host_triple();
        let sums = sums_for(&[NEWER]).replace(&digest_of(host), digest);
        self.write(&format!("releases/{NEWER}/SHA256SUMS"), &sums);
        let path = self.path(&format!("releases/{NEWER}/supdoc-{NEWER}-{host}.tar.gz"));
        std::fs::write(path, archive).expect("the archive should write");
    }

    /// Publishes the stub in a well-formed archive with its true digest.
    fn publish_stub(&self) {
        let host = host_triple();
        let entry = format!("supdoc-{NEWER}-{host}/supdoc");
        let archive = archive_of(&[(entry.as_str(), STUB)]);
        self.publish(&archive, &sha256_hex(&archive));
    }
}

#[test]
fn yes_replaces_the_binary_with_the_archives_and_leaves_nothing_beside_it() {
    let fixture = released();
    fixture.publish_stub();

    let run = fixture.run_released(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        run.stderr.contains("Installed supdoc 99.0.0."),
        "{}",
        run.stderr
    );
    let installed = fixture.path("dist/supdoc");
    assert_eq!(
        std::fs::read(&installed).expect("the binary should read"),
        STUB
    );
    let mode = std::fs::metadata(&installed)
        .expect("the binary should have metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "the installed binary's mode");
    assert_eq!(names_in(&fixture.path("dist")), ["supdoc"]);
    let ran = retry_busy(|| Command::new(&installed).output()).expect("the stub should run");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "stub-installed\n");
}

#[test]
fn yes_removes_the_files_an_earlier_upgrade_left_and_only_those() {
    let fixture = released();
    fixture.publish_stub();
    let dist = fixture.path("dist");
    std::fs::write(dist.join(".supdoc-upgrade-123-456"), "stale")
        .expect("the leftover should write");
    std::fs::write(dist.join("supdoc.exe.old"), "aside").expect("the aside should write");
    std::fs::write(dist.join("supdoc-upgrade"), "foreign").expect("the foreign file should write");
    std::fs::write(dist.join(".supdoc-upgrade"), "foreign").expect("the bare prefix should write");
    std::fs::write(dist.join(".supdoc-upgrade-notes.txt"), "foreign")
        .expect("the near-miss name should write");
    std::fs::create_dir(dist.join(".supdoc-upgrade-5-6")).expect("the directory should create");
    std::fs::write(dist.join("target-of-link"), "keep").expect("the link target should write");
    std::os::unix::fs::symlink(
        dist.join("target-of-link"),
        dist.join(".supdoc-upgrade-7-8"),
    )
    .expect("the symlink should be creatable");

    let run = fixture.run_released(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        !run.stderr.contains("Could not"),
        "skipping a directory or a link is not a failure to report: {}",
        run.stderr
    );
    assert_eq!(
        names_in(&dist),
        [
            ".supdoc-upgrade",
            ".supdoc-upgrade-5-6",
            ".supdoc-upgrade-7-8",
            ".supdoc-upgrade-notes.txt",
            "supdoc",
            "supdoc-upgrade",
            "target-of-link"
        ]
    );
    assert_eq!(
        std::fs::read(dist.join("target-of-link")).expect("the target should read"),
        b"keep"
    );
    let installed = dist.join("supdoc");
    assert_eq!(
        std::fs::read(&installed).expect("the binary should read"),
        STUB
    );
    let ran = retry_busy(|| Command::new(&installed).output()).expect("the stub should run");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "stub-installed\n");
}

#[test]
fn json_for_an_applied_release_binary_upgrade_says_installed() {
    let fixture = released();
    fixture.publish_stub();

    let run = fixture.run_released(&["--json", "upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        run.stdout.contains(r#""outcome":"installed""#),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains(r#""ran":true"#), "{}", run.stdout);
    let json = run.json();
    assert_eq!(
        super::support::at(&json, "/data/notes"),
        &serde_json::json!([]),
        "a clean run carries an empty notes array"
    );
}

#[test]
fn a_failed_replace_names_the_leftover_it_could_not_remove_in_text_and_json() {
    if !crate::permissions_restrict_this_user("a_failed_replace_names_the_leftover") {
        return;
    }
    let fixture = released();
    fixture.publish_stub();
    let dist = fixture.path("dist");
    std::fs::write(dist.join(".supdoc-upgrade-123-456"), "stale")
        .expect("the leftover should write");
    std::fs::set_permissions(&dist, std::fs::Permissions::from_mode(0o555))
        .expect("the directory should lock");

    let text = fixture.run_released(&["upgrade", NEWER, "--yes"]);
    let json = fixture.run_released(&["--json", "upgrade", NEWER, "--yes"]);

    std::fs::set_permissions(&dist, std::fs::Permissions::from_mode(0o755))
        .expect("the directory should unlock");
    let _ = text.assert_exit(74);
    assert!(
        text.stderr.contains("cli-upgrade-replace-failed")
            && text.stderr.contains(".supdoc-upgrade-123-456"),
        "{}",
        text.stderr
    );
    let _ = json.assert_exit(74);
    assert!(
        json.stderr.contains("cli-upgrade-replace-failed")
            && json.stderr.contains(".supdoc-upgrade-123-456"),
        "{}",
        json.stderr
    );
}

#[test]
fn a_digest_mismatch_is_refused_before_the_archive_is_read_and_changes_nothing() {
    let fixture = released();
    fixture.publish(
        b"not an archive at all",
        &digest_of("x86_64-unknown-linux-gnu"),
    );
    let before = std::fs::read(fixture.path("dist/supdoc")).expect("the binary should read");
    std::fs::write(fixture.path("dist/.supdoc-upgrade-123-456"), "stale")
        .expect("the leftover should write");

    let run = fixture.run_released(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("cli-upgrade-verify-failed"),
        "{}",
        run.stderr
    );
    assert_eq!(
        names_in(&fixture.path("dist")),
        [".supdoc-upgrade-123-456", "supdoc"],
        "a refused archive must not remove anything"
    );
    assert!(
        run.stderr.contains("SHA256SUMS lists"),
        "a bad digest should be reported before the archive is opened: {}",
        run.stderr
    );
    assert_eq!(
        std::fs::read(fixture.path("dist/supdoc")).expect("the binary should read"),
        before,
        "the binary must be untouched"
    );
}

#[test]
fn an_archive_without_the_expected_entry_is_refused_and_changes_nothing() {
    let fixture = released();
    let host = host_triple();
    let wrong = format!("supdoc-{NEWER}-{host}/other");
    let archive = archive_of(&[(wrong.as_str(), STUB)]);
    fixture.publish(&archive, &sha256_hex(&archive));
    let before = std::fs::read(fixture.path("dist/supdoc")).expect("the binary should read");

    let run = fixture.run_released(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("cli-upgrade-verify-failed"),
        "{}",
        run.stderr
    );
    assert_eq!(
        std::fs::read(fixture.path("dist/supdoc")).expect("the binary should read"),
        before,
        "the binary must be untouched"
    );
    assert_eq!(names_in(&fixture.path("dist")), ["supdoc"]);
}

#[test]
fn a_missing_archive_is_a_fetch_failure_and_changes_nothing() {
    let fixture = released();
    let before = std::fs::read(fixture.path("dist/supdoc")).expect("the binary should read");

    let run = fixture.run_released(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(74);
    assert!(
        run.stderr.contains("cli-upgrade-fetch-failed"),
        "{}",
        run.stderr
    );
    assert_eq!(
        std::fs::read(fixture.path("dist/supdoc")).expect("the binary should read"),
        before,
        "the binary must be untouched"
    );
}

#[test]
fn an_unwritable_directory_leaves_the_binary_byte_identical_and_no_temp_file() {
    let fixture = released();
    fixture.publish_stub();
    let dist = fixture.path("dist");
    let before = std::fs::read(dist.join("supdoc")).expect("the binary should read");
    std::fs::set_permissions(&dist, std::fs::Permissions::from_mode(0o555))
        .expect("the directory should lock");
    let restricted = std::fs::write(dist.join("probe"), "").is_err();

    let run = fixture.run_released(&["upgrade", NEWER, "--yes"]);

    std::fs::set_permissions(&dist, std::fs::Permissions::from_mode(0o755))
        .expect("the directory should unlock");
    if !restricted {
        std::fs::remove_file(dist.join("probe")).expect("the probe should remove");
        return;
    }
    let _ = run.assert_exit(74);
    assert!(
        run.stderr.contains("cli-upgrade-replace-failed"),
        "{}",
        run.stderr
    );
    assert_eq!(
        std::fs::read(dist.join("supdoc")).expect("the binary should read"),
        before,
        "the binary must be untouched"
    );
    assert_eq!(names_in(&dist), ["supdoc"]);
}

#[test]
fn a_symlinked_directory_is_replaced_at_its_real_location() {
    let fixture = released();
    fixture.publish_stub();
    let real = fixture.path("elsewhere");
    std::fs::rename(fixture.path("dist"), &real).expect("the directory should move");
    std::os::unix::fs::symlink(&real, fixture.path("dist"))
        .expect("the fixture symlink should be creatable");

    let run = fixture.run_released(&["upgrade", NEWER, "--yes"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        std::fs::read(real.join("supdoc")).expect("the binary should read"),
        STUB
    );
    assert!(
        fixture
            .path("dist")
            .symlink_metadata()
            .expect("the link should exist")
            .is_symlink(),
        "the symlink itself must stay a symlink"
    );
    assert_eq!(names_in(&real), ["supdoc"]);
}

#[test]
fn a_missing_or_malformed_sha256sums_is_a_fetch_failure() {
    let fixture = released();
    fixture.write("releases/99.0.0/SHA256SUMS", "not a checksum file\n");

    let run = fixture.run_released(&["upgrade", NEWER]);

    let _ = run.assert_exit(74);
    assert!(
        run.stderr.contains("cli-upgrade-fetch-failed"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_binary_cargo_recorded_keeps_the_cargo_route_even_with_the_release_marker() {
    let fixture = Fixture::installed("0.0.0");
    fixture.write("releases/99.0.0/SHA256SUMS", &sums_for(&[NEWER]));
    let releases = fixture.path("releases");

    let run = fixture.run_env_at(
        "cargo-home/bin/supdoc",
        &["upgrade", NEWER, "--json"],
        &[(
            "SUPDOC_TEST_RELEASE_DIR",
            releases.to_str().expect("the path should be UTF-8"),
        )],
    );

    let _ = run.assert_exit(0);
    let json = run.json();
    let data = super::support::at(&json, "/data");
    assert_eq!(data["method"], "cargo");
    assert_eq!(super::support::at(&json, "/data/command/0"), "cargo");
    assert!(data.get("asset").is_none(), "{data}");
}

#[test]
fn without_the_marker_a_downloaded_binary_is_still_refused() {
    let fixture = released();

    let run = fixture.run_at("dist/supdoc", &["upgrade", NEWER]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains("cli-upgrade-not-cargo"),
        "{}",
        run.stderr
    );
}
