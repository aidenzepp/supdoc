//! Checking Makefile code blocks, tagged `make`, `makefile`, `Makefile` or `mk`, formatted by
//! mbake.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::support::{Project, array_at, at, real_tool_dir};

/// The codes of a `--json` check's loose findings, in order.
fn finding_kinds(json: &serde_json::Value) -> Vec<String> {
    array_at(json, "/findings")
        .iter()
        .filter_map(|finding| finding["kind"].as_str().map(str::to_owned))
        .collect()
}

/// A `makefile` table: mbake formats, and no linter is known yet, so the format step alone
/// decides a block.
const MAKEFILE: &str = "[features.code-blocks]\n[features.code-blocks.makefile]\n";

/// A recorded `basic` project with [`MAKEFILE`] and `docs/blocks.md` holding `doc`.
fn project_with(doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", MAKEFILE);
    project.write("docs/blocks.md", doc);
    project
}

/// A [`project_with`] one block tagged `tag` holding `body`, whose first body line is the doc's
/// line 4.
fn makefile_project(tag: &str, body: &str) -> Project {
    project_with(&format!("# Blocks\n\n```{tag}\n{body}\n```\n"))
}

/// A fake mbake install in its own directory: an executable `#!/bin/sh` stub at `bin/mbake`
/// beside the `METADATA` of an `mbake` package at `version`, where supdoc reads the version.
struct Stub {
    /// Keeps the directory alive until the stub is dropped.
    dir: tempfile::TempDir,
}

impl Stub {
    /// The stub: every run appends its arguments to `args`, refuses (exit 5) any but
    /// `format --stdin --config <file>`, copies the config file it is given to `config`, then
    /// runs `format`.
    fn new(version: &str, format: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let bin = dir.path().join("bin");
        let info = dir
            .path()
            .join("lib/python3.12/site-packages/mbake-1.5.0.dist-info");
        std::fs::create_dir_all(&bin).expect("the stub's bin should be creatable");
        std::fs::create_dir_all(&info).expect("the stub's dist-info should be creatable");
        std::fs::write(
            info.join("METADATA"),
            format!("Metadata-Version: 2.4\nName: mbake\nVersion: {version}\n"),
        )
        .expect("the stub's METADATA should be writable");
        let script = bin.join("mbake");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 top=\"${{0%/bin/*}}\"\n\
                 printf '%s\\n' \"$*\" >> \"$top/args\"\n\
                 [ $# -eq 4 ] && [ \"$1 $2 $3\" = 'format --stdin --config' ] && [ -f \"$4\" ] || exit 5\n\
                 /bin/cp \"$4\" \"$top/config\"\n\
                 {format}\n"
            ),
        )
        .expect("the stub should be writable");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        Self { dir }
    }

    /// The `PATH` directory holding the stub.
    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    /// Each run's arguments, one line per run.
    fn runs(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("args"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The config the last run was given.
    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config"))
            .expect("the stub should have been given a config")
    }
}

/// A verified mbake's version.
const VERIFIED: &str = "1.5.0";

/// Runs `check --json` with `PATH` set to `dir` on `project`.
fn check(project: &Project, dir: &Path) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", dir)])
        .json()
}

/// Asserts `json` holds one `code-block-not-checked` warning whose message holds `said`, and
/// that the block's format step didn't run.
fn assert_not_run(json: &serde_json::Value, said: &str) {
    assert_eq!(finding_kinds(json), ["code-block-not-checked"], "{json}");
    assert!(
        at(json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains(said)),
        "{json}"
    );
    assert_eq!(
        at(json, "/data/summary/code_blocks/format_not_run"),
        1,
        "{json}"
    );
    assert_eq!(at(json, "/data/summary/code_blocks/passed"), 0, "{json}");
}

#[test]
fn makefile_every_tag_fails_unformatted_and_fix_writes_mbakes_output() {
    for tag in ["make", "makefile", "Makefile", "mk"] {
        let project = makefile_project(tag, "A=1\nall:x\n\techo");
        let stub = Stub::new(
            VERIFIED,
            "/bin/cat > /dev/null; printf 'A = 1\\nall: x\\n\\techo\\n'",
        );

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
            .assert_exit(1)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &stub.bin())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{tag}: {json}"
        );
        assert_eq!(at(&json, "/findings/0/location/line"), 3, "{tag}: {json}");
        assert_eq!(
            project.read("docs/blocks.md"),
            format!("# Blocks\n\n```{tag}\nA = 1\nall: x\n\techo\n```\n"),
            "{tag}"
        );
    }
}

