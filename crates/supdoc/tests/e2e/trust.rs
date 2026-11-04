//! `supdoc trust` and `supdoc trust --revoke`: the trust store they write, the root they pick and
//! the one line they print.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use crate::support::Project;

/// A scratch state directory, and the `trusted-roots` file inside it.
struct State {
    /// Removed on drop.
    dir: TempDir,
}

impl State {
    fn new() -> Self {
        Self {
            dir: TempDir::new().expect("a temporary state directory should be creatable"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The lines of the trust store, empty when it doesn't exist.
    fn roots(&self) -> Vec<String> {
        std::fs::read_to_string(self.path().join("supdoc/trusted-roots"))
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

/// The `basic` fixture with an empty `supdoc.config.toml`, so the project root is known.
fn project() -> Project {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "");
    project
}

/// The real path of `path`.
fn real(path: &Path) -> String {
    path.canonicalize()
        .expect("the fixture path should resolve")
        .display()
        .to_string()
}

/// Runs `git` with `args` in `dir`, with no user config and a fixed identity.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=supdoc",
            "-c",
            "user.email=supdoc@example.invalid",
        ])
        .args(["-c", "commit.gpgsign=false", "-C"])
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git should be runnable to build the fixture repository");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

#[test]
fn trusting_then_revoking_the_root_round_trips() {
    let project = project();
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];
    let root = real(&project.path("."));

    let first = project.supdoc_in_with(".", &["trust"], &vars);
    let again = project.supdoc_in_with(".", &["trust"], &vars);
    let listed = state.roots();
    let revoked = project.supdoc_in_with(".", &["trust", "--revoke"], &vars);
    let after_revoke = state.roots();
    let revoked_again = project.supdoc_in_with(".", &["trust", "--revoke"], &vars);

    assert_eq!(
        (
            first.assert_exit(0).stdout.as_str(),
            again.assert_exit(0).stdout.as_str(),
            listed,
            revoked.assert_exit(0).stdout.as_str(),
            after_revoke,
            revoked_again.assert_exit(0).stdout.as_str(),
        ),
        (
            "trusted .\n",
            "already trusted .\n",
            vec![root],
            "revoked .\n",
            Vec::<String>::new(),
            "was not trusted .\n",
        ),
        "trust, trust again, revoke, revoke again should say so and keep the store in step"
    );
}

#[test]
fn the_confirmation_from_a_subdirectory_shows_the_absolute_project_root() {
    let project = project();
    std::fs::create_dir_all(project.path("sub/deep"))
        .expect("the subdirectory should be creatable");
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];
    let root = real(&project.path("."));

    let run = project.supdoc_in_with("sub/deep", &["trust"], &vars);

    let _ = run.assert_exit(0);
    assert_eq!(
        (run.stdout.as_str(), state.roots()),
        (format!("trusted {root}\n").as_str(), vec![root.clone()]),
        "trust from a subdirectory should list the config's directory and show its absolute path"
    );
}

#[test]
fn an_explicit_path_is_listed_by_its_real_path() {
    let project = project();
    std::fs::create_dir_all(project.path("sub")).expect("the subdirectory should be creatable");
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];

    let run = project.supdoc_in_with(".", &["trust", "sub"], &vars);

    let _ = run.assert_exit(0);
    assert_eq!(
        (run.stdout.as_str(), state.roots()),
        (
            format!("trusted {}\n", real(&project.path("sub"))).as_str(),
            vec![real(&project.path("sub"))]
        ),
        "an explicit PATH should be the root, not the project root"
    );
}

#[test]
fn an_explicit_path_that_is_not_a_directory_is_a_usage_error() {
    let project = project();
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];

    let missing = project.supdoc_in_with(".", &["trust", "nowhere"], &vars);
    let file = project.supdoc_in_with(".", &["trust", "supdoc.config.toml"], &vars);

    let _ = missing.assert_exit(2);
    let _ = file.assert_exit(2);
    assert_eq!(
        state.roots(),
        Vec::<String>::new(),
        "nothing should be listed"
    );
}

#[test]
fn trusting_from_a_linked_worktree_records_the_worktrees_own_root() {
    let project = project();
    git(&project.path("."), &["init", "--quiet"]);
    git(&project.path("."), &["add", "."]);
    git(&project.path("."), &["commit", "--quiet", "-m", "fixture"]);
    git(&project.path("."), &["worktree", "add", "--quiet", "wt"]);
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];

    let run = project.supdoc_in_with("wt", &["trust"], &vars);

    let _ = run.assert_exit(0);
    assert_eq!(
        state.roots(),
        vec![real(&project.path("wt"))],
        "the worktree's own root should be listed, not the main checkout's"
    );
}

