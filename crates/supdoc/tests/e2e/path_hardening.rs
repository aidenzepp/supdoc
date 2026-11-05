//! The hostile-value matrix of the path-hardening design (§2.4, #336): for each input source, a
//! `..`, an absolute value or a symlinked parent is refused before any filesystem effect. Each
//! test stages one cell end to end and checks that nothing outside the project was read, shown or
//! written. Cells another suite already covers are named in the PH12 pull request, not repeated.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::containment::web_project;
use crate::support::{Project, Run, array_at};

/// The secret `web_project` writes outside its root, in `settings.py` and `.env`.
const SECRET_PARTS: [&str; 2] = ["s3cr3t", "hunter2"];

/// Fails if either stream of `run` shows any part of the outside secret.
fn assert_no_secret(run: &Run) {
    for leak in SECRET_PARTS {
        assert!(
            !run.stdout.contains(leak) && !run.stderr.contains(leak),
            "`supdoc {}` showed `{leak}`\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

/// A v1 lockfile with one `[[link]]` from `source` to `target`.
fn lock_with(source: &str, target: &str) -> String {
    format!(
        "# supdoc lockfile\nversion = 1\n\n[features.hash]\nline-endings = \"normalize\"\n\n\
         [[link]]\nsource = \"{source}\"\ntarget = \"{target}\"\n\
         hash = \"9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b\"\n"
    )
}

/// Runs `check` in the web project with `lock` as its lockfile, asserting a `lock-parse` refusal
/// that says `says` and shows nothing of the outside secret.
fn assert_lock_refused(lock: &str, says: &str) {
    let project = web_project();
    fs::write(project.path("services/web/supdoc.lock"), lock)
        .expect("the lockfile should be writable");

    let run = project.supdoc_in("services/web", &["check"]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains("lock-parse") && run.stderr.contains(says),
        "the lockfile `{lock}` should be a parse error saying `{says}`:\n{}",
        run.stderr
    );
    assert_no_secret(&run);
}

#[test]
fn a_lock_source_that_climbs_is_a_parse_error_and_nothing_is_read() {
    for source in ["../../settings.py", "docs/../../../settings.py"] {
        assert_lock_refused(&lock_with(source, "src/lib.rs"), "uses `..`");
    }
}

#[test]
fn a_lock_source_that_is_absolute_is_a_parse_error_and_nothing_is_read() {
    let project = web_project();
    let outside = project.path("settings.py").display().to_string();
    for source in [outside.as_str(), "//etc/passwd"] {
        assert_lock_refused(&lock_with(source, "src/lib.rs"), "is absolute");
    }
}

#[test]
fn a_lock_target_that_is_absolute_is_a_parse_error_and_nothing_is_read() {
    let project = web_project();
    let outside = project.path("settings.py").display().to_string();
    for target in [outside.as_str(), "//etc/passwd"] {
        assert_lock_refused(&lock_with("docs/guide.md", target), "is absolute");
    }
}

/// A marker an outside source file holds, which no run may show.
const MARKER: &str = "OUTSIDE_MARKER_4c1e";

/// How many `code-block-source-missing` findings a `--json` check printed.
fn source_missing_count(json: &serde_json::Value) -> usize {
    array_at(json, "/findings")
        .iter()
        .filter(|finding| finding["kind"] == "code-block-source-missing")
        .count()
}

/// A recorded `basic` project with code blocks on, an outside directory holding `secret.rs`
/// with [`MARKER`], and `src/vendor` a symlink to that directory.
fn project_with_outside_source() -> (Project, TempDir) {
    let outside = TempDir::new().expect("an outside directory should be creatable");
    fs::write(
        outside.path().join("secret.rs"),
        format!("pub fn secret() {{ let _ = \"{MARKER}\"; }}\n"),
    )
    .expect("the outside file should be writable");
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    symlink(outside.path(), project.path("src/vendor"))
        .expect("the directory symlink should be creatable");
    (project, outside)
}

#[test]
fn from_that_climbs_is_absolute_or_runs_through_a_symlinked_dir_shows_nothing_outside() {
    let (project, outside) = project_with_outside_source();
    let climb = format!(
        "../../{}/secret.rs#secret",
        outside
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .expect("the outside directory should have a UTF-8 name")
    );
    let absolute = format!("/{}/secret.rs#secret", outside.path().display());
    for (way, value, says) in [
        ("climbing", climb.as_str(), "climbs out of the project root"),
        (
            "absolute",
            absolute.as_str(),
            "climbs out of the project root",
        ),
        (
            "symlinked parent",
            "/src/vendor/secret.rs#secret",
            "is outside the project root",
        ),
    ] {
        project.write(
            "docs/blocks.md",
            &format!("# Blocks\n\n```rust from({value})\nfn secret() {{}}\n```\n"),
        );

        let json_run = project.supdoc(&["check", "--json"]);
        let text_run = project.supdoc(&["check"]);

        let json = json_run.assert_exit(1).json();
        let _ = text_run.assert_exit(1);
        assert_eq!(
            source_missing_count(&json),
            1,
            "{way} `from({value})`: {json}"
        );
        assert!(
            text_run.stdout.contains(says),
            "{way} `from({value})` should say `{says}`:\n{}",
            text_run.stdout
        );
        for run in [&json_run, &text_run] {
            assert!(
                !run.stdout.contains(MARKER) && !run.stderr.contains(MARKER),
                "{way} `from({value})` showed the outside file\nstdout:\n{}\nstderr:\n{}",
                run.stdout,
                run.stderr
            );
        }
    }
}

/// A doc whose `from(/src/lib.rs#add)` block no longer matches its source.
const STALE_DOC: &str = "# Blocks\n\n```rust from(/src/lib.rs#add)\nfn add() {}\n```\n";

#[test]
fn fix_never_writes_a_doc_under_a_symlinked_dir() {
    let outside = TempDir::new().expect("an outside directory should be creatable");
    fs::write(outside.path().join("blocks.md"), STALE_DOC)
        .expect("the outside doc should be writable");
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write("docs/blocks.md", STALE_DOC);
    symlink(outside.path(), project.path("docs/linked"))
        .expect("the directory symlink should be creatable");

    let run = project.supdoc(&["fix", "--yes"]);

    let _ = run.assert_exit(0);
    assert_ne!(
        project.read("docs/blocks.md"),
        STALE_DOC,
        "the in-root copy of the stale doc should be fixed"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("blocks.md"))
            .expect("the outside doc should be readable"),
        STALE_DOC,
        "the doc behind the symlinked directory should be byte-identical"
    );
}

#[test]
fn update_never_records_or_writes_beneath_a_symlinked_dir() {
    let outside = TempDir::new().expect("an outside directory should be creatable");
    fs::write(
        outside.path().join("notes.md"),
        "# Notes\n\nThe [`add`](/src/lib.rs#add) function.\n",
    )
    .expect("the outside doc should be writable");
    let before = tree(outside.path());
    let project = Project::fixture("basic");
    symlink(outside.path(), project.path("docs/linked"))
        .expect("the directory symlink should be creatable");

    let run = project.supdoc(&["update", "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        !project.read("supdoc.lock").contains("docs/linked"),
        "no link behind the symlinked directory should be recorded:\n{}",
        project.read("supdoc.lock")
    );
    assert_eq!(
        tree(outside.path()),
        before,
        "nothing should be written beneath the symlinked directory"
    );
}

/// Every path beneath `dir`, relative to it, without following symlinks.
fn tree(dir: &Path) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).expect("the scratch directory should be listable") {
            let path = entry.expect("the scratch entry should be readable").path();
            let kind = fs::symlink_metadata(&path).expect("the scratch entry should be statable");
            if kind.is_dir() {
                pending.push(path.clone());
            }
            let _ = found.insert(
                path.strip_prefix(dir)
                    .expect("the entry should be beneath the scratch directory")
                    .to_path_buf(),
            );
        }
    }
    found
}

