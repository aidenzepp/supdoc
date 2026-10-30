//! A `[PATH]` or `FILE` that can't be looked up is reported as unreadable, never as missing.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use crate::support::{Project, Run};

/// The string at `key` in the error object `run` printed on stderr.
fn error_field(run: &Run, key: &str) -> String {
    error_of(run)
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("the error should have a `{key}` string: {}", run.stderr))
        .to_owned()
}

/// The error object a failed `--json` run prints on stderr.
fn error_of(run: &Run) -> serde_json::Value {
    serde_json::from_str(&run.stderr).unwrap_or_else(|error| {
        panic!(
            "`supdoc {}` should print one JSON error on stderr ({error}):\n{}",
            run.args, run.stderr
        )
    })
}

/// A project whose `docs/private` directory can't be searched, or `None` when the test runs as a
/// user the mode doesn't restrict (root), where the scenario can't happen.
fn project_with_locked_directory() -> Option<Project> {
    let project = Project::fixture("basic");
    fs::create_dir(project.path("docs/private")).expect("the directory should be creatable");
    fs::write(project.path("docs/private/c.md"), "# C\n").expect("the file should be writable");
    lock(&project, 0o000);
    if fs::symlink_metadata(project.path("docs/private/c.md")).is_ok() {
        unlock(&project);
        return None;
    }
    Some(project)
}

/// Sets the mode of the project's `docs/private` directory.
fn lock(project: &Project, mode: u32) {
    fs::set_permissions(
        project.path("docs/private"),
        fs::Permissions::from_mode(mode),
    )
    .expect("the directory's permissions should be settable");
}

/// Lets the temporary directory be removed again.
fn unlock(project: &Project) {
    lock(project, 0o755);
}

#[test]
fn a_path_under_an_unreadable_directory_is_unreadable_not_missing() {
    let Some(project) = project_with_locked_directory() else {
        return;
    };

    let runs = [
        project.supdoc(&["--json", "check", "docs/private/c.md"]),
        project.supdoc(&["--json", "update", "docs/private/c.md"]),
        project.supdoc(&["--json", "fix", "docs/private/c.md"]),
        project.supdoc(&["--json", "resolve", "docs/private/c.md"]),
    ];
    unlock(&project);

    for run in runs {
        let _ = run.assert_exit(74);
        let message = error_field(&run, "message");
        assert_eq!(
            error_field(&run, "kind"),
            "cli-path-unreadable",
            "{message}"
        );
        assert!(
            message.contains("docs/private/c.md") && !message.contains("does not exist"),
            "`supdoc {}` should name the path and not call it missing: {message}",
            run.args
        );
    }
}

#[test]
fn a_missing_path_or_one_under_a_file_stays_missing() {
    let project = Project::fixture("basic");

    for path in ["docs/nope.md", "docs/guide.md/x"] {
        for command in ["check", "resolve"] {
            let run = project.supdoc(&["--json", command, path]);
            let _ = run.assert_exit(2);
            assert_eq!(
                error_field(&run, "kind"),
                "cli-path-missing",
                "`supdoc {}`",
                run.args
            );
        }
    }
}
