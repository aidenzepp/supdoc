//! supdoc stays inside the project root, even when an untrusted checkout controls the docs, the
//! config, symlinks and the lockfile: nothing outside is read, hashed, named or printed.
//!
//! The `monorepo` fixture's `services/web` is the project, so the fixture's own root is outside.

use std::fs;

use crate::support::{Project, array_at, at, links};

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

/// Every way a run can print an error: text, JSON and GitHub annotations.
const ERROR_FORMATS: [&[&str]; 3] = [
    &["check"],
    &["--json", "check"],
    &["--output", "github", "check"],
];

/// Fails if either stream of `run` names a key of [`SECRET`], as a parse error quoting it would.
fn assert_no_secret_key(run: &crate::support::Run) {
    assert_no_secret(run);
    for leak in ["API_KEY", "DB_PASSWORD"] {
        assert!(
            !run.stdout.contains(leak) && !run.stderr.contains(leak),
            "`supdoc {}` named `{leak}`\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

#[cfg(unix)]
#[test]
fn a_lockfile_symlinked_out_of_the_root_is_refused_alike_whether_or_not_its_target_exists() {
    let project = web_project();
    let lock = project.path("services/web/supdoc.lock");
    std::os::unix::fs::symlink(project.path(".env"), &lock)
        .expect("the symlink should be creatable");
    let present: Vec<_> = ERROR_FORMATS
        .iter()
        .map(|args| project.supdoc_in("services/web", args))
        .collect();
    fs::remove_file(&lock).expect("the symlink should be removable");
    std::os::unix::fs::symlink(project.path("no-such-file"), &lock)
        .expect("the dangling symlink should be creatable");

    for (with_target, args) in present.iter().zip(ERROR_FORMATS) {
        let without_target = project.supdoc_in("services/web", args);

        let _ = with_target.assert_exit(78);
        assert_no_secret_key(with_target);
        assert!(
            format!("{}{}", with_target.stdout, with_target.stderr).contains("lock-outside-root"),
            "`supdoc {}` should refuse the lockfile\nstdout:\n{}\nstderr:\n{}",
            with_target.args,
            with_target.stdout,
            with_target.stderr
        );
        assert_eq!(
            (
                &without_target.code,
                &without_target.stdout,
                &without_target.stderr
            ),
            (&with_target.code, &with_target.stdout, &with_target.stderr),
            "`supdoc {}` must not reveal whether the lockfile's target exists",
            with_target.args
        );
    }
}

#[cfg(unix)]
#[test]
fn a_found_config_symlinked_to_an_outside_dotenv_file_is_refused_without_printing_it() {
    let project = web_project();
    let config = project.path("services/web/supdoc.config.toml");
    fs::remove_file(&config).expect("the config should be removable");
    std::os::unix::fs::symlink(project.path(".env"), &config)
        .expect("the symlink should be creatable");

    for args in ERROR_FORMATS {
        let run = project.supdoc_in("services/web", args);

        let _ = run.assert_exit(78);
        assert_no_secret_key(&run);
        assert!(
            format!("{}{}", run.stdout, run.stderr).contains("config-symlink-not-config"),
            "`supdoc {}` should refuse the config\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

#[cfg(unix)]
#[test]
fn a_found_config_symlinked_out_of_the_root_is_refused_alike_whatever_its_target_is() {
    let project = web_project();
    fs::create_dir(project.path("outside-dir")).expect("the outside directory should be creatable");
    let config = project.path("services/web/supdoc.config.toml");
    let link_to = |target: &str| -> Vec<_> {
        // Renaming a fresh link over the config swaps the target without deleting anything.
        let staged = project.path("services/web/staged-link");
        std::os::unix::fs::symlink(project.path(target), &staged)
            .expect("the symlink should be creatable");
        fs::rename(&staged, &config).expect("the symlink should replace the config");
        ERROR_FORMATS
            .iter()
            .map(|args| project.supdoc_in("services/web", args))
            .collect()
    };
    let to_file = link_to(".env");
    let to_directory = link_to("outside-dir");
    let to_nothing = link_to("no-such-file");

    for ((file_run, directory_run), missing_run) in
        to_file.iter().zip(&to_directory).zip(&to_nothing)
    {
        let _ = file_run.assert_exit(78);
        assert_no_secret(file_run);
        assert!(
            format!("{}{}", file_run.stdout, file_run.stderr).contains("config-symlink-not-config"),
            "`supdoc {}` should refuse the config\nstdout:\n{}\nstderr:\n{}",
            file_run.args,
            file_run.stdout,
            file_run.stderr
        );
        for (target, other) in [("a directory", directory_run), ("nothing", missing_run)] {
            assert_eq!(
                (&other.code, &other.stdout, &other.stderr),
                (&file_run.code, &file_run.stdout, &file_run.stderr),
                "`supdoc {}` with the config linked to {target} must match the link to `.env`: \
                 the result must not reveal what the link's target is",
                file_run.args
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn a_found_config_symlinked_to_another_config_file_still_loads() {
    let project = web_project();
    fs::create_dir(project.path("shared")).expect("the shared directory should be creatable");
    fs::write(
        project.path("shared/supdoc.config.toml"),
        "include = [\"docs/guide.md\"]\n",
    )
    .expect("the shared config should be writable");
    let config = project.path("services/web/supdoc.config.toml");
    fs::remove_file(&config).expect("the config should be removable");
    std::os::unix::fs::symlink(project.path("shared/supdoc.config.toml"), &config)
        .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["config", "show"]);

    let _ = run.assert_exit(0);
    assert!(run.stdout.contains("docs/guide.md"), "{}", run.stdout);
}

#[test]
fn the_config_flag_still_loads_a_file_of_any_name() {
    let project = web_project();
    fs::write(
        project.path("services/web/ci.toml"),
        "include = [\"docs\"]\n",
    )
    .expect("the flag's config should be writable");

    let run = project.supdoc_in("services/web", &["--config", "ci.toml", "config", "show"]);

    let _ = run.assert_exit(0);
    assert!(run.stdout.contains("ci.toml"), "{}", run.stdout);
}

/// Fails unless every format refuses the web project's lockfile as `lock-outside-root` without
/// printing any key or value of [`SECRET`] or `leak`.
fn assert_lockfile_refused_quietly(project: &Project, leak: &str) {
    for args in ERROR_FORMATS {
        let run = project.supdoc_in("services/web", args);

        let _ = run.assert_exit(78);
        assert_no_secret_key(&run);
        let shown = format!("{}{}", run.stdout, run.stderr);
        assert!(
            shown.contains("lock-outside-root") && !shown.contains(leak),
            "`supdoc {}` should refuse the lockfile without showing `{leak}`\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

#[cfg(unix)]
#[test]
fn a_lockfile_symlinked_to_an_in_root_dotenv_file_is_refused_without_printing_it() {
    let project = web_project();
    fs::write(project.path("services/web/.env"), "API_KEY=\"s3cr3t\"\n")
        .expect("the secret should be writable");
    std::os::unix::fs::symlink(".env", project.path("services/web/supdoc.lock"))
        .expect("the symlink should be creatable");

    assert_lockfile_refused_quietly(&project, "API_KEY");
}

#[cfg(unix)]
#[test]
fn a_lockfile_symlinked_to_an_in_root_file_with_a_lock_key_is_refused_without_printing_its_value() {
    let project = web_project();
    fs::write(
        project.path("services/web/token.toml"),
        "version = \"s3cr3t-token\"\n",
    )
    .expect("the secret should be writable");
    std::os::unix::fs::symlink("token.toml", project.path("services/web/supdoc.lock"))
        .expect("the symlink should be creatable");

    assert_lockfile_refused_quietly(&project, "s3cr3t-token");
}

#[cfg(unix)]
#[test]
fn a_lockfile_symlinked_to_an_in_root_supdoc_lock_still_loads() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    fs::create_dir(project.path("services/web/kept")).expect("the directory should be creatable");
    fs::rename(
        project.path("services/web/supdoc.lock"),
        project.path("services/web/kept/supdoc.lock"),
    )
    .expect("the lockfile should be movable");
    std::os::unix::fs::symlink("kept/supdoc.lock", project.path("services/web/supdoc.lock"))
        .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["--json", "check"]);

    assert_eq!(
        at(&run.json(), "/data/summary/passed"),
        1,
        "the recorded link should pass against the lockfile behind the symlink\n{}",
        run.stdout
    );
}

/// The web project's `check --json` with `docs/x/supdoc.config.toml` symlinked to `target`
/// (relative to the fixture root) and a broken link beside it.
#[cfg(unix)]
fn check_with_nested_config_symlinked_to(target: &str) -> crate::support::Run {
    let project = web_project();
    fs::create_dir(project.path("services/web/docs/x")).expect("the directory should be creatable");
    fs::write(
        project.path("services/web/docs/x/p.md"),
        "# P\n\n[gone](../../src/missing.rs)\n",
    )
    .expect("the doc should be writable");
    std::os::unix::fs::symlink(
        project.path(target),
        project.path("services/web/docs/x/supdoc.config.toml"),
    )
    .expect("the symlink should be creatable");
    project.supdoc_in("services/web", &["--json", "check"])
}

#[cfg(unix)]
#[test]
fn a_nested_config_symlinked_out_of_the_root_scans_alike_whether_or_not_its_target_exists() {
    let present = check_with_nested_config_symlinked_to("settings.py");
    let absent = check_with_nested_config_symlinked_to("no-such-file");

    assert_no_secret(&present);
    assert!(
        !present.stdout.contains("missing.rs"),
        "the directory with its own config is a separate project\n{}",
        present.stdout
    );
    let report = |run: &crate::support::Run| {
        let mut json = run.json();
        // Each run has its own temporary root; only what the scan found is compared.
        if let Some(root) = json.get_mut("root") {
            *root = serde_json::Value::Null;
        }
        (run.code, json, run.stderr.clone())
    };
    assert_eq!(
        report(&absent),
        report(&present),
        "the scan must not reveal whether the nested config's target exists"
    );
}

#[cfg(unix)]
#[test]
fn skill_install_refuses_an_agents_directory_symlinked_out_of_the_root() {
    let project = web_project();
    fs::create_dir(project.path("elsewhere")).expect("the outside directory should be creatable");
    std::os::unix::fs::symlink("../../elsewhere", project.path("services/web/.agents"))
        .expect("the symlink should be creatable");
    let landing = fs::canonicalize(project.path("elsewhere"))
        .expect("the outside directory should resolve")
        .join("skills/supdoc/SKILL.md");

    for args in [&["skill", "install"][..], &["skill", "install", "--yes"]] {
        let run = project.supdoc_in("services/web", args);

        let _ = run.assert_exit(1);
        assert!(
            run.stderr.contains("cli-skill-outside-root")
                && run.stderr.contains(&landing.display().to_string()),
            "`supdoc {}` should refuse and name where the skill would land\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
    let written: Vec<_> = fs::read_dir(project.path("elsewhere"))
        .expect("the outside directory should be readable")
        .collect();
    assert!(
        written.is_empty(),
        "nothing should be written outside the root: {written:?}"
    );
    assert!(
        !project.path("services/web/.claude").exists(),
        "a refusal should write neither copy"
    );
}