/// A scratch directory standing in for one environment variable's home: `real` (a plain
/// directory), `elsewhere/inner` (where `link` points) and `link`, so `real/..` and `link/..`
/// both hold a `..`.
struct HostileHome {
    /// Removed on drop.
    dir: TempDir,
}

impl HostileHome {
    fn new() -> Self {
        let dir = TempDir::new().expect("a scratch home should be creatable");
        fs::create_dir(dir.path().join("real")).expect("the plain directory should be creatable");
        fs::create_dir_all(dir.path().join("elsewhere/inner"))
            .expect("the link's destination should be creatable");
        symlink(dir.path().join("elsewhere/inner"), dir.path().join("link"))
            .expect("the link should be creatable");
        Self { dir }
    }

    /// The two hostile values: `real/..` and `link/..`.
    fn values(&self) -> [PathBuf; 2] {
        [
            self.dir.path().join("real").join(".."),
            self.dir.path().join("link").join(".."),
        ]
    }

    /// Every path in the scratch home, to show a run created nothing in it.
    fn tree(&self) -> BTreeSet<PathBuf> {
        tree(self.dir.path())
    }
}

/// The `basic` fixture with an empty config, so `trust` has a project root.
fn trust_project() -> Project {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "");
    project
}

/// Runs `supdoc trust` and `supdoc trust --revoke` with `variable` set to each hostile value of a
/// fresh [`HostileHome`], asserting each is refused as `cli-trust-store` (74) naming `variable`,
/// and that nothing is created in the scratch home.
fn assert_trust_refused(variable: &str) {
    let project = trust_project();
    let home = HostileHome::new();
    let before = home.tree();
    let names = format!("`{variable}` holds `..`");
    for value in home.values() {
        for args in [&["trust"][..], &["trust", "--revoke"][..]] {
            let run = project.supdoc_in_with(".", args, &[(variable, &value)]);

            let _ = run.assert_exit(74);
            assert!(
                run.stderr.contains("cli-trust-store") && run.stderr.contains(&names),
                "`supdoc {}` with {variable}={} should refuse the store naming the variable:\n{}",
                run.args,
                value.display(),
                run.stderr
            );
            assert_eq!(home.tree(), before, "`supdoc {}` created a file", run.args);
        }
    }
}