#[test]
fn without_a_config_trust_refuses_and_lists_nothing() {
    let project = Project::fixture("basic");
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];

    let run = project.supdoc_in_with(".", &["trust"], &vars);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains("cli-trust-no-config") && state.roots().is_empty(),
        "no config should mean no root: {} {:?}",
        run.stderr,
        state.roots()
    );
}

#[test]
fn a_deleted_root_can_be_revoked() {
    let project = project();
    std::fs::create_dir_all(project.path("gone")).expect("the directory should be creatable");
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];
    let _ = project
        .supdoc_in_with(".", &["trust", "gone"], &vars)
        .assert_exit(0);
    std::fs::remove_dir(project.path("gone")).expect("the directory should be removable");

    let run = project.supdoc_in_with(".", &["trust", "--revoke", "gone"], &vars);

    let _ = run.assert_exit(0);
    assert_eq!(
        (run.stdout.as_str(), state.roots()),
        (
            format!("revoked {}/gone\n", real(&project.path("."))).as_str(),
            Vec::<String>::new()
        ),
        "a deleted root should be revocable by the path as typed"
    );
}

#[test]
fn revoking_a_root_that_became_a_symlink_prints_the_listed_entry() {
    let project = project();
    std::fs::create_dir_all(project.path("old")).expect("the directory should be creatable");
    std::fs::create_dir_all(project.path("target")).expect("the directory should be creatable");
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];
    let _ = project
        .supdoc_in_with(".", &["trust", "old"], &vars)
        .assert_exit(0);
    std::fs::remove_dir(project.path("old")).expect("the directory should be removable");
    std::os::unix::fs::symlink(project.path("target"), project.path("old"))
        .expect("the symlink should be creatable");

    let run = project.supdoc_in_with(".", &["trust", "--revoke", "old"], &vars);

    let _ = run.assert_exit(0);
    assert_eq!(
        (run.stdout.as_str(), state.roots()),
        (
            format!("revoked {}/old\n", real(&project.path("."))).as_str(),
            Vec::<String>::new()
        ),
        "the line should name the entry removed, not the symlink's target"
    );
}

#[test]
fn a_symlinked_path_records_the_real_path() {
    let project = project();
    std::fs::create_dir_all(project.path("real")).expect("the directory should be creatable");
    std::os::unix::fs::symlink(project.path("real"), project.path("link"))
        .expect("the symlink should be creatable");
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];

    let run = project.supdoc_in_with(".", &["trust", "link"], &vars);

    let _ = run.assert_exit(0);
    assert_eq!(
        state.roots(),
        vec![real(&project.path("real"))],
        "the store should list the symlink's target"
    );
}

#[test]
fn a_config_flag_that_check_refuses_is_refused_by_trust() {
    let project = project();
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];
    for config in [".", "missing.toml"] {
        let checked = project.supdoc_in_with(".", &["-c", config, "check"], &vars);
        let trusted = project.supdoc_in_with(".", &["-c", config, "trust"], &vars);
        assert_ne!(checked.code, Some(0), "check -c {config} should be refused");
        assert_eq!(
            trusted.code, checked.code,
            "trust -c {config} should exit as check does: {}",
            trusted.stderr
        );
    }
    assert_eq!(
        state.roots(),
        Vec::<String>::new(),
        "nothing should be listed"
    );
}

#[test]
fn a_path_with_a_newline_is_a_usage_error() {
    let project = project();
    std::fs::create_dir_all(project.path("a\nb")).expect("the directory should be creatable");
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];

    let run = project.supdoc_in_with(".", &["trust", "a\nb"], &vars);

    let _ = run.assert_exit(2);
    assert!(run.stderr.contains("cli-trust-path"), "{}", run.stderr);
}

