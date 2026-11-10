//! Checking Makefile code blocks, tagged `make`, `makefile`, `Makefile` or `mk`, formatted by
//! mbake and linted by checkmake.

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

/// A `makefile` table with checkmake's lint step off, so mbake's format step alone decides a
/// block.
const MAKEFILE: &str = "[features.code-blocks]\n[features.code-blocks.makefile]\n\
                        [features.code-blocks.makefile.checkmake]\nlint = \"off\"\n";

/// A `makefile` table with mbake's format step off, so checkmake's lint step alone decides a
/// block.
const LINT_ONLY: &str = "[features.code-blocks]\n[features.code-blocks.makefile]\n\
                         [features.code-blocks.makefile.mbake]\nformat = \"off\"\n";

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

/// A [`makefile_project`] whose table is [`LINT_ONLY`].
fn lint_project(tag: &str, body: &str) -> Project {
    let project = makefile_project(tag, body);
    project.write("supdoc.config.toml", LINT_ONLY);
    project
}

/// A fake checkmake in its own directory: an executable `#!/bin/sh` stub at `bin/checkmake` that
/// prints `checkmake v<version> built at ...` for `--version`. Any other run appends its arguments
/// to `args`, refuses (exit 5) any but `-o json --config <file> <file>`, copies the config to
/// `config` and the block file to `block`, writes the block file's path to `path` and the modes
/// of it and its directory to `modes`, then runs `lint`.
struct Checkmake {
    /// Keeps the directory alive until the stub is dropped.
    dir: tempfile::TempDir,
}

impl Checkmake {
    /// The stub, as [`Checkmake`] describes it.
    fn new(version: &str, lint: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("the stub's bin should be creatable");
        let script = bin.join("checkmake");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 top=\"${{0%/bin/*}}\"\n\
                 if [ \"$1\" = --version ]; then echo 'checkmake v{version} built at x by y with go'; exit 0; fi\n\
                 printf '%s\\n' \"$*\" >> \"$top/args\"\n\
                 [ $# -eq 5 ] && [ \"$1 $2 $3\" = '-o json --config' ] && [ -f \"$4\" ] && [ -f \"$5\" ] || exit 5\n\
                 /bin/cp \"$4\" \"$top/config\"\n\
                 /bin/cp \"$5\" \"$top/block\"\n\
                 printf '%s' \"$5\" > \"$top/path\"\n\
                 /bin/ls -ld \"${{5%/*}}\" \"${{5%/*/*}}\" \"$5\" > \"$top/modes\"\n\
                 {lint}\n"
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

    /// What the stub wrote to `name`, empty when it wrote nothing there.
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }
}

/// A verified checkmake's version.
const CHECKMAKE: &str = "0.3.2";

/// A checkmake stub's lint: two `uniquetargets` findings and a `maxbodylength` one, out of order,
/// with the count on stderr, exit 1.
const THREE_FINDINGS: &str = "printf '%s' '[{\"rule\":\"uniquetargets\",\"violation\":\"Target \\\"b\\\" defined multiple times (lines 3 and 5).\",\"file_name\":\"x\",\"line_number\":5},{\"rule\":\"uniquetargets\",\"violation\":\"Target \\\"a\\\" defined multiple times (lines 1 and 3).\",\"file_name\":\"x\",\"line_number\":3},{\"rule\":\"maxbodylength\",\"violation\":\"long\",\"file_name\":\"x\",\"line_number\":3}]'; echo 'Error: violations found (3)' >&2; exit 1";