#[test]
fn makefile_supdoc_never_asks_mbake_for_its_version_or_validates() {
    let project = makefile_project("make", "A=1");
    let stub = Stub::new(VERIFIED, "/bin/cat > /dev/null; printf 'A = 1\\n'");

    let _checked = check(&project, &stub.bin());
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &stub.bin())]);
    let again = check(&project, &stub.bin());

    let runs = stub.runs();
    assert!(!runs.is_empty(), "mbake never ran");
    for run in &runs {
        assert!(
            run.starts_with("format --stdin --config "),
            "supdoc ran `mbake {run}`"
        );
        for banned in ["--version", "validate", "--validate", "update", "--check"] {
            assert!(
                !run.split(' ').any(|arg| arg == banned),
                "supdoc ran `mbake {run}`"
            );
        }
    }
    assert!(finding_kinds(&again).is_empty(), "{again}");
}

#[test]
fn makefile_bake_format_in_any_spelling_leaves_the_format_step_not_run() {
    for body in [
        "# bake-format off\nA=1",
        "#BAKE-FORMAT   Off\nA=1",
        "# bake-format offx\nA=1",
        "A=1\n# bake-format on",
        "all:\n\t@echo hi # bake-format off",
        "# ba\u{212a}e-format off\nA=1",
        "# format\u{a0}off\nA=1",
    ] {
        let project = makefile_project("make", body);
        let doc = project.read("docs/blocks.md");
        let stub = Stub::new(VERIFIED, "/bin/cat > /dev/null; printf 'changed\\n'");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
            .assert_exit(0)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &stub.bin())]);

        assert_not_run(&json, "holds `bake-format`");
        assert!(
            stub.runs().is_empty(),
            "{body:?}: mbake was given the block"
        );
        assert_eq!(project.read("docs/blocks.md"), doc, "{body:?}");
    }
}

#[test]
fn makefile_a_block_holding_a_carriage_return_is_never_given_to_mbake() {
    let project = makefile_project("make", "all:\n\techo a\rb");
    let doc = project.read("docs/blocks.md");
    let stub = Stub::new(
        VERIFIED,
        "/bin/cat > /dev/null; printf 'all:\\n\\techo a\\n'",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
        .assert_exit(0)
        .json();
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &stub.bin())]);

    assert_not_run(&json, "holds a carriage return");
    assert!(stub.runs().is_empty(), "mbake was given the block");
    assert_eq!(project.read("docs/blocks.md"), doc);
}

#[test]
fn makefile_a_block_holding_another_control_or_separator_character_is_never_given_to_mbake() {
    for body in [
        "A = 1\u{b}\nall:\n\techo a",
        "A = a\u{2028}b\nB = \u{fdd0}",
        "A = \u{85}",
        "A = \u{7f}",
    ] {
        let project = makefile_project("make", body);
        let doc = project.read("docs/blocks.md");
        let stub = Stub::new(VERIFIED, "/bin/cat > /dev/null; printf 'A = 1\\n'");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
            .assert_exit(0)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &stub.bin())]);

        assert_not_run(&json, "another control or separator character");
        assert!(
            stub.runs().is_empty(),
            "{body:?}: mbake was given the block"
        );
        assert_eq!(project.read("docs/blocks.md"), doc, "{body:?}");
    }
}

#[test]
fn makefile_mbakes_upgrade_notice_on_stdout_is_a_tool_failure_not_a_format_result() {
    for notice in [
        "printf '\\033[93m[mbake upgrade notice]\\033[0m\\nTo ensure a clean upgrade\\n'; cat",
        "printf '[mbake upgrade notice]\\n'; cat",
        "printf '\\033[0m'; cat",
    ] {
        let project = makefile_project("make", "A = 1");
        let doc = project.read("docs/blocks.md");
        let stub = Stub::new(VERIFIED, notice);

        let json = check(&project, &stub.bin());
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &stub.bin())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-tool-failed"],
            "{notice}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("upgrade notice")),
            "{notice}: {json}"
        );
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
        assert_eq!(project.read("docs/blocks.md"), doc, "{notice}");
    }
}

#[test]
fn makefile_a_rule_error_is_reported_at_its_doc_line() {
    let project = makefile_project("make", "all:\n\ta\nall:\n\tb");
    let stub = Stub::new(
        VERIFIED,
        "/bin/cat > /dev/null; echo \"3: Error: Duplicate target 'all' defined at lines 1 and 3.\" >&2; exit 2",
    );

    let json = check(&project, &stub.bin());

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
}

#[test]
fn makefile_a_config_error_on_stdout_is_a_tool_failure_not_a_pass() {
    let project = makefile_project("make", "A = 1");
    let doc = project.read("docs/blocks.md");
    let stub = Stub::new(
        VERIFIED,
        "/bin/cat > /dev/null; echo 'Fatal error: Failed to parse configuration file'; exit 1",
    );

    let json = check(&project, &stub.bin());
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &stub.bin())]);

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
    assert_eq!(project.read("docs/blocks.md"), doc);
}

