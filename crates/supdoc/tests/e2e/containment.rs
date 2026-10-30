//! supdoc stays inside the project root, even when an untrusted checkout controls the docs, the
//! config, symlinks and the lockfile: no outside file is read, hashed, named or printed, other
//! than supdoc's own config and ignore inputs.
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

/// Writes `extends` into the web project's config, points `services/web/x/supdoc.config.toml` at
/// an outside file, an outside directory and nothing in turn, and fails unless every format
/// refuses all three alike as `config-extends-not-config`.
#[cfg(unix)]
fn assert_extends_link_refused_alike(project: &Project, extends: &str) {
    fs::create_dir(project.path("outside-dir")).expect("the outside directory should be creatable");
    fs::write(
        project.path("services/web/supdoc.config.toml"),
        format!("extends = \"{extends}\"\n"),
    )
    .expect("the config should be writable");
    fs::create_dir(project.path("services/web/x")).expect("the link directory should be creatable");
    let link = project.path("services/web/x/supdoc.config.toml");
    let link_to = |target: &str| -> Vec<_> {
        // Renaming a fresh link over the old one swaps the target without deleting anything.
        let staged = project.path("services/web/x/staged-link");
        std::os::unix::fs::symlink(project.path(target), &staged)
            .expect("the symlink should be creatable");
        fs::rename(&staged, &link).expect("the symlink should replace the last one");
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
            format!("{}{}", file_run.stdout, file_run.stderr).contains("config-extends-not-config"),
            "`supdoc {}` with `extends = \"{extends}\"` should refuse it\nstdout:\n{}\nstderr:\n{}",
            file_run.args,
            file_run.stdout,
            file_run.stderr
        );
        for (target, other) in [("a directory", directory_run), ("nothing", missing_run)] {
            assert_eq!(
                (&other.code, &other.stdout, &other.stderr),
                (&file_run.code, &file_run.stdout, &file_run.stderr),
                "`supdoc {}` with `extends = \"{extends}\"` linked to {target} must match the \
                 link to `.env`: the result must not reveal what the link's target is",
                file_run.args
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn an_extends_symlinked_out_of_the_root_is_refused_alike_whatever_its_target_is() {
    assert_extends_link_refused_alike(&web_project(), "x/supdoc.config.toml");
}

#[cfg(unix)]
#[test]
fn an_extends_through_an_outside_alias_of_the_root_is_refused_alike_whatever_its_target_is() {
    let project = web_project();
    // A link outside the root that leads back into it spells an in-root path from outside.
    std::os::unix::fs::symlink(
        project.path("services/web"),
        project.path("services/alias-to-web"),
    )
    .expect("the symlink should be creatable");

    assert_extends_link_refused_alike(&project, "../alias-to-web/x/supdoc.config.toml");
}

#[cfg(unix)]
#[test]
fn an_extends_symlinked_to_another_config_file_still_loads() {
    let project = web_project();
    fs::create_dir(project.path("shared")).expect("the shared directory should be creatable");
    fs::write(
        project.path("shared/supdoc.config.toml"),
        "include = [\"docs/guide.md\"]\n",
    )
    .expect("the shared config should be writable");
    fs::write(
        project.path("services/web/supdoc.config.toml"),
        "extends = \"x/supdoc.config.toml\"\n",
    )
    .expect("the config should be writable");
    fs::create_dir(project.path("services/web/x")).expect("the link directory should be creatable");
    std::os::unix::fs::symlink(
        project.path("shared/supdoc.config.toml"),
        project.path("services/web/x/supdoc.config.toml"),
    )
    .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["check"]);

    let _ = run.assert_exit(0);
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

/// Asserts that `supdoc skill install <args>` run in `services/web` refused with `code` and
/// `exit`, and left no skill copy under the project or the directory `-c` pointed at.
fn assert_skill_install_refused(project: &Project, args: &[&str], code: &str, exit: i32) {
    let run = project.supdoc_in("services/web", args);

    let _ = run.assert_exit(exit);
    assert!(
        run.stderr.contains(code),
        "`supdoc {}` should fail with {code}\nstdout:\n{}\nstderr:\n{}",
        run.args,
        run.stdout,
        run.stderr
    );
    for created in [".agents", ".claude"] {
        assert!(
            !project.path("services/web").join(created).exists() && !project.path(created).exists(),
            "a refused `supdoc {}` should create no {created}",
            run.args
        );
    }
}

#[test]
fn skill_install_refuses_a_config_that_does_not_exist() {
    let project = web_project();
    fs::create_dir(project.path("elsewhere")).expect("the directory should be creatable");

    for args in [
        &[
            "skill",
            "install",
            "-c",
            "../../elsewhere/supdoc.config.toml",
        ][..],
        &[
            "skill",
            "install",
            "--yes",
            "-c",
            "../../elsewhere/supdoc.config.toml",
        ],
    ] {
        assert_skill_install_refused(&project, args, "config-not-found", 78);
    }
    assert!(
        !project.path("elsewhere/.agents").exists(),
        "nothing should be created beside the missing config"
    );
}

#[cfg(unix)]
#[test]
fn skill_install_refuses_a_config_symlink_that_dangles() {
    let project = web_project();
    fs::create_dir(project.path("elsewhere")).expect("the directory should be creatable");
    std::os::unix::fs::symlink("gone.toml", project.path("elsewhere/link.toml"))
        .expect("the symlink should be creatable");

    assert_skill_install_refused(
        &project,
        &[
            "skill",
            "install",
            "--yes",
            "-c",
            "../../elsewhere/link.toml",
        ],
        "config-not-found",
        78,
    );
    assert!(
        !project.path("elsewhere/.agents").exists(),
        "nothing should be created beside the dangling symlink"
    );
}

#[test]
fn skill_install_refuses_a_config_beneath_a_file() {
    let project = web_project();

    assert_skill_install_refused(
        &project,
        &[
            "skill",
            "install",
            "--yes",
            "-c",
            "../../settings.py/supdoc.config.toml",
        ],
        "config-not-found",
        78,
    );
}

#[test]
fn skill_install_refuses_a_config_that_is_a_directory() {
    let project = web_project();
    fs::create_dir(project.path("elsewhere")).expect("the directory should be creatable");

    assert_skill_install_refused(
        &project,
        &["skill", "install", "--yes", "-c", "../../elsewhere"],
        "config-read",
        74,
    );
    assert!(
        !project.path("elsewhere/.agents").exists(),
        "nothing should be created inside the directory"
    );
}

#[test]
fn skill_install_accepts_an_existing_file_of_any_name_as_the_config() {
    let project = web_project();
    fs::create_dir(project.path("elsewhere")).expect("the directory should be creatable");
    fs::write(project.path("elsewhere/anything.txt"), "not toml {{")
        .expect("the file should be writable");

    let run = project.supdoc_in(
        "services/web",
        &[
            "skill",
            "install",
            "--yes",
            "-c",
            "../../elsewhere/anything.txt",
        ],
    );

    let _ = run.assert_exit(0);
    assert!(
        project
            .path("elsewhere/.agents/skills/supdoc/SKILL.md")
            .exists(),
        "the skill should be installed beside the named file"
    );
}

#[cfg(unix)]
#[test]
fn a_named_path_beside_a_gitignore_symlinked_out_of_the_root_is_not_quoted() {
    let project = web_project();
    // `gui*.md` matches the guide; no path the run names contains it.
    fs::write(project.path("rules"), "gui*.md\n").expect("the outside rules should be writable");
    std::os::unix::fs::symlink(
        project.path("rules"),
        project.path("services/web/.gitignore"),
    )
    .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["check", "docs/guide.md"]);

    for stream in [&run.stdout, &run.stderr] {
        assert!(
            !stream.contains("gui*.md"),
            "`supdoc {}` printed a line from a file outside the root\nstdout:\n{}\nstderr:\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

/// The number of findings of `kind` in a `--json` run.
fn count_of(run: &crate::support::Run, kind: &str) -> usize {
    array_at(&run.json(), "/findings")
        .iter()
        .filter(|finding| finding["kind"].as_str() == Some(kind))
        .count()
}

/// The sources of every link a `--json list` run reports.
fn listed_sources(run: &crate::support::Run) -> Vec<String> {
    array_at(&run.json(), "/data/links")
        .iter()
        .map(|link| at(link, "/source").as_str().unwrap_or_default().to_owned())
        .collect()
}

#[cfg(unix)]
#[test]
fn a_directory_whose_gitignore_is_symlinked_out_is_checked_without_its_rules() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    // As in git, the symlink is treated as absent, so this rule hides nothing.
    fs::write(project.path("rules"), "gui*.md\n").expect("the outside rules should be writable");
    std::os::unix::fs::symlink(
        project.path("rules"),
        project.path("services/web/docs/.gitignore"),
    )
    .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["--json", "check"]);
    let list = project.supdoc_in("services/web", &["--json", "list"]);

    assert_eq!(
        count_of(&run, "scan-ignore-file-symlink"),
        1,
        "{}",
        run.stdout
    );
    assert_eq!(count_of(&run, "link-obsolete"), 0, "{}", run.stdout);
    assert!(
        listed_sources(&list).contains(&"docs/guide.md".to_owned()),
        "the guide is walked and its links counted\n{}",
        list.stdout
    );
    assert!(!run.stdout.contains("gui*.md"), "{}", run.stdout);
}

#[cfg(unix)]
#[test]
fn a_root_gitignore_symlinked_out_of_the_root_is_not_read_and_the_scan_goes_on() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    let locked = project.read("services/web/supdoc.lock");
    fs::write(project.path("rules"), "gui*.md\n").expect("the outside rules should be writable");
    std::os::unix::fs::symlink(
        project.path("rules"),
        project.path("services/web/.gitignore"),
    )
    .expect("the symlink should be creatable");

    let check = project.supdoc_in("services/web", &["check"]);
    let update = project.supdoc_in("services/web", &["update", "--yes"]);

    // The fixture's guide has a link into an undefined namespace, so `check` fails on that.
    let _ = check.assert_exit(1);
    let _ = update.assert_exit(0);
    for run in [&check, &update] {
        let shown = format!("{}{}", run.stdout, run.stderr);
        assert_eq!(
            shown.matches("scan-ignore-file-symlink").count(),
            1,
            "`supdoc {}` should warn once about the symlink\n{shown}",
            run.args
        );
        assert!(
            !shown.contains("gui*.md"),
            "`supdoc {}` printed a line from outside the root\n{shown}",
            run.args
        );
    }
    assert_eq!(
        project.read("services/web/supdoc.lock"),
        locked,
        "the guide is still walked, so its entries stay"
    );
}

#[cfg(unix)]
#[test]
fn a_package_whose_gitignore_links_to_the_monorepos_scans_with_one_warning() {
    let project = monorepo_with_git();
    // The monorepo's own `.gitignore` sits above the package root and still applies.
    fs::write(project.path(".gitignore"), "scr*.md\n").expect("the gitignore should be writable");
    std::os::unix::fs::symlink("../../.gitignore", project.path("services/web/.gitignore"))
        .expect("the symlink should be creatable");
    write(&project, "services/web/docs/scratch.md", SCRATCH);

    let run = project.supdoc_in("services/web", &["--json", "update", "--yes"]);
    let list = project.supdoc_in("services/web", &["--json", "list"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        count_of(&run, "scan-ignore-file-symlink"),
        1,
        "{}",
        run.stdout
    );
    let sources = listed_sources(&list);
    assert!(
        sources.contains(&"docs/guide.md".to_owned())
            && !sources.contains(&"docs/scratch.md".to_owned()),
        "the guide is counted, and the monorepo's rule above the root still hides scratch.md\n{}",
        list.stdout
    );
}

#[cfg(unix)]
#[test]
fn a_gitignore_symlinked_within_the_root_is_not_honoured() {
    let project = web_project();
    write(&project, "services/web/rules", "gui*.md\n");
    std::os::unix::fs::symlink("../rules", project.path("services/web/docs/.gitignore"))
        .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["--json", "update", "--yes"]);
    let list = project.supdoc_in("services/web", &["--json", "list"]);

    let _ = run.assert_exit(0);
    assert_eq!(
        count_of(&run, "scan-ignore-file-symlink"),
        1,
        "{}",
        run.stdout
    );
    assert!(
        listed_sources(&list).contains(&"docs/guide.md".to_owned()),
        "git doesn't honour the symlink, so neither does supdoc\n{}",
        list.stdout
    );
}

/// Writes `text` to `relative` in `project`, creating its directories.
fn write(project: &Project, relative: &str, text: &str) {
    let path = project.path(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the fixture directory should be creatable");
    }
    fs::write(path, text).expect("the fixture file should be writable");
}

/// Makes `relative` in `project` a git directory git accepts as a repository, without running
/// git: a `HEAD` naming a branch, and empty `objects` and `refs` directories.
fn make_git_dir(project: &Project, relative: &str) {
    write(
        project,
        &format!("{relative}/HEAD"),
        "ref: refs/heads/main\n",
    );
    for dir in ["objects", "refs"] {
        fs::create_dir_all(project.path(&format!("{relative}/{dir}")))
            .expect("the fixture git directory should be creatable");
    }
}

/// A doc with one link, for a file the tests hide with an ignore rule.
const SCRATCH: &str = "The [`handler`](../src/lib.rs#handler) function.\n";

#[test]
fn a_gitignore_whitelist_overrides_a_git_exclude_rule_and_keeps_the_lock_entry() {
    let project = web_project();
    write(&project, "services/web/docs/keep.md", SCRATCH);
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    let kept = "source = \"docs/keep.md\"";
    assert!(project.read("services/web/supdoc.lock").contains(kept));
    // As in git, the `.gitignore` whitelist outranks the exclude file's rule.
    make_git_dir(&project, "services/web/.git");
    write(&project, "services/web/.git/info/exclude", "docs/*.md\n");
    write(&project, "services/web/.gitignore", "!docs/keep.md\n");

    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);

    let locked = project.read("services/web/supdoc.lock");
    assert!(
        locked.contains(kept),
        "keep.md is walked, so its entry stays:\n{locked}"
    );
}

#[test]
fn a_git_exclude_whitelist_overrides_the_global_excludes_file() {
    let project = web_project();
    let home = tempfile::tempdir().expect("a home directory should be creatable");
    fs::create_dir_all(home.path().join(".config/git"))
        .expect("the git config directory should be creatable");
    fs::write(home.path().join(".config/git/ignore"), "docs/*.md\n")
        .expect("the global excludes file should be writable");
    write(&project, "services/web/docs/keep.md", SCRATCH);
    make_git_dir(&project, "services/web/.git");
    write(
        &project,
        "services/web/.git/info/exclude",
        "!docs/keep.md\n",
    );

    let run = project.supdoc_in_with(
        "services/web",
        &["--json", "list"],
        &[("HOME", home.path())],
    );

    let sources: Vec<_> = array_at(&run.json(), "/data/links")
        .iter()
        .map(|link| at(link, "/source").as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        sources.contains(&"docs/keep.md".to_owned()),
        "the exclude whitelist outranks the global rule, as in git\n{}\n{}",
        run.stdout,
        run.stderr
    );
}

/// Runs `supdoc update --yes` from `dir` with a global excludes file of `pattern` in an isolated
/// home, and returns the web project's lockfile.
fn lock_with_global_pattern(project: &Project, dir: &str, pattern: &str) -> String {
    let home = tempfile::tempdir().expect("a home directory should be creatable");
    fs::create_dir_all(home.path().join(".config/git"))
        .expect("the git config directory should be creatable");
    fs::write(
        home.path().join(".config/git/ignore"),
        format!("{pattern}\n"),
    )
    .expect("the global excludes file should be writable");
    write(project, "services/web/docs/keep.md", SCRATCH);

    let run = project.supdoc_in_with(dir, &["update", "--yes"], &[("HOME", home.path())]);

    let _ = run.assert_exit(0);
    lock_of_web(project)
}

#[test]
fn an_anchored_global_pattern_counts_from_the_work_tree_top_whatever_the_working_directory() {
    for dir in ["services/web", "services/web/docs"] {
        let project = monorepo_with_git();

        let locked = lock_with_global_pattern(&project, dir, "services/web/docs/gui*.md");

        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "git anchors a global pattern at the work tree top, so it hides the guide from `{dir}`:\n{locked}"
        );
    }
}

