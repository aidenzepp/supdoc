//! A help names a command or a value that works as written from the directory that printed it,
//! and advice that clears the error it follows.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use crate::support::{Project, Run};

/// The project at `fixture "basic"` with an empty config, so a run from `docs/` still finds its root.
fn project() -> Project {
    let project = Project::fixture("basic");
    write(&project, "supdoc.config.toml", "");
    project
}

/// Writes `text` to `relative` inside `project`, creating its directories.
fn write(project: &Project, relative: &str, text: &str) {
    let path = project.path(relative);
    fs::create_dir_all(path.parent().expect("a project path should have a parent"))
        .expect("the directory should be creatable");
    fs::write(path, text).expect("the file should be writable");
}

/// The absolute path `supdoc` prints for `relative` inside `project`: its temp directory resolved
/// through any symlink, as the current directory is.
fn printed(project: &Project, relative: &str) -> PathBuf {
    fs::canonicalize(project.path("."))
        .expect("the project root should resolve")
        .join(relative)
}

/// The text of `run`'s output from the first `marker` to the next blank line.
fn block(run: &Run, marker: &str) -> String {
    let output = format!("{}{}", run.stdout, run.stderr);
    let start = output
        .find(marker)
        .unwrap_or_else(|| panic!("the output should have `{marker}`:\n{output}"));
    let rest = &output[start..];
    rest.split("\n\n").next().unwrap_or(rest).to_owned()
}

#[test]
fn init_help_keeps_the_config_flag_when_the_directory_is_missing() {
    let project = project();

    let run = project.supdoc(&["-c", "nope/x.toml", "init"]);

    let _ = run.assert_exit(74);
    assert!(
        run.stderr
            .contains("run `supdoc init -c nope/x.toml` again"),
        "the help should write the same file the error names:\n{}",
        run.stderr
    );
}

#[test]
fn init_help_keeps_the_config_flag_when_the_file_exists() {
    let project = project();
    write(&project, "sub/x.toml", "");

    let run = project.supdoc(&["-c", "sub/x.toml", "init"]);

    let _ = run.assert_exit(1);
    assert!(
        run.stderr.contains("run `supdoc init -c sub/x.toml`"),
        "the help should write the same file the error names:\n{}",
        run.stderr
    );
}

#[test]
fn missing_path_help_lists_the_nearest_directory_that_exists() {
    let project = project();
    write(&project, "my dir/keep.md", "");

    let nothing = project.supdoc(&["check", "nope/x.md"]);
    let deeper = project.supdoc(&["check", "docs/nope/x.md"]);
    let spaced = project.supdoc(&["check", "my dir/x.md"]);

    for (run, listing) in [
        (&nothing, "`ls .`"),
        (&deeper, "`ls docs`"),
        (&spaced, "`ls 'my dir'`"),
    ] {
        let _ = run.assert_exit(2);
        assert!(
            run.stderr.contains(listing),
            "`supdoc {}` should help with {listing}:\n{}",
            run.args,
            run.stderr
        );
    }
}

#[test]
fn unreadable_lock_help_names_the_lock_by_its_full_path() {
    let project = project();
    project.record();
    let lock = project.path("supdoc.lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o000))
        .expect("the lock should accept new permissions");

    let run = project.supdoc_in("docs", &["check"]);

    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644))
        .expect("the lock should accept its permissions back");
    let _ = run.assert_exit(74);
    let chmod = format!("`chmod u+r {}`", printed(&project, "supdoc.lock").display());
    assert!(
        run.stderr.contains(&chmod),
        "from docs/ the help should name {chmod}:\n{}",
        run.stderr
    );
}

#[test]
fn lock_symlink_help_names_the_symlink_by_its_full_path() {
    let project = project();
    write(&project, "base.lock", "");
    symlink("base.lock", project.path("supdoc.lock")).expect("the symlink should be creatable");

    let run = project.supdoc_in("docs", &["check"]);

    let _ = run.assert_exit(78);
    let rm = format!("`rm {}`", printed(&project, "supdoc.lock").display());
    assert!(
        run.stderr.contains(&rm),
        "from docs/ the help should name {rm}:\n{}",
        run.stderr
    );
}

#[test]
fn config_symlink_help_names_the_config_and_keeps_it_for_init() {
    let project = project();
    fs::remove_file(project.path("supdoc.config.toml")).expect("the config should be removable");
    write(&project, "base.toml", "");
    symlink("base.toml", project.path("supdoc.config.toml"))
        .expect("the symlink should be creatable");

    let run = project.supdoc_in("docs", &["check"]);

    let _ = run.assert_exit(78);
    let config = printed(&project, "supdoc.config.toml");
    let replace = format!("`rm {0} && supdoc -c {0} init`", config.display());
    assert!(
        run.stderr.contains(&replace),
        "from docs/ the help should name {replace}:\n{}",
        run.stderr
    );
}