#[test]
fn makefile_only_a_verified_mbake_version_runs_the_format_step() {
    for (version, runs) in [
        ("1.5.7", true),
        ("1.6.0", false),
        ("1.4.2", false),
        ("2.5.0", false),
        ("1.5", false),
    ] {
        let project = makefile_project("make", "A = 1");
        let stub = Stub::new(version, "/bin/cat");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
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
            assert_not_run(
                &json,
                "isn't a version supdoc's checks are verified against (1.5)",
            );
            assert!(stub.runs().is_empty(), "{version}: the format step ran");
        }
    }
}

#[test]
fn makefile_the_private_config_keeps_only_the_typed_formatter_keys_of_the_nearest_bake_toml() {
    let project = makefile_project("make", "A = 1");
    project.write(
        ".bake.toml",
        "debug = true\nverbose = true\ngnu_error_format = false\n[formatter]\ntab_width = 8\n",
    );
    project.write(
        "docs/.bake.toml",
        "debug = true\n[formatter]\nspace_around_assignment = false\nmax_line_length = 80\n\
         unknown = \"x\"\n",
    );
    let stub = Stub::new(VERIFIED, "/bin/cat");

    let json = check(&project, &stub.bin());

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(
        stub.config(),
        "[formatter]\nspace_around_assignment = false\nmax_line_length = 80\n"
    );
}

#[test]
fn makefile_with_no_bake_toml_mbake_gets_a_config_of_its_defaults() {
    let project = makefile_project("make", "A = 1");
    let stub = Stub::new(VERIFIED, "/bin/cat");

    let json = check(&project, &stub.bin());

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(stub.config(), "[formatter]\n");
}

#[test]
fn makefile_a_bake_toml_supdoc_cant_write_again_leaves_the_blocks_unformatted() {
    for config in [
        "[formatter\n",
        "[formatter]\ntab_width = \"4\"\n",
        "[formatter]\nphony_at_top = 1\n",
    ] {
        let project = makefile_project("make", "A = 1");
        project.write(".bake.toml", config);
        let stub = Stub::new(VERIFIED, "/bin/cat");

        let json = check(&project, &stub.bin());

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{config:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("the project's `.bake.toml`")),
            "{config:?}: {json}"
        );
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
        assert!(stub.runs().is_empty(), "{config:?}: mbake ran");
    }
}

#[test]
fn makefile_the_real_mbake_formats_a_block_and_fix_writes_its_output() {
    let Some(dir) = real_tool_dir(
        "mbake",
        "makefile_the_real_mbake_formats_a_block_and_fix_writes_its_output",
    ) else {
        return;
    };
    let project = makefile_project("makefile", "CC=gcc\nall:app\n    $(CC) -o app main.c");

    let json = check(&project, &dir);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    let again = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```makefile\nCC = gcc\nall: app\n\t$(CC) -o app main.c\n```\n"
    );
    assert!(finding_kinds(&again).is_empty(), "{again}");
    assert_eq!(at(&again, "/data/summary/code_blocks/passed"), 1, "{again}");
}

#[test]
fn makefile_the_real_mbake_reports_a_duplicate_target_at_its_doc_line() {
    let Some(dir) = real_tool_dir(
        "mbake",
        "makefile_the_real_mbake_reports_a_duplicate_target_at_its_doc_line",
    ) else {
        return;
    };
    let project = makefile_project("make", "all:\n\ta\nall:\n\tb");

    let json = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
}

#[test]
fn makefile_the_real_mbake_reads_the_projects_bake_toml_and_never_one_from_home_or_the_cwd() {
    let Some(dir) = real_tool_dir(
        "mbake",
        "makefile_the_real_mbake_reads_the_projects_bake_toml_and_never_one_from_home_or_the_cwd",
    ) else {
        return;
    };
    // The project keeps `A=1` as written; a home or XDG config would also keep `all:x`.
    let project = makefile_project("make", "A=1\nall: x");
    project.write(
        "docs/.bake.toml",
        "[formatter]\nspace_around_assignment = false\n",
    );
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let xdg = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let canary = "[formatter]\nspace_after_colon = false\nspace_around_assignment = true\n";
    std::fs::write(home.path().join(".bake.toml"), canary).expect("the canary should be writable");
    std::fs::write(xdg.path().join("bake.toml"), canary).expect("the canary should be writable");

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[
                ("PATH", &dir),
                ("HOME", home.path()),
                ("XDG_CONFIG_HOME", xdg.path()),
            ],
        )
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn makefile_the_real_mbake_is_never_given_a_block_that_switches_its_format_off() {
    let Some(dir) = real_tool_dir(
        "mbake",
        "makefile_the_real_mbake_is_never_given_a_block_that_switches_its_format_off",
    ) else {
        return;
    };
    let project = makefile_project("make", "# bake-format off\nA=1");
    let doc = project.read("docs/blocks.md");

    let json = check(&project, &dir);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

    assert_not_run(&json, "holds `bake-format`");
    assert_eq!(project.read("docs/blocks.md"), doc);
}