#[test]
fn supdoc_revoke_turns_trust_into_revoke_and_says_so() {
    let project = project();
    let state = State::new();
    let vars = [("XDG_STATE_HOME", state.path())];
    let _ = project
        .supdoc_in_with(".", &["trust"], &vars)
        .assert_exit(0);

    let run = project.supdoc_in_with(
        ".",
        &["trust"],
        &[
            ("XDG_STATE_HOME", state.path()),
            ("SUPDOC_REVOKE", Path::new("1")),
        ],
    );

    let _ = run.assert_exit(0);
    assert_eq!(
        (run.stdout.as_str(), state.roots()),
        ("revoked .\n", Vec::<String>::new()),
        "SUPDOC_REVOKE=1 should revoke and say `revoked`"
    );
}

#[test]
fn the_supdoc_trust_variable_is_not_consulted() {
    let project = project();
    let state = State::new();

    // A value `check` would refuse as a usage error does not stop `trust`, and `1` is not what
    // `trust` records: it lists the root either way.
    let bogus = project.supdoc_in_with(
        ".",
        &["trust"],
        &[
            ("XDG_STATE_HOME", state.path()),
            ("SUPDOC_TRUST", Path::new("maybe")),
        ],
    );
    let revoke = project.supdoc_in_with(
        ".",
        &["trust", "--revoke"],
        &[
            ("XDG_STATE_HOME", state.path()),
            ("SUPDOC_TRUST", Path::new("1")),
        ],
    );

    let _ = bogus.assert_exit(0);
    let _ = revoke.assert_exit(0);
    assert_eq!(
        (bogus.stdout.as_str(), revoke.stdout.as_str(), state.roots()),
        ("trusted .\n", "revoked .\n", Vec::<String>::new()),
        "`SUPDOC_TRUST` should neither fail nor change what `trust` and `--revoke` do"
    );
}

#[test]
fn no_state_directory_is_a_config_error() {
    let project = project();

    let run = project.supdoc_in_with(".", &["trust"], &[("HOME", Path::new(""))]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains("cli-trust-no-state"),
        "the error should name the missing state directory: {}",
        run.stderr
    );
}

/// Runs `supdoc trust args` in `project` with `HOME` set to `home` and the store in `state`,
/// asserting it is refused as `cli-trust-broad-root` (2), names `named` and lists nothing.
fn assert_refused_as_broad(
    project: &Project,
    state: &State,
    home: &Path,
    args: &[&str],
    named: &str,
) {
    let vars = [("XDG_STATE_HOME", state.path()), ("HOME", home)];

    let run = project.supdoc_in_with(".", args, &vars);

    let _ = run.assert_exit(2);
    assert!(
        run.stderr.contains("cli-trust-broad-root") && run.stderr.contains(&format!("`{named}`")),
        "`supdoc {}` should be refused as too broad, naming {named}: {}",
        args.join(" "),
        run.stderr
    );
    assert!(
        !state.path().join("supdoc").exists(),
        "a refused root should leave the store untouched: {:?}",
        state.roots()
    );
}

#[test]
fn trusting_home_is_refused_and_leaves_the_store_unchanged() {
    let project = project();
    std::fs::create_dir_all(project.path("home")).expect("the home directory should be creatable");
    let state = State::new();

    assert_refused_as_broad(
        &project,
        &state,
        &project.path("home"),
        &["trust", "home"],
        &real(&project.path("home")),
    );
}

#[test]
fn trusting_an_ancestor_of_home_is_refused_and_leaves_the_store_unchanged() {
    let project = project();
    std::fs::create_dir_all(project.path("home/user"))
        .expect("the home directory should be creatable");
    let state = State::new();

    // The project root holds the config and sits above `HOME`, so its trust would reach `HOME`.
    assert_refused_as_broad(
        &project,
        &state,
        &project.path("home/user"),
        &["trust"],
        &real(&project.path(".")),
    );
}

#[test]
fn trusting_the_filesystem_root_is_refused_and_leaves_the_store_unchanged() {
    let project = project();
    let state = State::new();

    assert_refused_as_broad(
        &project,
        &state,
        &project.path("no-home"),
        &["trust", "/"],
        "/",
    );
}

#[test]
fn a_directory_under_home_can_still_be_trusted() {
    let project = project();
    std::fs::create_dir_all(project.path("home/work"))
        .expect("the home directory should be creatable");
    let state = State::new();
    let home = project.path("home");
    let vars = [("XDG_STATE_HOME", state.path()), ("HOME", home.as_path())];

    let run = project.supdoc_in_with(".", &["trust", "home/work"], &vars);

    let _ = run.assert_exit(0);
    assert_eq!(state.roots(), vec![real(&project.path("home/work"))]);
}
