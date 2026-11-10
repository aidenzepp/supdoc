//! Checking Dockerfile code blocks, tagged `dockerfile`, `Dockerfile`, `containerfile` or
//! `Containerfile`, linted by hadolint. supdoc knows no Dockerfile formatter yet.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use crate::support::{Project, array_at, at, real_tool_dir};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// The rules named by a `--json` check's `code-block-lint` findings, in order.
fn lint_rules(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter(|finding| finding["kind"] == "code-block-lint")
        .filter_map(|finding| finding["message"].as_str())
        .filter_map(|message| {
            let (_, rest) = message.split_once("reports `")?;
            rest.split_once('`').map(|(rule, _)| rule.to_owned())
        })
        .collect()
}

/// A recorded `basic` project with a `dockerfile` table and one block tagged `tag` holding `body`
/// in `docs/blocks.md`, whose first body line is the doc's line 4.
fn dockerfile_project(tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.dockerfile]\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `hadolint` running `script` in `dir`.
fn stub_hadolint(dir: &Path, script: &str) {
    let path = dir.join("hadolint");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A stub printing `version` for `--version`; otherwise it refuses (exit 5) a run without
/// `-f json` or a `--config` naming a file, appends that config to `seen` beside itself, and then
/// runs `lint`.
fn stub_at(version: &str, lint: &str) -> String {
    format!(
        "[ \"$1\" = --version ] && {{ echo '{version}'; exit 0; }}\n\
         case \" $* \" in *' -f json '*) ;; *) exit 5;; esac\n\
         config=; prev=\n\
         for arg in \"$@\"; do [ \"$prev\" = --config ] && config=\"$arg\"; prev=\"$arg\"; done\n\
         [ -f \"$config\" ] || exit 5\n\
         /bin/cat \"$config\" >> \"${{0%/*}}/seen\"\n\
         {lint}"
    )
}

/// [`stub_at`] as a verified hadolint.
fn stub(lint: &str) -> String {
    stub_at("Haskell Dockerfile Linter 2.15.1", lint)
}

/// One warning-level finding at the block's line 1, printed with exit 0, as hadolint does under a
/// `no-fail` or a `failure-threshold` above warning.
const WARNING_EXIT_0: &str = "echo '[{\"code\":\"DL3006\",\"column\":1,\"file\":\"-\",\"level\":\"warning\",\"line\":1,\"message\":\"Always tag the version of an image explicitly\"}]'; exit 0";

#[test]
fn dockerfile_every_tag_is_linted_and_a_finding_on_exit_0_fails_at_its_doc_line() {
    for tag in ["dockerfile", "Dockerfile", "containerfile", "Containerfile"] {
        let project = dockerfile_project(tag, "FROM ubuntu");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_hadolint(outside.path(), &stub(WARNING_EXIT_0));

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(1)
            .json();

        assert_eq!(finding_kinds(&json), ["code-block-lint"], "{tag}: {json}");
        assert_eq!(at(&json, "/findings/0/location/line"), 4, "{tag}: {json}");
    }
}

#[test]
fn dockerfile_the_copy_drops_no_fail_failure_threshold_and_format() {
    let project = dockerfile_project("dockerfile", "FROM ubuntu");
    project.write(
        ".hadolint.yaml",
        "no-fail: true\nfailure-threshold: error\nformat: tty\nno-color: true\nignored:\n  - DL3008\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_hadolint(outside.path(), &stub(WARNING_EXIT_0));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    let seen = std::fs::read_to_string(outside.path().join("seen"))
        .expect("the stub should have seen a config");
    assert_eq!(seen, "ignored:\n  - DL3008\n");
}

#[test]
fn dockerfile_with_no_project_config_hadolint_still_gets_a_private_config() {
    let project = dockerfile_project("dockerfile", "FROM ubuntu:24.04");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_hadolint(outside.path(), &stub("echo '[]'; exit 0"));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    let seen = std::fs::read_to_string(outside.path().join("seen"))
        .expect("the stub should have seen a config");
    assert_eq!(seen, "ignored: []\n");
}

#[test]
fn dockerfile_a_config_error_on_stderr_is_a_tool_failure_not_a_pass() {
    let project = dockerfile_project("dockerfile", "FROM ubuntu:24.04");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_hadolint(
        outside.path(),
        &stub(
            "echo \"Error parsing your config file in  'x':\" >&2; echo 'It should contain one of the keys' >&2; echo '[]'; exit 0",
        ),
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("couldn't read the config supdoc gave it")),
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
}

#[test]
fn dockerfile_a_config_line_supdoc_cant_sort_leaves_the_blocks_unchecked() {
    let project = dockerfile_project("dockerfile", "FROM ubuntu");
    project.write(".hadolint.yml", "\"format\": tty\n");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_hadolint(outside.path(), "exit 9");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("`.hadolint.yml`")),
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        1,
        "{json}"
    );
}

