//! Checking Dockerfile code blocks, tagged `dockerfile`, `Dockerfile`, `containerfile` or
//! `Containerfile`, formatted by dockerfmt and linted by hadolint.

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

/// A `dockerfile` table with dockerfmt's format step off, so the lint step alone decides a block.
const LINT_ONLY: &str = "[features.code-blocks]\n[features.code-blocks.dockerfile]\n\
                         [features.code-blocks.dockerfile.dockerfmt]\nformat = \"off\"\n";

/// A `dockerfile` table with hadolint's lint step off, so the format step alone decides a block.
const FORMAT_ONLY: &str = "[features.code-blocks]\n[features.code-blocks.dockerfile]\n\
                           [features.code-blocks.dockerfile.hadolint]\nlint = \"off\"\n";

/// A recorded `basic` project with `config` and `docs/blocks.md` holding `doc`.
fn project_with(config: &str, doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", config);
    project.write("docs/blocks.md", doc);
    project
}

/// A [`project_with`] [`LINT_ONLY`] and one block tagged `tag` holding `body`, whose first body
/// line is the doc's line 4.
fn dockerfile_project(tag: &str, body: &str) -> Project {
    project_with(LINT_ONLY, &format!("# Blocks\n\n```{tag}\n{body}\n```\n"))
}

/// A [`project_with`] [`FORMAT_ONLY`] and one `dockerfile` block holding `body`, whose first
/// body line is the doc's line 4.
fn formatted_project(body: &str) -> Project {
    project_with(
        FORMAT_ONLY,
        &format!("# Blocks\n\n```dockerfile\n{body}\n```\n"),
    )
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
    for config in [
        "\"format\": tty\n",
        "ignored: []\rformat: tty\n",
        "  ignored: [DL3006]\n",
    ] {
        let project = dockerfile_project("dockerfile", "FROM ubuntu");
        project.write(".hadolint.yml", config);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_hadolint(outside.path(), "exit 9");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{config:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("`.hadolint.yml`")),
            "{config:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/not_checked"),
            1,
            "{config:?}: {json}"
        );
    }
}

/// A [`dockerfile_project`] whose blocks are formatted by the custom command `fmt-stub`, which
/// leaves `formatted` beside itself and prints the block back unchanged.
fn custom_formatted_project(dir: &Path) -> Project {
    let project = dockerfile_project("dockerfile", "FROM ubuntu");
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.dockerfile]\n\
         formatter = { command = [\"fmt-stub\"] }\n",
    );
    let path = dir.join("fmt-stub");
    std::fs::write(&path, "#!/bin/sh\n: > \"${0%/*}/formatted\"\n/bin/cat\n")
        .expect("the formatter stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the formatter stub should be made executable");
    project
}

#[test]
fn dockerfile_an_unverified_hadolint_or_a_refused_config_still_runs_a_custom_formatter() {
    for (version, config, says) in [
        (
            "Haskell Dockerfile Linter 2.16.0",
            None,
            "isn't a version supdoc's checks are verified against",
        ),
        (
            "Haskell Dockerfile Linter 2.15.1",
            Some("  ignored: [DL3006]\n"),
            "`.hadolint.yaml`",
        ),
    ] {
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let project = custom_formatted_project(outside.path());
        if let Some(config) = config {
            project.write(".hadolint.yaml", config);
        }
        stub_hadolint(outside.path(), &stub_at(version, WARNING_EXIT_0));

        let json = project
            .supdoc_in_with(
                ".",
                &["check", "--json"],
                &[("PATH", outside.path()), ("SUPDOC_TRUST", Path::new("1"))],
            )
            .assert_exit(0)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{version}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains(says)),
            "{version}: {json}"
        );
        assert!(
            outside.path().join("formatted").exists(),
            "{version}: the custom formatter didn't run: {json}"
        );
        assert!(
            !outside.path().join("seen").exists(),
            "{version}: the lint step ran"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/lint_not_run"),
            1,
            "{version}: {json}"
        );
    }
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