#[test]
fn an_anchored_global_pattern_counts_from_the_root_outside_a_repository() {
    for dir in ["services/web", "services/web/docs"] {
        let project = web_project();

        let locked = lock_with_global_pattern(&project, dir, "docs/gui*.md");

        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "with no repository the pattern counts from the root, so it hides the guide from `{dir}`:\n{locked}"
        );
    }
}

#[test]
fn a_separate_git_dir_brings_its_info_exclude_file_unquoted() {
    let project = web_project();
    // A submodule or `--separate-git-dir` checkout: a `.git` file naming the git directory, which
    // has no `commondir`, so it is the shared directory itself.
    write(&project, "outside.git/info/exclude", "gui*.md\n");
    write(
        &project,
        "services/web/.git",
        &format!("gitdir: {}\n", project.path("outside.git").display()),
    );

    assert_followed_and_never_quoted(&project);
}

#[test]
fn a_worktrees_shared_exclude_file_applies_and_is_never_quoted() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    // `git worktree add` leaves a `.git` file naming the worktree's git directory, whose
    // `commondir` leads to the main repository's, outside the root.
    write(
        &project,
        "services/web/.git",
        &format!(
            "gitdir: {}\n",
            project.path("main.git/worktrees/web").display()
        ),
    );
    write(&project, "main.git/worktrees/web/commondir", "../..\n");
    write(&project, "main.git/info/exclude", "scr*.md\n");
    write(&project, "services/web/docs/scratch.md", SCRATCH);

    let check = project.supdoc_in("services/web", &["check"]);
    let named = project.supdoc_in("services/web", &["check", "docs/scratch.md"]);

    assert!(!check.stdout.contains("scratch.md"), "{}", check.stdout);
    assert!(
        named.stdout.contains("ignored by an ignore file"),
        "{}",
        named.stdout
    );
    for run in [&check, &named] {
        assert!(
            !run.stdout.contains("scr*.md") && !run.stderr.contains("scr*.md"),
            "`supdoc {}` printed a line from outside the root\n{}\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
}

#[test]
fn a_parent_repositorys_exclude_file_applies_to_a_root_in_a_subdirectory() {
    let project = web_project();
    let _ = project
        .supdoc_in("services/web", &["update", "--yes"])
        .assert_exit(0);
    make_git_dir(&project, ".git");
    write(&project, ".git/info/exclude", "scratch.md\n");
    write(&project, "services/web/docs/scratch.md", SCRATCH);

    let run = project.supdoc_in("services/web", &["check"]);

    assert!(
        !run.stdout.contains("scratch.md"),
        "the parent's exclude rule hides scratch.md\n{}",
        run.stdout
    );
}

/// Asserts that the web project scans as git would have it with an ignore file it doesn't own
/// hiding the guide: `check` and `update --yes` succeed without the guide, the named guide is
/// blamed on "an ignore file", and no line of that file is printed.
fn assert_followed_and_never_quoted(project: &Project) {
    write(project, "services/web/docs/keep.md", SCRATCH);
    let update = project.supdoc_in("services/web", &["update", "--yes"]);
    let check = project.supdoc_in("services/web", &["check"]);
    let named = project.supdoc_in("services/web", &["check", "docs/guide.md"]);

    for run in [&update, &check, &named] {
        let _ = run.assert_exit(0);
        assert!(
            !run.stdout.contains("gui*.md") && !run.stderr.contains("gui*.md"),
            "`supdoc {}` printed a line from outside the root\n{}\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
        assert!(
            !run.stdout.contains("scan-ignore-file-symlink")
                && !run.stderr.contains("scan-ignore-file-symlink"),
            "`supdoc {}` warned about an ignore file it doesn't own\n{}\n{}",
            run.args,
            run.stdout,
            run.stderr
        );
    }
    let locked = project.read("services/web/supdoc.lock");
    assert!(
        locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
        "the rule hides the guide, as in git, and nothing else:\n{locked}"
    );
    assert!(
        named.stdout.contains("ignored by an ignore file"),
        "{}",
        named.stdout
    );
}

/// Makes `link` in `project` a symlink to a git directory outside the root whose exclude file
/// hides the guide.
#[cfg(unix)]
fn git_dir_symlinked_out(project: &Project, link: &str) {
    make_git_dir(project, "outside.git");
    write(project, "outside.git/info/exclude", "gui*.md\n");
    std::os::unix::fs::symlink(project.path("outside.git"), project.path(link))
        .expect("the symlink should be creatable");
}

#[cfg(unix)]
#[test]
fn a_subdirectory_whose_git_dir_is_symlinked_out_applies_its_exclude_file_unquoted() {
    let project = web_project();
    // Git refuses to check out a `.git` path, so a checkout can't plant this one.
    git_dir_symlinked_out(&project, "services/web/docs/.git");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_root_whose_git_dir_is_symlinked_out_applies_its_exclude_file_unquoted() {
    let project = web_project();
    git_dir_symlinked_out(&project, "services/web/.git");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_root_whose_git_info_is_symlinked_out_applies_its_exclude_file_unquoted() {
    let project = web_project();
    write(&project, "outside.git/info/exclude", "gui*.md\n");
    make_git_dir(&project, "services/web/.git");
    std::os::unix::fs::symlink(
        project.path("outside.git/info"),
        project.path("services/web/.git/info"),
    )
    .expect("the symlink should be creatable");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_dotfile_gitignore_above_the_root_symlinked_elsewhere_applies_unquoted() {
    let project = monorepo_with_git();
    // A dotfiles manager such as stow links the monorepo's `.gitignore` into a directory of its own.
    let dotfiles = tempfile::tempdir().expect("a dotfiles directory should be creatable");
    fs::write(dotfiles.path().join("gitignore"), "gui*.md\n")
        .expect("the dotfile should be writable");
    std::os::unix::fs::symlink(
        dotfiles.path().join("gitignore"),
        project.path(".gitignore"),
    )
    .expect("the symlink should be creatable");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_shared_ignore_file_above_the_root_symlinked_within_the_checkout_applies_unquoted() {
    let project = monorepo_with_git();
    // A monorepo whose root `.ignore` links to a shared file outside the subproject's root.
    write(&project, "tools/ignore", "gui*.md\n");
    std::os::unix::fs::symlink(project.path("tools/ignore"), project.path(".ignore"))
        .expect("the symlink should be creatable");

    assert_followed_and_never_quoted(&project);
}

#[cfg(unix)]
#[test]
fn a_parent_repository_whose_git_dir_is_symlinked_elsewhere_applies_its_exclude_unquoted() {
    let project = web_project();
    git_dir_symlinked_out(&project, ".git");

    assert_followed_and_never_quoted(&project);
}

/// The web project inside a git repository whose work tree is the whole `monorepo` fixture, so
/// the ignore files of the directories above the project apply.
fn monorepo_with_git() -> Project {
    let project = web_project();
    make_git_dir(&project, ".git");
    project
}

/// The web project's `supdoc.lock`, or nothing when `update` found no link to write one for.
fn lock_of_web(project: &Project) -> String {
    fs::read_to_string(project.path("services/web/supdoc.lock")).unwrap_or_default()
}

/// Asserts that `update --yes` in the web project records the guide, so no rule hides it.
fn assert_guide_recorded(project: &Project, why: &str) {
    let run = project.supdoc_in("services/web", &["update", "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        lock_of_web(project).contains("docs/guide.md"),
        "{why}\n{}\n{}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn a_gitignore_above_a_repository_root_does_not_apply() {
    let project = web_project();
    fs::write(project.path(".gitignore"), "*\n").expect("the gitignore should be writable");
    make_git_dir(&project, "services/web/.git");

    assert_guide_recorded(
        &project,
        "git reads no `.gitignore` above the work tree, so the `*` above the repository hides nothing",
    );
}

#[test]
fn a_gitignore_above_a_project_outside_any_repository_does_not_apply() {
    let project = web_project();
    fs::write(project.path(".gitignore"), "*\n").expect("the gitignore should be writable");

    assert_guide_recorded(
        &project,
        "with no `.git` in the root or above it, no `.gitignore` above the root applies",
    );
}

#[test]
fn a_gitignore_above_the_work_tree_top_does_not_apply_but_one_at_the_top_does() {
    let project = web_project();
    make_git_dir(&project, "services/.git");
    fs::write(project.path(".gitignore"), "gui*.md\n").expect("the gitignore should be writable");
    fs::write(project.path("services/.gitignore"), "scr*.md\n")
        .expect("the gitignore should be writable");
    write(&project, "services/web/docs/scratch.md", SCRATCH);

    let run = project.supdoc_in("services/web", &["update", "--yes"]);

    let _ = run.assert_exit(0);
    let locked = lock_of_web(&project);
    assert!(
        locked.contains("docs/guide.md") && !locked.contains("docs/scratch.md"),
        "the work tree's top `.gitignore` hides scratch.md, and the one above the top hides \
         nothing:\n{locked}"
    );
}

#[cfg(unix)]
#[test]
fn a_dangling_gitignore_above_the_root_changes_nothing() {
    let project = web_project();
    make_git_dir(&project, ".git");
    std::os::unix::fs::symlink(project.path("gone"), project.path(".gitignore"))
        .expect("the symlink should be creatable");

    let run = project.supdoc_in("services/web", &["update", "--yes"]);

    let _ = run.assert_exit(0);
    assert!(
        project
            .read("services/web/supdoc.lock")
            .contains("docs/guide.md"),
        "nothing hides the guide\n{}",
        run.stderr
    );
}

/// A `.git` entry git can't use, which it passes over in its search for the work tree top.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
enum UnusableGit {
    /// An empty directory.
    EmptyDir,
    /// A symlink to nothing.
    Dangling,
}

/// Every [`UnusableGit`].
#[cfg(unix)]
const UNUSABLE_GITS: [UnusableGit; 2] = [UnusableGit::EmptyDir, UnusableGit::Dangling];

/// The web project, whose own `.git` is `unusable`, inside a repository whose work tree is the
/// whole `monorepo` fixture, the top git finds.
#[cfg(unix)]
fn monorepo_with_unusable_web_git(unusable: UnusableGit) -> Project {
    let project = monorepo_with_git();
    let dot_git = project.path("services/web/.git");
    match unusable {
        UnusableGit::EmptyDir => {
            fs::create_dir(&dot_git).expect("the empty git directory should be creatable");
        }
        UnusableGit::Dangling => std::os::unix::fs::symlink(project.path("gone"), &dot_git)
            .expect("the dangling symlink should be creatable"),
    }
    project
}

#[cfg(unix)]
#[test]
fn a_gitignore_above_a_git_entry_git_cannot_use_applies_up_to_the_real_top() {
    for unusable in UNUSABLE_GITS {
        let project = monorepo_with_unusable_web_git(unusable);
        fs::write(project.path(".gitignore"), "gui*.md\n")
            .expect("the gitignore should be writable");
        write(&project, "services/web/docs/keep.md", SCRATCH);

        let run = project.supdoc_in("services/web", &["update", "--yes"]);

        let _ = run.assert_exit(0);
        let locked = lock_of_web(&project);
        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "git passes over a {unusable:?} `.git` to the monorepo's top, whose `.gitignore` hides \
             the guide:\n{locked}"
        );
    }
}

#[cfg(unix)]
#[test]
fn an_anchored_global_pattern_counts_from_the_real_top_past_a_git_entry_git_cannot_use() {
    for unusable in UNUSABLE_GITS {
        let project = monorepo_with_unusable_web_git(unusable);

        let locked =
            lock_with_global_pattern(&project, "services/web", "services/web/docs/gui*.md");

        assert!(
            locked.contains("docs/keep.md") && !locked.contains("docs/guide.md"),
            "git passes over a {unusable:?} `.git` and anchors the pattern at the monorepo's top, \
             so it hides the guide:\n{locked}"
        );
    }
}

#[test]
fn a_git_directory_holding_only_an_exclude_file_hides_nothing() {
    let project = monorepo_with_git();
    // Git rejects a `.git` without `HEAD`, `objects` and `refs`, so never reads its exclude file.
    write(&project, "services/web/.git/info/exclude", "gui*.md\n");

    assert_guide_recorded(
        &project,
        "git passes over a `.git` it rejects, so the exclude file inside it hides nothing",
    );
}

/// The web project with a directory, not a file, named `supdoc.config.toml` inside `sub`, and a
/// doc there linking to a source that does not exist.
fn web_project_with_a_config_named_directory_in_sub() -> Project {
    let project = web_project();
    fs::write(project.path("services/web/supdoc.config.toml"), "")
        .expect("the config should be writable");
    fs::create_dir_all(project.path("services/web/sub/supdoc.config.toml"))
        .expect("the directory should be creatable");
    fs::create_dir(project.path("services/web/sub/docs"))
        .expect("the directory should be creatable");
    fs::write(
        project.path("services/web/sub/docs/p.md"),
        "# P\n\n[gone](nothere.rs)\n",
    )
    .expect("the doc should be writable");
    project
}

/// The `source: kind` of each link finding in `run`'s `--json` output.
fn link_findings(run: &crate::support::Run) -> Vec<String> {
    array_at(&run.json(), "/data/links")
        .iter()
        .map(|link| {
            format!(
                "{}: {}",
                link["source"].as_str().unwrap_or_default(),
                link.pointer("/finding/kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
            )
        })
        .collect()
}

#[test]
fn a_directory_named_like_the_config_is_not_a_nested_project_from_the_root() {
    let project = web_project_with_a_config_named_directory_in_sub();

    let run = project.supdoc_in("services/web", &["--json", "check"]);

    assert!(
        run.stdout.contains("sub/docs/nothere.rs"),
        "the doc under `sub` should be scanned by the root project\n{}",
        run.stdout
    );
    assert!(
        link_findings(&run).contains(&"sub/docs/p.md: read-missing".to_owned()),
        "{}",
        run.stdout
    );
    let _ = run.assert_exit(1);
}

#[test]
fn a_directory_named_like_the_config_reports_the_same_from_inside_it() {
    let project = web_project_with_a_config_named_directory_in_sub();

    let from_root = project.supdoc_in("services/web", &["--json", "check"]);
    let from_sub = project.supdoc_in("services/web/sub", &["--json", "check"]);

    let _ = from_sub.assert_exit(1);
    assert_eq!(
        (
            at(&from_sub.json(), "/root").clone(),
            link_findings(&from_sub)
        ),
        (
            at(&from_root.json(), "/root").clone(),
            link_findings(&from_root)
        ),
        "both runs should use the root project's config and find the same things\nfrom sub:\n{}\nfrom root:\n{}",
        from_sub.stdout,
        from_root.stdout
    );
}