#[test]
fn dockerfile_only_a_verified_hadolint_version_runs_the_lint_step() {
    for (version, runs) in [
        ("Haskell Dockerfile Linter 2.15.9", true),
        ("Haskell Dockerfile Linter 2.16.0", false),
        ("Haskell Dockerfile Linter 3.15.1", false),
        ("garbage", false),
    ] {
        let project = dockerfile_project("dockerfile", "FROM ubuntu");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_hadolint(outside.path(), &stub_at(version, WARNING_EXIT_0));

        let run = project.supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())]);

        if runs {
            let json = run.assert_exit(1).json();
            assert_eq!(
                finding_kinds(&json),
                ["code-block-lint"],
                "{version}: {json}"
            );
        } else {
            let json = run.assert_exit(0).json();
            assert_eq!(
                finding_kinds(&json),
                ["code-block-not-checked"],
                "{version}: {json}"
            );
            assert!(
                at(&json, "/findings/0/message")
                    .as_str()
                    .is_some_and(|message| message
                        .contains("isn't a version supdoc's checks are verified against (2.15)")),
                "{version}: {json}"
            );
            assert_eq!(
                at(&json, "/data/summary/code_blocks/lint_not_run"),
                1,
                "{version}: {json}"
            );
            assert!(
                !outside.path().join("seen").exists(),
                "{version}: the lint step ran"
            );
        }
    }
}

/// Runs `check --json` with the real hadolint in `dir` on `project`, with `vars` added.
fn real_check(project: &Project, dir: &Path, vars: &[(&str, &Path)]) -> serde_json::Value {
    let mut all = vec![("PATH", dir)];
    all.extend_from_slice(vars);
    project
        .supdoc_in_with(".", &["check", "--json"], &all)
        .json()
}

#[test]
fn dockerfile_the_real_hadolint_reports_every_level_at_its_doc_line() {
    let Some(dir) = real_tool_dir(
        "hadolint",
        "dockerfile_the_real_hadolint_reports_every_level_at_its_doc_line",
    ) else {
        return;
    };

    let clean = dockerfile_project("dockerfile", "FROM ubuntu:24.04");
    let clean = real_check(&clean, &dir, &[]);
    let info = dockerfile_project("Containerfile", "FROM ubuntu:24.04\nRUN echo a\nRUN echo b");
    let info = real_check(&info, &dir, &[]);

    assert!(finding_kinds(&clean).is_empty(), "{clean}");
    assert_eq!(lint_rules(&info), ["DL3059"], "{info}");
    assert_eq!(at(&info, "/findings/0/location/line"), 6, "{info}");
}

#[test]
fn dockerfile_the_real_hadolint_ignores_no_fail_threshold_format_and_its_environment() {
    let Some(dir) = real_tool_dir(
        "hadolint",
        "dockerfile_the_real_hadolint_ignores_no_fail_threshold_format_and_its_environment",
    ) else {
        return;
    };
    let project = dockerfile_project("dockerfile", "FROM ubuntu\nRUN apt-get install curl");
    project.write(
        ".hadolint.yaml",
        "no-fail: true\nfailure-threshold: error\nformat: tty\nignored: [DL3015]\n",
    );

    let json = real_check(
        &project,
        &dir,
        &[
            ("HADOLINT_IGNORE", Path::new("DL3006,DL3008")),
            ("HADOLINT_FORMAT", Path::new("tty")),
        ],
    );

    assert_eq!(lint_rules(&json), ["DL3006", "DL3008", "DL3014"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
}

#[test]
fn dockerfile_the_real_hadolint_never_takes_a_config_from_home_or_the_cwd() {
    let Some(dir) = real_tool_dir(
        "hadolint",
        "dockerfile_the_real_hadolint_never_takes_a_config_from_home_or_the_cwd",
    ) else {
        return;
    };
    let project = dockerfile_project("dockerfile", "FROM ubuntu");
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let xdg = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    std::fs::write(home.path().join(".hadolint.yaml"), "ignored: [DL3006]\n")
        .expect("the home config should be writable");
    std::fs::write(xdg.path().join("hadolint.yaml"), "ignored: [DL3006]\n")
        .expect("the XDG config should be writable");

    let json = real_check(
        &project,
        &dir,
        &[("HOME", home.path()), ("XDG_CONFIG_HOME", xdg.path())],
    );

    assert_eq!(lint_rules(&json), ["DL3006"], "{json}");
}