/// Writes an executable `#!/bin/sh` stub named `dockerfmt` in `dir`: it prints `version` for
/// `version`; for any other run it leaves `ran` beside itself, refuses (exit 5) arguments other
/// than `-i 4 -n`, and then runs `format`.
fn stub_dockerfmt(dir: &Path, version: &str, format: &str) {
    let path = dir.join("dockerfmt");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             [ \"$1\" = version ] && {{ echo '{version}'; exit 0; }}\n\
             : > \"${{0%/*}}/ran\"\n\
             [ \"$*\" = '-i 4 -n' ] || exit 5\n\
             {format}\n"
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A verified dockerfmt's `version` line.
const VERIFIED: &str = "dockerfmt v0.6.0";

/// What dockerfmt 0.6.0 prints, with exit 0, for input of at most one byte (probe V6-PR2, 7).
const HELP: &str = "echo 'Format Dockerfiles and shell commands within RUN steps. If no files are \
                    specified, input is read from stdin.'; echo 'Usage:'; exit 0";

#[test]
fn dockerfile_an_unformatted_block_fails_and_fix_writes_dockerfmts_output() {
    let project = formatted_project("FROM   ubuntu:24.04\nRUN   a  &&  b");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_dockerfmt(
        outside.path(),
        VERIFIED,
        "/bin/cat > /dev/null; printf 'FROM ubuntu:24.04\\nRUN a && b\\n'",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 3, "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```dockerfile\nFROM ubuntu:24.04\nRUN a && b\n```\n"
    );
}

#[test]
fn dockerfile_a_block_of_at_most_one_byte_never_gets_dockerfmts_help_text() {
    for doc in [
        "# Blocks\n\n```dockerfile\n```\n",
        "# Blocks\n\n```dockerfile\n\n```\n",
    ] {
        let project = project_with(FORMAT_ONLY, doc);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_dockerfmt(outside.path(), VERIFIED, HELP);

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{doc:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("is at most one byte long")),
            "{doc:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/format_not_run"),
            1,
            "{doc:?}: {json}"
        );
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{doc:?}");
        assert!(
            !outside.path().join("ran").exists(),
            "{doc:?}: dockerfmt was given the block"
        );
        assert_eq!(project.read("docs/blocks.md"), doc);
    }
}

#[test]
fn dockerfile_dockerfmt_ignore_or_a_near_miss_leaves_the_format_step_not_run() {
    for body in [
        "FROM ubuntu:24.04\n# dockerfmt-ignore\nRUN   a  &&  b",
        "FROM ubuntu:24.04\n#  DockerFmt-Ignored\nRUN   a  &&  b",
    ] {
        let project = formatted_project(body);
        let before = project.read("docs/blocks.md");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_dockerfmt(outside.path(), VERIFIED, "/bin/cat");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("`dockerfmt-ignore`")),
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/format_not_run"),
            1,
            "{body:?}: {json}"
        );
        assert!(
            !outside.path().join("ran").exists(),
            "{body:?}: dockerfmt was given the block"
        );
        assert_eq!(project.read("docs/blocks.md"), before, "{body:?}");
    }
}

#[test]
fn dockerfile_a_block_of_only_comments_is_never_given_to_dockerfmt() {
    let project = formatted_project("# escape=`\n# a note");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_dockerfmt(
        outside.path(),
        VERIFIED,
        "echo 'dockerfmt: stdin: file with no instructions' >&2; exit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_not_run"),
        1,
        "{json}"
    );
    assert!(
        !outside.path().join("ran").exists(),
        "dockerfmt was given the block"
    );
}