/// The `(line, message)` of each finding in `json`, in order.
fn lines_and_messages(json: &serde_json::Value) -> Vec<(u64, String)> {
    array_at(json, "/findings")
        .iter()
        .map(|finding| {
            (
                finding
                    .pointer("/location/line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
                finding
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

#[test]
fn makefile_every_tag_is_linted_and_findings_fail_at_their_doc_lines_sorted_by_line_then_rule() {
    for tag in ["make", "makefile", "Makefile", "mk"] {
        let project = lint_project(tag, "a:\n\tx\na:\n\ty\nb:\n\tz");
        let stub = Checkmake::new(CHECKMAKE, THREE_FINDINGS);

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
            .assert_exit(1)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-lint", "code-block-lint", "code-block-lint"],
            "{tag}: {json}"
        );
        let found = lines_and_messages(&json);
        let [(6, first), (6, second), (8, _)] = found.as_slice() else {
            panic!("{tag}: the findings should be at doc lines 6, 6 and 8: {json}");
        };
        assert!(
            first.contains("long") && second.contains("\"a\""),
            "maxbodylength sorts before uniquetargets on one line: {json}"
        );
        assert_eq!(stub.read("block"), "a:\n\tx\na:\n\ty\nb:\n\tz\n", "{tag}");
    }
}

#[test]
fn makefile_checkmake_reads_one_private_file_in_a_private_directory_removed_after_the_run() {
    let project = lint_project("make", "all:\n\techo");
    let stub = Checkmake::new(CHECKMAKE, "exit 0");

    let json = check(&project, &stub.bin());

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    let path = stub.read("path");
    assert!(path.ends_with("/block.mk"), "the block file was {path:?}");
    assert!(
        !Path::new(&path).exists() && !Path::new(&path).parent().is_some_and(Path::exists),
        "the block file and its directory should be removed: {path}"
    );
    assert!(
        !path.starts_with(&*project.path("").to_string_lossy()),
        "the block file should sit outside the project: {path}"
    );
    let modes = stub.read("modes");
    let kinds: Vec<&str> = modes
        .lines()
        .map(|line| line.split(' ').next().unwrap_or_default())
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|mode| mode.starts_with("drwx------"))
            .count(),
        2,
        "the run's directory and checkmake-lint are 0700: {modes}"
    );
    assert!(
        kinds.iter().any(|mode| mode.starts_with("-rw-------")),
        "the block file is 0600: {modes}"
    );
}

#[test]
fn makefile_an_error_exit_without_the_json_array_is_a_tool_failure_not_a_pass() {
    for lint in [
        "echo 'Error: failed to parse \"x\"' >&2; exit 1",
        "exit 1",
        "echo 'RULE  DESCRIPTION'; echo 'Error: violations found (1)' >&2; exit 1",
        "printf '[]'; echo 'Error: violations found (0)' >&2; exit 1",
        "echo 'Usage: checkmake'; exit 0",
    ] {
        let project = lint_project("make", "all:\n\techo");
        let stub = Checkmake::new(CHECKMAKE, lint);

        let json = check(&project, &stub.bin());

        assert_eq!(
            finding_kinds(&json),
            ["code-block-tool-failed"],
            "{lint}: {json}"
        );
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
    }
}

#[test]
fn makefile_a_minphony_finding_is_a_tool_failure_since_supdoc_switches_it_off() {
    let project = lint_project("make", "all:\n\techo");
    let stub = Checkmake::new(
        CHECKMAKE,
        "printf '%s' '[{\"rule\":\"minphony\",\"violation\":\"Required target \\\"clean\\\" is missing from the Makefile.\",\"file_name\":\"x\",\"line_number\":1}]'; echo 'Error: violations found (1)' >&2; exit 1",
    );

    let json = check(&project, &stub.bin());

    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("minphony")),
        "{json}"
    );
}

#[test]
fn makefile_checkmake_always_gets_a_private_config_switching_minphony_off() {
    let project = lint_project("make", "all:\n\techo");
    project.write(
        "checkmake.ini",
        "output = text\n[default]\nformat = x\n[minphony]\ndisabled = false\nrequired = all\n\
         [maxbodylength]\nmaxBodyLength = 2\n[uniquetargets]\nignore = a, b\n",
    );
    let stub = Checkmake::new(CHECKMAKE, "exit 0");
    let bare = lint_project("make", "all:\n\techo");
    let bare_stub = Checkmake::new(CHECKMAKE, "exit 0");

    let json = check(&project, &stub.bin());
    let bare_json = check(&bare, &bare_stub.bin());

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert!(finding_kinds(&bare_json).is_empty(), "{bare_json}");
    assert_eq!(
        stub.read("config"),
        "[minphony]\ndisabled = true\n\n[maxbodylength]\nmaxBodyLength = 2\n\n\
         [uniquetargets]\nignore = a, b\n"
    );
    assert_eq!(bare_stub.read("config"), "[minphony]\ndisabled = true\n");
}