#[test]
fn update_points_at_fix_only_for_a_link_fix_can_repair() {
    let project = project();
    write(
        &project,
        "docs/typo.md",
        "See [t](../src/lib.rs#ad) here.\n",
    );
    write(&project, "docs/gone.md", "See [g](../src/gone.rs) here.\n");

    let run = project.supdoc(&["update"]);

    assert!(
        block(&run, "--> docs/typo.md").contains("`supdoc fix docs/typo.md --yes`"),
        "fix repairs a misspelled symbol:\n{}",
        run.stderr
    );
    let gone = block(&run, "--> docs/gone.md");
    assert!(
        !gone.contains("supdoc fix"),
        "fix repairs nothing for a missing file:\n{gone}"
    );
    assert!(
        gone.contains("can't record a broken link"),
        "the help should still say update skips it:\n{gone}"
    );
}

#[test]
fn config_failure_help_repeats_the_command_that_failed() {
    let project = project();
    write(&project, "supdoc.config.toml", "include = \n");

    let resolve = project.supdoc(&["resolve", "src/lib.rs"]);
    let update = project.supdoc(&["update", "--yes"]);

    let _ = resolve.assert_exit(78);
    assert!(
        resolve
            .stderr
            .contains("run `supdoc resolve src/lib.rs` again"),
        "resolve needs its FILE:\n{}",
        resolve.stderr
    );
    let _ = update.assert_exit(78);
    assert!(
        update.stderr.contains("run `supdoc update --yes` again"),
        "update should keep --yes:\n{}",
        update.stderr
    );
}

#[test]
fn syntax_error_help_repeats_the_resolve_with_its_file() {
    let project = project();
    write(
        &project,
        "src/bad.rs",
        "pub fn fine() {}\npub fn broken( {\n",
    );

    let run = project.supdoc(&["resolve", "src/bad.rs"]);

    let warning = block(&run, "warning[read-syntax-errors]");
    assert!(
        warning.contains("run `supdoc resolve src/bad.rs` again"),
        "resolve needs its FILE:\n{warning}"
    );
}

#[test]
fn non_utf8_target_help_does_not_offer_exclude() {
    let project = project();
    fs::write(project.path("src/bad.rs"), [0xff, 0xfe, b'\n'])
        .expect("the target should be writable");
    write(&project, "docs/b.md", "[b](../src/bad.rs)\n");

    let run = project.supdoc(&["check"]);

    let target = block(&run, "error[read-not-utf8]");
    assert!(
        !target.contains("exclude"),
        "excluding a link's target leaves the error in place:\n{target}"
    );
    let source = block(&run, "warning[read-not-utf8]");
    assert!(
        source.contains("exclude"),
        "excluding a scanned file clears its warning:\n{source}"
    );
}

#[test]
fn lock_without_hash_features_help_names_the_table_and_the_recovery() {
    let project = project();
    project.record();
    let lock = project.read("supdoc.lock");
    let kept: Vec<&str> = lock
        .lines()
        .filter(|line| !line.starts_with("[features") && !line.starts_with("line-endings"))
        .collect();
    write(&project, "supdoc.lock", &kept.join("\n"));

    let run = project.supdoc(&["check"]);

    let _ = run.assert_exit(78);
    for needle in ["`[features.hash]`", "`supdoc update --yes`"] {
        assert!(
            run.stderr.contains(needle),
            "the help should name {needle}:\n{}",
            run.stderr
        );
    }
}

#[test]
fn rename_config_failure_help_repeats_the_whole_command() {
    let project = project();
    write(&project, "supdoc.config.toml", "include = \n");

    let run = project.supdoc(&["namespace", "rename", "old", "new", "--yes"]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr
            .contains("run `supdoc namespace rename old new --yes` again"),
        "rename needs OLD, NEW and --yes:\n{}",
        run.stderr
    );
}

#[test]
fn resolve_non_utf8_file_help_does_not_offer_exclude() {
    let project = project();
    fs::write(project.path("src/bad.rs"), [0xff, 0xfe, b'\n'])
        .expect("the file should be writable");

    let run = project.supdoc(&["resolve", "src/bad.rs"]);

    let _ = run.assert_exit(1);
    assert!(
        !run.stderr.contains("exclude"),
        "excluding a file leaves resolve reading it:\n{}",
        run.stderr
    );
}