#[test]
fn dockerfile_a_syntax_error_is_reported_at_its_doc_line() {
    let project = formatted_project("FROM ubuntu:24.04\nRUN <<EOF");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_dockerfmt(
        outside.path(),
        VERIFIED,
        "echo 'dockerfmt: stdin: line 2: unterminated heredoc' >&2; exit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn dockerfile_only_a_verified_dockerfmt_version_runs_the_format_step() {
    for (version, runs) in [
        ("dockerfmt v0.6.7", true),
        ("dockerfmt v0.7.0", false),
        ("dockerfmt v1.6.0", false),
        ("dockerfmt dev", false),
    ] {
        let project = formatted_project("FROM ubuntu:24.04");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_dockerfmt(outside.path(), version, "/bin/cat");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        if runs {
            assert!(finding_kinds(&json).is_empty(), "{version}: {json}");
            assert_eq!(
                at(&json, "/data/summary/code_blocks/passed"),
                1,
                "{version}: {json}"
            );
        } else {
            assert_eq!(
                finding_kinds(&json),
                ["code-block-not-checked"],
                "{version}: {json}"
            );
            assert!(
                at(&json, "/findings/0/message")
                    .as_str()
                    .is_some_and(|message| message
                        .contains("isn't a version supdoc's checks are verified against (0.6)")),
                "{version}: {json}"
            );
            assert_eq!(
                at(&json, "/data/summary/code_blocks/format_not_run"),
                1,
                "{version}: {json}"
            );
            assert!(
                !outside.path().join("ran").exists(),
                "{version}: the format step ran"
            );
        }
    }
}

#[test]
fn dockerfile_an_unverified_dockerfmt_still_leaves_the_lint_step_to_hadolint() {
    let project = project_with(
        "[features.code-blocks]\n[features.code-blocks.dockerfile]\n",
        "# Blocks\n\n```dockerfile\nFROM ubuntu\n```\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_dockerfmt(outside.path(), "dockerfmt v0.7.0", "/bin/cat");
    stub_hadolint(outside.path(), &stub(WARNING_EXIT_0));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(
        finding_kinds(&json),
        ["code-block-not-checked", "code-block-lint"],
        "{json}"
    );
    assert!(!outside.path().join("ran").exists(), "the format step ran");
}

#[test]
fn dockerfile_the_real_dockerfmt_formats_a_block_and_fix_writes_its_output() {
    let Some(dir) = real_tool_dir(
        "dockerfmt",
        "dockerfile_the_real_dockerfmt_formats_a_block_and_fix_writes_its_output",
    ) else {
        return;
    };

    let clean =
        formatted_project("FROM ubuntu:24.04\nRUN apt-get update && apt-get install -y curl");
    let clean = real_check(&clean, &dir, &[]);
    let messy = formatted_project(
        "FROM   ubuntu:24.04\nRUN   apt-get update &&    apt-get install -y curl",
    );
    let json = real_check(&messy, &dir, &[]);
    let _fixed = messy.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    let again = real_check(&messy, &dir, &[]);

    assert!(finding_kinds(&clean).is_empty(), "{clean}");
    assert_eq!(at(&clean, "/data/summary/code_blocks/passed"), 1, "{clean}");
    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        messy.read("docs/blocks.md"),
        "# Blocks\n\n```dockerfile\nFROM ubuntu:24.04\nRUN apt-get update && apt-get install -y curl\n```\n"
    );
    assert!(finding_kinds(&again).is_empty(), "{again}");
}

#[test]
fn dockerfile_the_real_dockerfmt_under_an_escape_directive_fix_writes_only_its_output() {
    let Some(dir) = real_tool_dir(
        "dockerfmt",
        "dockerfile_the_real_dockerfmt_under_an_escape_directive_fix_writes_only_its_output",
    ) else {
        return;
    };
    let project = formatted_project(
        "# escape=`\nFROM   mcr.microsoft.com/windows\nRUN   dir `\n    c:\\\\\nCOPY   a   b",
    );

    let json = real_check(&project, &dir, &[]);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    let again = real_check(&project, &dir, &[]);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```dockerfile\n# escape=`\nFROM mcr.microsoft.com/windows\nRUN   dir `\n    c:\\\\\nCOPY a b\n```\n"
    );
    assert!(finding_kinds(&again).is_empty(), "{again}");
}

#[test]
fn dockerfile_the_real_dockerfmt_is_never_given_a_block_of_at_most_one_byte() {
    let Some(dir) = real_tool_dir(
        "dockerfmt",
        "dockerfile_the_real_dockerfmt_is_never_given_a_block_of_at_most_one_byte",
    ) else {
        return;
    };
    for doc in [
        "# Blocks\n\n```dockerfile\n```\n",
        "# Blocks\n\n```dockerfile\n\n```\n",
    ] {
        let project = project_with(FORMAT_ONLY, doc);

        let json = real_check(&project, &dir, &[]);
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{doc:?}: {json}"
        );
        assert_eq!(project.read("docs/blocks.md"), doc);
    }
}
