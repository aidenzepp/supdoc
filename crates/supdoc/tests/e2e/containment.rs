//! supdoc stays inside the project root, even when an untrusted checkout controls the docs, the
//! config, symlinks and the lockfile: nothing outside is read, hashed, named or printed.
//!
//! The `monorepo` fixture's `services/web` is the project, so the fixture's own root is outside.

use std::fs;

use crate::support::{Project, array_at, links};

/// A secret a CI runner might hold outside the checkout.
const SECRET: &str = "API_KEY = \"s3cr3t\"\nDB_PASSWORD = \"hunter2\"\n";

/// Fails if either stream of `run` shows any part of [`SECRET`].
fn assert_no_secret(run: &crate::support::Run) {
    for leak in ["s3cr3t", "hunter2", "API_KEY = ", "DB_PASSWORD = "] {
        assert!(
            !run.stdout.contains(leak) && !run.stderr.contains(leak),
            "`supdoc {}` showed `{leak}`\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

/// The `monorepo` fixture with `services/web` as a standalone project and [`SECRET`] outside it.
fn web_project() -> Project {
    let project = Project::fixture("monorepo");
    fs::write(
        project.path("services/web/supdoc.config.toml"),
        "include = [\"docs\"]\n",
    )
    .expect("the config should be writable");
    fs::write(project.path("settings.py"), SECRET).expect("the secret should be writable");
    fs::write(project.path(".env"), SECRET).expect("the secret should be writable");
    project
}

/// Appends `text` to the web project's guide.
fn add_to_guide(project: &Project, text: &str) {
    let guide = project.path("services/web/docs/guide.md");
    let before = fs::read_to_string(&guide).expect("the guide should be readable");
    fs::write(&guide, format!("{before}\n{text}\n")).expect("the guide should be writable");
}

#[test]
fn extends_naming_a_non_config_file_is_refused_without_printing_it() {
    let project = web_project();
    fs::write(
        project.path("services/web/supdoc.config.toml"),
        "extends = \"../../.env\"\n",
    )
    .expect("the config should be writable");

    let run = project.supdoc_in("services/web", &["check"]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains("config-extends-not-config"),
        "{}",
        run.stderr
    );
    assert_no_secret(&run);
}

#[test]
fn links_climbing_out_of_the_root_are_not_read_or_recorded() {
    let project = web_project();
    add_to_guide(
        &project,
        "[a](../../../settings.py#API_KEY) [b](/../../settings.py#API_KEY) \
         [c](../../../settings.py#API_KEX) [d](../../../settings.py)",
    );

    let update = project.supdoc_in("services/web", &["update", "--yes"]);
    let check = project.supdoc_in("services/web", &["--json", "check"]);

    assert_no_secret(&update);
    assert_no_secret(&check);
    assert_eq!(
        project.locked_links("services/web/supdoc.lock"),
        links(&["src/lib.rs#handler"]),
        "only the in-root link is hashed"
    );
    let outside = array_at(&check.json(), "/findings")
        .iter()
        .filter(|finding| finding["kind"] == "link-outside-root")
        .count();
    assert_eq!(outside, 4, "{}", check.stdout);
}

#[cfg(unix)]
#[test]
fn symlink_out_of_the_root_is_not_read_or_recorded() {
    let project = web_project();
    std::os::unix::fs::symlink(
        project.path("settings.py"),
        project.path("services/web/docs/pw.py"),
    )
    .expect("the symlink should be creatable");
    add_to_guide(&project, "[a](pw.py#API_KEY) [b](pw.py#API_KEX) [c](pw.py)");

    let update = project.supdoc_in("services/web", &["update", "--yes"]);
    let check = project.supdoc_in("services/web", &["--json", "check"]);

    assert_no_secret(&update);
    assert_no_secret(&check);
    assert_eq!(
        project.locked_links("services/web/supdoc.lock"),
        links(&["src/lib.rs#handler"]),
        "nothing behind the symlink is hashed"
    );
    assert!(
        check.stdout.contains("link-outside-root"),
        "{}",
        check.stdout
    );
}

#[cfg(unix)]
#[test]
fn symlink_named_like_python_to_a_dotenv_file_is_not_parsed_as_python() {
    let project = web_project();
    fs::write(project.path("services/web/.env"), SECRET).expect("the secret should be writable");
    std::os::unix::fs::symlink(
        project.path("services/web/.env"),
        project.path("services/web/docs/env.py"),
    )
    .expect("the symlink should be creatable");
    add_to_guide(&project, "[a](env.py#DB_PASSWORD)");

    let update = project.supdoc_in("services/web", &["update", "--yes"]);

    assert_no_secret(&update);
    assert_eq!(
        project.locked_links("services/web/supdoc.lock"),
        links(&["src/lib.rs#handler"]),
        "the .env file is not read as Python"
    );
}

#[test]
fn a_child_config_cannot_inherit_a_namespace_outside_its_root() {
    let project = Project::fixture("monorepo");

    let run = project.supdoc_in("services/web", &["update", "--yes"]);

    let _ = run.assert_exit(78);
    assert!(
        run.stderr.contains("config-namespace-outside-root") && run.stderr.contains("`shared`"),
        "{}",
        run.stderr
    );
}

#[test]
fn lockfile_target_climbing_out_of_the_root_is_a_parse_error() {
    let project = web_project();
    fs::write(
        project.path("services/web/supdoc.lock"),
        "# supdoc lockfile\nversion = 1\n\n[features.hash]\nline-endings = \"normalize\"\n\n[[link]]\nsource = \"docs/guide.md\"\n\
         target = \"../../settings.py\"\nhash = \"9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b9f2c8e4b\"\n",
    )
    .expect("the lockfile should be writable");

    let run = project.supdoc_in("services/web", &["check"]);

    assert_ne!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stderr.contains("lock-parse"), "{}", run.stderr);
    assert!(
        run.stderr
            .contains("target `../../settings.py` climbs out of the project root"),
        "{}",
        run.stderr
    );
}