#[test]
fn trust_and_revoke_refuse_a_state_home_holding_dot_dot_and_create_nothing() {
    assert_trust_refused("XDG_STATE_HOME");
}

#[test]
fn trust_and_revoke_refuse_a_home_holding_dot_dot_and_create_nothing() {
    assert_trust_refused("HOME");
}

/// Runs `check` on a project whose clippy step needs trust, with `variable` set to each hostile
/// value of a fresh [`HostileHome`], asserting the run says the store wasn't read because
/// `variable` holds `..` and creates nothing in the scratch home. The result cache gets its own
/// clean `XDG_CACHE_HOME`, so only the trust store reads the hostile value.
fn assert_check_reads_no_trust(variable: &str) {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.rust]\n",
    );
    project.write("docs/blocks.md", "# Blocks\n\n```rust\nfn main() {}\n```\n");
    let stub = TempDir::new().expect("the stub directory should be creatable");
    let rustfmt = stub.path().join("rustfmt");
    fs::write(&rustfmt, "#!/bin/sh\nexec /bin/cat\n").expect("the stub should be writable");
    fs::set_permissions(&rustfmt, fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
    let cache = TempDir::new().expect("a clean cache directory should be creatable");
    let home = HostileHome::new();
    let before = home.tree();
    let says = format!("the trust store wasn't read, as `{variable}` holds `..`");
    for value in home.values() {
        let run = project.supdoc_in_with(
            ".",
            &["check"],
            &[
                ("PATH", stub.path()),
                ("XDG_CACHE_HOME", cache.path()),
                (variable, &value),
            ],
        );

        assert!(
            run.stdout.contains(&says),
            "`supdoc check` with {variable}={} should say `{says}`:\n{}",
            value.display(),
            run.stdout
        );
        assert_eq!(home.tree(), before, "`supdoc check` created a file");
    }
}

#[test]
fn check_reads_no_trust_through_a_state_home_holding_dot_dot_and_says_which_variable() {
    assert_check_reads_no_trust("XDG_STATE_HOME");
}

#[test]
fn check_reads_no_trust_through_a_home_holding_dot_dot_and_says_which_variable() {
    assert_check_reads_no_trust("HOME");
}