#[test]
fn makefile_a_config_switching_off_every_rule_or_one_supdoc_cant_read_leaves_the_blocks_unlinted() {
    let every = "[maxbodylength]\ndisabled = true\n[phonydeclared]\ndisabled = true\n\
                 [timestampexpanded]\ndisabled = true\n[uniquetargets]\ndisabled = true\n";
    for (config, said) in [
        (every, "switches off every checkmake rule"),
        ("[uniquetargets]\ndisabled = TRUE\n", "doesn't accept"),
        (
            "[uniqueTargets]\ndisabled = true\n",
            "isn't a checkmake rule",
        ),
        ("[uniquetargets]\ndisabled: true\n", "neither a section"),
    ] {
        let project = lint_project("make", "all:\n\techo");
        project.write("docs/checkmake.ini", config);
        let stub = Checkmake::new(CHECKMAKE, "exit 0");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
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
                .is_some_and(|message| {
                    message.contains("the project's `docs/checkmake.ini`") && message.contains(said)
                }),
            "{config:?}: {json}"
        );
        assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
        assert!(stub.read("args").is_empty(), "{config:?}: checkmake ran");
    }
}

#[test]
fn makefile_only_a_verified_checkmake_version_runs_the_lint_step() {
    for (version, runs) in [
        ("0.3.9", true),
        ("0.4.0", false),
        ("0.2.2", false),
        ("1.3.2", false),
        ("0.3", false),
    ] {
        let project = lint_project("make", "all:\n\techo");
        let stub = Checkmake::new(version, "exit 0");

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
            assert_eq!(
                finding_kinds(&json),
                ["code-block-not-checked"],
                "{version}: {json}"
            );
            assert!(
                at(&json, "/findings/0/message")
                    .as_str()
                    .is_some_and(|message| message
                        .contains("isn't a version supdoc's checks are verified against (0.3)")),
                "{version}: {json}"
            );
            assert_eq!(
                at(&json, "/data/summary/code_blocks/lint_not_run"),
                1,
                "{version}: {json}"
            );
            assert!(stub.read("args").is_empty(), "{version}: the lint step ran");
        }
    }
}

#[test]
fn makefile_a_line_longer_than_checkmake_reads_is_never_given_to_it() {
    let project = lint_project("make", &format!("A = {}", "x".repeat(65_532)));
    let stub = Checkmake::new(CHECKMAKE, "exit 0");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &stub.bin())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("longer than 65535 bytes")),
        "{json}"
    );
    assert!(
        stub.read("args").is_empty(),
        "checkmake was given the block"
    );
}

#[test]
fn makefile_the_real_checkmake_reports_a_duplicate_target_at_its_doc_line() {
    let Some(dir) = real_tool_dir(
        "checkmake",
        "makefile_the_real_checkmake_reports_a_duplicate_target_at_its_doc_line",
    ) else {
        return;
    };
    let project = lint_project("make", "foo:\n\ta\nfoo:\n\tb\nNOW = $(shell date)");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        finding_kinds(&json),
        ["code-block-lint", "code-block-lint"],
        "{json}"
    );
    let found = lines_and_messages(&json);
    let [(6, duplicate), (8, timestamp)] = found.as_slice() else {
        panic!("the findings should be at doc lines 6 and 8: {json}");
    };
    assert!(duplicate.contains("defined multiple times"), "{json}");
    assert!(timestamp.contains("\"NOW\""), "{json}");
}

#[test]
fn makefile_the_real_checkmake_never_runs_minphony_on_a_block() {
    let Some(dir) = real_tool_dir(
        "checkmake",
        "makefile_the_real_checkmake_never_runs_minphony_on_a_block",
    ) else {
        return;
    };
    let project = lint_project("make", "build:\n\tcc -o app main.c");
    project.write(
        "checkmake.ini",
        "[minphony]\ndisabled = false\nrequired = all, clean, test\n",
    );
    let empty = lint_project("make", "");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let empty_json = empty
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert!(finding_kinds(&empty_json).is_empty(), "{empty_json}");
}

#[test]
fn makefile_the_real_checkmake_reads_the_projects_config_and_never_one_from_home() {
    let Some(dir) = real_tool_dir(
        "checkmake",
        "makefile_the_real_checkmake_reads_the_projects_config_and_never_one_from_home",
    ) else {
        return;
    };
    // The project allows two-line bodies; the home config would allow one.
    let body = "a:\n\tx\n\ty\nb:\n\tx\n\ty\n\tz";
    let project = lint_project("make", body);
    project.write("checkmake.ini", "[maxbodylength]\nmaxBodyLength = 2\n");
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    std::fs::write(
        home.path().join("checkmake.ini"),
        "[maxbodylength]\nmaxBodyLength = 1\n",
    )
    .expect("the canary should be writable");

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", &dir), ("HOME", home.path())],
        )
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    let found = lines_and_messages(&json);
    let [(7, message)] = found.as_slice() else {
        panic!("the finding should be at doc line 7: {json}");
    };
    assert!(message.contains("allowed length of 2 lines (3)"), "{json}");
}
