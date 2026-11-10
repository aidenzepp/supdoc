//! Checking SQL code blocks, tagged `sql` or a dialect's name, formatted by `sqruff fix` under a
//! private config that keeps only the layout and capitalisation rules, and linted by `sqruff lint`
//! under a second private config with sqruff's own rule set.

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

/// A recorded `basic` project whose `[features.code-blocks.sql]` table holds `table`, with one
/// block tagged `tag` holding `body` in `docs/blocks.md`, whose first body line is the doc's
/// line 4.
fn sql_project(table: &str, tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.sql]\n{table}"),
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
    );
    project
}

/// The report `sqruff lint -f json` prints for a block with no findings.
const NO_FINDINGS: &str = "{\"<string>\":[]}";

/// Writes an executable `#!/bin/sh` stub named `sqruff` in `dir`, as [`stub_sqruff_reporting`]
/// does, whose lint run reports no findings.
fn stub_sqruff(dir: &Path, version: &str) {
    stub_sqruff_reporting(dir, version, NO_FINDINGS);
}

/// Writes an executable `#!/bin/sh` stub named `sqruff` in `dir`: it prints `version` for
/// `--version`; otherwise it refuses (exit 5) a run without `--ignore-local-config` or a
/// `--config` naming a file. A `lint` run, refused too without `--parsing-errors`, appends that
/// config to `lint-seen` and the `--dialect` to `lint-dialects` beside itself and prints
/// `report`; any other run appends them to `seen` and `dialects` and prints the block back
/// unchanged.
fn stub_sqruff_reporting(dir: &Path, version: &str, report: &str) {
    std::fs::write(dir.join("report"), report).expect("the stub's report should be writable");
    let script = format!(
        "#!/bin/sh\n\
         [ \"$1\" = --version ] && {{ echo '{version}'; exit 0; }}\n\
         case \" $* \" in *' --ignore-local-config '*) ;; *) exit 5;; esac\n\
         config=; dialect=; prev=\n\
         for arg in \"$@\"; do\n\
         [ \"$prev\" = --config ] && config=\"$arg\"\n\
         [ \"$prev\" = --dialect ] && dialect=\"$arg\"\n\
         prev=\"$arg\"\n\
         done\n\
         [ -f \"$config\" ] || exit 5\n\
         if [ \"$1\" = lint ]; then\n\
         case \" $* \" in *' --parsing-errors '*) ;; *) exit 5;; esac\n\
         /bin/cat \"$config\" >> \"${{0%/*}}/lint-seen\"\n\
         echo \"$dialect\" >> \"${{0%/*}}/lint-dialects\"\n\
         /bin/cat > /dev/null\n\
         /bin/cat \"${{0%/*}}/report\"\n\
         exit 0\n\
         fi\n\
         /bin/cat \"$config\" >> \"${{0%/*}}/seen\"\n\
         echo \"$dialect\" >> \"${{0%/*}}/dialects\"\n\
         /bin/cat\n"
    );
    let path = dir.join("sqruff");
    std::fs::write(&path, script).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// What every private format config opens with.
const FORCED: &str = "[sqruff]\ntemplater = raw\nrules = layout,capitalisation\n";

/// What every private lint config opens with: no `rules`, so sqruff's own set.
const LINT_FORCED: &str = "[sqruff]\ntemplater = raw\n";

/// Runs `check --json` on `project` with `dir` as `PATH`.
fn check(project: &Project, dir: &Path) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", dir)])
        .json()
}

#[test]
fn sql_a_dialect_tag_overrides_the_setting_and_sql_reads_it() {
    for (table, tag, dialect) in [
        ("", "sql", "ansi"),
        ("dialect = \"mysql\"\n", "sql", "mysql"),
        ("dialect = \"mysql\"\n", "pgsql", "postgres"),
        ("", "postgresql", "postgres"),
        ("dialect = \"bigquery\"\n", "mssql", "tsql"),
        ("", "tsql", "tsql"),
        ("", "duckdb", "duckdb"),
    ] {
        let project = sql_project(table, tag, "SELECT 1;");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff(outside.path(), "sqruff 0.44.0");

        let json = check(&project, outside.path());

        assert!(finding_kinds(&json).is_empty(), "{tag}: {json}");
        let dialects = std::fs::read_to_string(outside.path().join("dialects"))
            .expect("the stub should have run");
        assert_eq!(dialects, format!("{dialect}\n"), "{table:?} {tag}");
        let linted = std::fs::read_to_string(outside.path().join("lint-dialects"))
            .expect("the stub should have linted");
        assert_eq!(linted, format!("{dialect}\n"), "{table:?} {tag}: lint");
    }
}

#[test]
fn sql_with_no_project_config_sqruff_gets_only_raw_layout_and_capitalisation() {
    let project = sql_project("", "sql", "SELECT 1;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_sqruff(outside.path(), "sqruff 0.44.0");

    let json = check(&project, outside.path());

    assert!(finding_kinds(&json).is_empty(), "{json}");
    let seen = std::fs::read_to_string(outside.path().join("seen"))
        .expect("the stub should have seen a config");
    assert_eq!(seen, FORCED);
    let linted = std::fs::read_to_string(outside.path().join("lint-seen"))
        .expect("the stub should have linted with a config");
    assert_eq!(linted, LINT_FORCED);
}

#[test]
fn sql_the_copy_replaces_rules_and_dialect_and_keeps_rule_settings() {
    let project = sql_project("", "sql", "SELECT 1;");
    project.write(
        ".sqruff",
        "[sqruff]\nrules = all\ndialect = tsql\nexclude_rules = LT05\n\
         [sqruff:rules:capitalisation.keywords]\ncapitalisation_policy = lower\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_sqruff(outside.path(), "sqruff 0.44.0");

    let json = check(&project, outside.path());

    assert!(finding_kinds(&json).is_empty(), "{json}");
    let seen = std::fs::read_to_string(outside.path().join("seen"))
        .expect("the stub should have seen a config");
    let kept = "exclude_rules = LT05\n[sqruff:rules:capitalisation.keywords]\n\
                capitalisation_policy = lower\n";
    assert_eq!(seen, format!("{FORCED}{kept}"));
    let linted = std::fs::read_to_string(outside.path().join("lint-seen"))
        .expect("the stub should have linted with a config");
    assert_eq!(linted, format!("{LINT_FORCED}{kept}"));
    let dialects =
        std::fs::read_to_string(outside.path().join("dialects")).expect("the stub should have run");
    assert_eq!(
        dialects, "ansi\n",
        "the config's dialect never reaches sqruff"
    );
}

#[test]
fn sql_a_templater_library_path_cr_or_unsorted_line_leaves_the_blocks_unchecked() {
    for (config, says) in [
        ("[sqruff]\ntemplater = jinja\n", "templater"),
        ("[sqruff:templater:python]\n", "templater"),
        ("[sqruff]\nlibrary_path = macros\n", "read files"),
        (
            "[sqruff]\nexclude_rules = LT05\rtemplater = jinja\n",
            "carriage return",
        ),
        ("[sqruff]\nexclude_rules = LT05,\n  LT01\n", "indented line"),
        ("[sqlfluff]\n", "section supdoc doesn't read"),
        ("[sqruff]\nexclude_rules = core\n", "`exclude_rules`"),
    ] {
        let project = sql_project("", "sql", "SELECT 1;");
        project.write(".sqruff", config);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff(outside.path(), "sqruff 0.44.0");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{config:?}: {json}"
        );
        let message = at(&json, "/findings/0/message")
            .as_str()
            .unwrap_or_default();
        assert!(
            message.contains("`.sqruff`") && message.contains(says),
            "{config:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/help")
                .as_str()
                .is_some_and(|help| help.contains("`.sqruff`")),
            "{config:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/not_checked"),
            1,
            "{config:?}: {json}"
        );
        for ran in ["seen", "lint-seen"] {
            assert!(
                !outside.path().join(ran).exists(),
                "{config:?}: sqruff ran ({ran})"
            );
        }
    }
}

#[test]
fn sql_only_a_verified_sqruff_version_runs_the_sqruff_steps() {
    for (version, runs) in [
        ("sqruff 0.44.3", true),
        ("sqruff 0.45.0", false),
        ("sqruff 1.44.0", false),
        ("garbage", false),
    ] {
        let project = sql_project("", "sql", "SELECT 1;");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff(outside.path(), version);

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        if runs {
            assert!(finding_kinds(&json).is_empty(), "{version}: {json}");
            assert!(outside.path().join("seen").exists(), "{version}: no run");
            assert!(
                outside.path().join("lint-seen").exists(),
                "{version}: no lint run"
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
                        .contains("isn't a version supdoc's checks are verified against (0.44)")),
                "{version}: {json}"
            );
            for ran in ["seen", "lint-seen"] {
                assert!(
                    !outside.path().join(ran).exists(),
                    "{version}: a sqruff step ran ({ran})"
                );
            }
        }
    }
}

/// Asserts `json` holds one `code-block-not-checked` warning for an inline sqruff directive on
/// doc line `line`, and the block not checked.
fn assert_directive_skipped(json: &serde_json::Value, line: u32, case: &str) {
    assert_eq!(
        finding_kinds(json),
        ["code-block-not-checked"],
        "{case}: {json}"
    );
    assert_eq!(
        at(json, "/findings/0/location/line"),
        line,
        "{case}: {json}"
    );
    assert!(
        at(json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains(&format!("line {line} "))
                && message.contains("inline sqruff directive")),
        "{case}: {json}"
    );
    assert_eq!(
        at(json, "/data/summary/code_blocks/not_checked"),
        1,
        "{case}: {json}"
    );
}

#[test]
fn sql_a_block_holding_an_inline_directive_is_never_given_to_sqruff() {
    for directive in [
        "-- sqruff:rules:all",
        "-- sqlfluff:dialect:tsql",
        "   -- sqruff:templater:jinja",
        "--SQRUFF : rules:all",
        "SELECT 2; -- sqlfluff:rules:all",
    ] {
        let project = sql_project("", "sql", &format!("SELECT 1;\n{directive}"));
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff(outside.path(), "sqruff 0.44.0");

        let json = check(&project, outside.path());

        assert_directive_skipped(&json, 5, directive);
        for ran in ["seen", "lint-seen"] {
            assert!(
                !outside.path().join(ran).exists(),
                "{directive}: sqruff was given the block ({ran})"
            );
        }
    }
}

#[test]
fn sql_a_directive_keeps_the_block_from_sqruff_lint_whatever_formats_it() {
    for (table, vars) in [
        (
            "formatter = { command = [\"/bin/cat\"] }\n",
            vec![("SUPDOC_TRUST", Path::new("1"))],
        ),
        (
            "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
            vec![],
        ),
    ] {
        let project = sql_project(table, "sql", "SELECT 1;\n-- sqruff:rules:all");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff(outside.path(), "sqruff 0.44.0");
        let mut vars = vars;
        vars.push(("PATH", outside.path()));

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &vars)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{table}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("line 5 ")
                    && message.contains("its lint step didn't run")),
            "{table}: {json}"
        );
        assert!(
            !outside.path().join("lint-seen").exists(),
            "{table}: sqruff lint was given the block"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/lint_not_run"),
            1,
            "{table}: {json}"
        );
    }
}

#[test]
fn sql_a_noqa_supdoc_cant_confirm_names_only_rule_codes_leaves_both_sqruff_steps_not_run() {
    for body in [
        "SELECT 1; -- noqa",
        "SELECT a, -- noqa\n  b FROM t;",
        "SELECT a, -- noqa\n  b FROM t; /* noqa */",
        "SELECT 1; /* noqa: LT01 */",
        "SELECT 1; -- x\r-- noqa: disable=all\nSELECT  b  FROM t;",
        "SELECT '/*' AS x;\nSELECT */* noqa: disable=LT01\n, all */\nFROM t;\nSELECT  a  FROM t;",
        "-- noqa: disable=all\nSELECT 1;",
        "SELECT 1; -- NOQA: All",
        "SELECT 1; -- noqa: core",
        "/*\nnoqa: disable=all\n*/\nSELECT 1;",
        "/* noqa: disable=LT01\n, all */\nSELECT 1;",
        "SELECT 1;\n/* noqa",
        "select a FROM t WHERE b = -- noqa\r    :LT01",
    ] {
        let project = sql_project("", "sql", body);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff(outside.path(), "sqruff 0.44.0");

        let json = check(&project, outside.path());

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message
                    .contains("holds a `noqa` comment supdoc can't confirm names only rule codes")
                    && message.contains("its sqruff steps didn't run")),
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/lint_not_run"),
            1,
            "{body:?}: {json}"
        );
        assert!(
            !outside.path().join("lint-seen").exists(),
            "{body:?}: sqruff lint ran"
        );
        assert!(
            !outside.path().join("seen").exists(),
            "{body:?}: sqruff fix ran, and it obeys noqa too"
        );
    }
}

#[test]
fn sql_noqa_naming_only_rule_codes_still_lints_the_block() {
    for body in [
        "SELECT 1; -- noqa: LT01",
        "-- noqa: disable=LT01\nSELECT 1;",
        "/* plain comment */\nSELECT 1;",
    ] {
        let project = sql_project("", "sql", body);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff(outside.path(), "sqruff 0.44.0");

        let json = check(&project, outside.path());

        assert!(finding_kinds(&json).is_empty(), "{body:?}: {json}");
        assert!(
            outside.path().join("lint-seen").exists(),
            "{body:?}: sqruff lint didn't run"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{body:?}: {json}"
        );
    }
}

#[test]
fn sql_a_lint_report_supdoc_cant_read_is_a_tool_failure() {
    for report in ["", "not json", "[]", "{\"x.sql\":[]}"] {
        let project = sql_project("", "sql", "SELECT 1;");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_sqruff_reporting(outside.path(), "sqruff 0.44.0", report);

        let json = check(&project, outside.path());

        assert_eq!(
            finding_kinds(&json),
            ["code-block-tool-failed"],
            "{report:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{report:?}: {json}"
        );
    }
}

#[test]
fn sql_a_lint_finding_fails_the_block_at_its_doc_line() {
    let report = "{\"<string>\":[{\"range\":{\"start\":{\"line\":2,\"character\":8},\
                  \"end\":{\"line\":2,\"character\":8}},\"message\":\"Alias 'X' is never used.\",\
                  \"severity\":\"Warning\",\"source\":\"sqruff\",\"code\":\"AL05\"}]}";
    let project = sql_project("", "sql", "SELECT a\nFROM t x;");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_sqruff_reporting(outside.path(), "sqruff 0.44.0", report);

    let json = check(&project, outside.path());

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("AL05")),
        "{json}"
    );
}

/// The real sqruff's directory, or `None` (the test is skipped) when it isn't on `PATH`.
fn real_sqruff(test: &str) -> Option<std::path::PathBuf> {
    real_tool_dir("sqruff", test)
}

#[test]
fn sql_the_real_sqruff_fails_an_unformatted_block_and_fix_rewrites_it() {
    let Some(dir) =
        real_sqruff("sql_the_real_sqruff_fails_an_unformatted_block_and_fix_rewrites_it")
    else {
        return;
    };
    let project = sql_project("", "sql", "select a,b from t where x=1");

    let json = check(&project, &dir);
    let kinds = finding_kinds(&json);
    assert_eq!(
        kinds.first().map(String::as_str),
        Some("code-block-unformatted"),
        "{json}"
    );
    assert!(
        kinds.iter().skip(1).all(|kind| kind == "code-block-lint"),
        "sqruff lint flags the spacing too: {json}"
    );

    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```sql\nselect\n    a,\n    b\nfrom t\nwhere x = 1\n```\n"
    );
    let again = check(&project, &dir);
    assert!(finding_kinds(&again).is_empty(), "{again}");
}

#[test]
fn sql_the_real_sqruff_passes_a_block_under_one_dialect_and_fails_it_under_another() {
    let Some(dir) = real_sqruff(
        "sql_the_real_sqruff_passes_a_block_under_one_dialect_and_fails_it_under_another",
    ) else {
        return;
    };
    let top = "SELECT TOP 5 * FROM t;";

    let ansi = check(&sql_project("", "sql", top), &dir);
    let setting = check(&sql_project("dialect = \"tsql\"\n", "sql", top), &dir);
    let tagged = check(&sql_project("", "tsql", top), &dir);
    let tag_beats_setting = check(&sql_project("dialect = \"tsql\"\n", "postgres", top), &dir);

    assert_eq!(finding_kinds(&ansi), ["code-block-parse"], "{ansi}");
    assert_eq!(at(&ansi, "/findings/0/location/line"), 4, "{ansi}");
    assert!(finding_kinds(&setting).is_empty(), "{setting}");
    assert!(finding_kinds(&tagged).is_empty(), "{tagged}");
    assert_eq!(
        finding_kinds(&tag_beats_setting),
        ["code-block-parse"],
        "{tag_beats_setting}"
    );
}

#[test]
fn sql_the_real_sqruff_never_changes_a_block_through_a_rule_outside_layout_and_capitalisation() {
    let Some(dir) = real_sqruff(
        "sql_the_real_sqruff_never_changes_a_block_through_a_rule_outside_layout_and_capitalisation",
    ) else {
        return;
    };
    // An unused alias: sqruff's aliasing rules would drop `x`, which isn't formatting.
    let project = sql_project("", "sql", "SELECT a FROM t x;");
    project.write(".sqruff", "[sqruff]\nrules = all\n");
    let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    std::fs::write(home.path().join(".sqruff"), "[sqruff]\nrules = all\n")
        .expect("the home config should be writable");

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", &dir), ("HOME", home.path())],
        )
        .json();

    // sqruff lint flags the unused alias (a rule outside formatting), but fix never drops it.
    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert!(json.pointer("/findings/0/fix").is_none(), "{json}");
}

#[test]
fn sql_the_real_sqruff_formats_a_block_whose_only_violation_it_cant_fix_and_lint_flags_it() {
    let Some(dir) = real_sqruff(
        "sql_the_real_sqruff_formats_a_block_whose_only_violation_it_cant_fix_and_lint_flags_it",
    ) else {
        return;
    };
    // fix exits 0 and prints the block unchanged, still reporting the long line on stderr; lint
    // reports it.
    let project = sql_project(
        "",
        "sql",
        "SELECT a\nFROM t;\n-- this is a very long trailing comment that goes on and on and on \
         past the eighty column limit",
    );

    let json = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("LT05")),
        "{json}"
    );
}

#[test]
fn sql_the_real_sqruff_never_sees_a_block_whose_directive_turns_every_rule_on() {
    let Some(dir) =
        real_sqruff("sql_the_real_sqruff_never_sees_a_block_whose_directive_turns_every_rule_on")
    else {
        return;
    };
    // Given to sqruff, the directive would turn the aliasing rules on and fix would add `as`.
    let doc = "# Blocks\n\n```sql\n-- sqruff:rules:all\nselect a.x from tbl a\n```\n";
    let project = sql_project("", "sql", "-- sqruff:rules:all\nselect a.x from tbl a");

    let json = check(&project, &dir);
    assert_directive_skipped(&json, 4, "rules:all");
    assert!(json.pointer("/findings/0/fix").is_none(), "{json}");

    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    assert_eq!(project.read("docs/blocks.md"), doc);
}

#[test]
fn sql_the_real_sqruff_never_sees_a_block_whose_directive_changes_the_dialect() {
    let Some(dir) =
        real_sqruff("sql_the_real_sqruff_never_sees_a_block_whose_directive_changes_the_dialect")
    else {
        return;
    };
    // Given to sqruff, the directive would beat the tag and pass the block as T-SQL.
    let project = sql_project(
        "",
        "postgres",
        "-- sqruff:dialect:tsql\nSELECT TOP 5 * FROM t;",
    );

    let json = check(&project, &dir);

    assert_directive_skipped(&json, 4, "dialect:tsql");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
}

#[test]
fn sql_the_real_sqruff_lint_fails_a_finding_at_its_doc_line() {
    let Some(dir) = real_sqruff("sql_the_real_sqruff_lint_fails_a_finding_at_its_doc_line") else {
        return;
    };
    let project = sql_project("", "sql", "SELECT a\nFROM t x;");

    let json = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("AL05")),
        "{json}"
    );
}

#[test]
fn sql_the_real_sqruff_lint_reports_a_block_it_cant_parse() {
    let Some(dir) = real_sqruff("sql_the_real_sqruff_lint_reports_a_block_it_cant_parse") else {
        return;
    };
    // With the format step off, only lint's `--parsing-errors` catches the T-SQL under ansi; an
    // unframed block's syntax error is a lint finding, as hadolint's is.
    let project = sql_project(
        "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
        "sql",
        "SELECT TOP 5 * FROM t;",
    );

    let json = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("Unparsable section")),
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/failed"), 1, "{json}");
}

#[test]
fn sql_the_real_sqruff_never_passes_a_block_noqa_switches_off() {
    let Some(dir) = real_sqruff("sql_the_real_sqruff_never_passes_a_block_noqa_switches_off")
    else {
        return;
    };
    // Linted, the bare noqa would hide the unparsable T-SQL and the block would pass.
    let project = sql_project(
        "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
        "sql",
        "SELECT TOP 5 * FROM t; -- noqa",
    );

    let json = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
}

#[test]
fn sql_the_real_sqruff_never_passes_a_block_whose_noqa_names_more_than_rule_codes() {
    let Some(dir) = real_sqruff(
        "sql_the_real_sqruff_never_passes_a_block_whose_noqa_names_more_than_rule_codes",
    ) else {
        return;
    };
    // Each list is a pattern sqruff matches against every rule, so linted the block would pass.
    for body in [
        "SELECT a  FROM t; -- noqa: *",
        "SELECT a  FROM t; -- noqa: ????",
        "SELECT a  FROM t; -- noqa: [A-Z]*",
        "SELECT a  FROM t; -- noqa: core",
        "-- noqa: disable=*\nSELECT a  FROM t;",
        "-- noqa: disable=core\nSELECT a  FROM t;",
    ] {
        let project = sql_project("", "sql", body);

        let json = check(&project, &dir);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/lint_not_run"),
            1,
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{body:?}: {json}"
        );
    }
}

#[test]
fn sql_the_real_sqruff_never_sees_a_block_whose_block_comment_holds_noqa() {
    let Some(dir) =
        real_sqruff("sql_the_real_sqruff_never_sees_a_block_whose_block_comment_holds_noqa")
    else {
        return;
    };
    // sqruff reads each block comment as one directive, its list continued across lines, so each
    // switches every rule off and, linted and fixed, the badly spaced block would pass.
    for body in [
        "/*\nnoqa: disable=all\n*/\nSELECT  a  FROM t",
        "/* noqa: disable=LT01\n, all */\nSELECT  a  FROM t",
        "SELECT  a  FROM t /* noqa: LT01\n, core */",
        "SELECT  a  FROM t;\n/* noqa",
    ] {
        let project = sql_project("", "sql", body);

        let json = check(&project, &dir);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/lint_not_run"),
            1,
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/format_not_run"),
            1,
            "{body:?}: sqruff fix ran, and it obeys the comment too: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{body:?}: {json}"
        );
    }
}

#[test]
fn sql_the_real_sqruff_still_lints_a_block_whose_block_comment_holds_no_noqa() {
    let Some(dir) =
        real_sqruff("sql_the_real_sqruff_still_lints_a_block_whose_block_comment_holds_no_noqa")
    else {
        return;
    };
    let project = sql_project(
        "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
        "sql",
        "/* plain comment\n-- with a dash comment inside */\nSELECT a  FROM t;",
    );

    let json = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/failed"), 1, "{json}");
}

#[test]
fn sql_the_real_sqruff_still_honors_a_noqa_naming_rule_codes() {
    let Some(dir) = real_sqruff("sql_the_real_sqruff_still_honors_a_noqa_naming_rule_codes") else {
        return;
    };
    // The block is linted twice, the second time with every noqa off for parse errors alone, so
    // the LT01 finding the comment names stays hidden.
    let project = sql_project(
        "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
        "sql",
        "SELECT a  FROM t; -- noqa: LT01",
    );

    let json = check(&project, &dir);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn sql_the_real_sqruff_never_passes_a_parse_error_behind_a_bare_noqa() {
    let Some(dir) =
        real_sqruff("sql_the_real_sqruff_never_passes_a_parse_error_behind_a_bare_noqa")
    else {
        return;
    };
    // Linted, the bare noqa would hide the unfinished WHERE and the block would pass.
    for table in [
        "",
        "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
    ] {
        let project = sql_project(table, "sql", "SELECT 1;\nSELECT a FROM t WHERE -- noqa");

        let json = check(&project, &dir);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{table:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/lint_not_run"),
            1,
            "{table:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{table:?}: {json}"
        );
    }
}

#[test]
fn sql_the_real_sqruff_fails_a_parse_error_behind_a_plain_code_noqa() {
    let Some(dir) = real_sqruff("sql_the_real_sqruff_fails_a_parse_error_behind_a_plain_code_noqa")
    else {
        return;
    };
    // A noqa naming rule codes is honored, but no rule code masks a parse error: `sqruff fix`
    // reports it first, and with the format step off lint's `--parsing-errors` does.
    for (table, kind) in [
        ("", "code-block-parse"),
        (
            "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
            "code-block-lint",
        ),
    ] {
        let project = sql_project(
            table,
            "sql",
            "SELECT 1;\nSELECT a FROM t WHERE -- noqa: LT01",
        );

        let json = check(&project, &dir);

        let kinds = finding_kinds(&json);
        assert!(
            !kinds.is_empty() && kinds.iter().all(|found| found == kind),
            "{table:?}: {json}"
        );
        assert!(
            array_at(&json, "/findings").iter().any(|finding| finding
                .pointer("/message")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|message| message.contains("Unparsable section"))
                && finding
                    .pointer("/location/line")
                    .and_then(serde_json::Value::as_u64)
                    == Some(5)),
            "{table:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{table:?}: {json}"
        );
    }
}

#[test]
fn sql_the_real_sqruff_never_sees_a_string_hidden_block_comment_or_a_cr_joined_noqa() {
    let Some(dir) = real_sqruff(
        "sql_the_real_sqruff_never_sees_a_string_hidden_block_comment_or_a_cr_joined_noqa",
    ) else {
        return;
    };
    // sqruff honors each as every rule off, so linted and fixed the badly spaced block would pass.
    for body in [
        "SELECT '/*' AS x;\nSELECT */* noqa: disable=LT01\n, all */\nFROM t;\nSELECT  a  FROM t;",
        "SELECT 1; -- x\r-- noqa: disable=all\nSELECT  b  FROM t;",
    ] {
        let project = sql_project("", "sql", body);

        let json = check(&project, &dir);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/lint_not_run"),
            1,
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/format_not_run"),
            1,
            "{body:?}: sqruff fix ran, and it obeys the comment too: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{body:?}: {json}"
        );
    }
}

#[test]
fn sql_the_real_sqruff_never_sees_a_noqa_block_holding_a_carriage_return_inside_a_line() {
    let Some(dir) = real_sqruff(
        "sql_the_real_sqruff_never_sees_a_noqa_block_holding_a_carriage_return_inside_a_line",
    ) else {
        return;
    };
    // sqruff ends the comment at the lone CR, so it reads a bare `noqa` that masks the badly
    // spaced line, a syntax error's too.
    let project = sql_project(
        "",
        "postgres",
        "SELECT  a  FROM t;\nselect a FROM t WHERE b = -- noqa\r    :LT01\nSELECT  b  FROM t;",
    );

    let json = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_not_run"),
        1,
        "sqruff fix ran, and it obeys the comment too: {json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
}

#[test]
fn sql_the_real_sqruff_still_lints_a_plain_code_noqa_in_a_crlf_doc() {
    let Some(dir) = real_sqruff("sql_the_real_sqruff_still_lints_a_plain_code_noqa_in_a_crlf_doc")
    else {
        return;
    };
    // The block's line-end CRs are gone before the noqa scan, so only a CR inside a line withholds
    // a block; if this one is withheld, CRLF reached the scan.
    let project = sql_project(
        "[features.code-blocks.sql.sqruff-format]\nformat = \"off\"\n",
        "sql",
        "SELECT a  FROM t; -- noqa: LT01",
    );
    project.write(
        "docs/blocks.md",
        "# Blocks\r\n\r\n```sql\r\nSELECT a  FROM t; -- noqa: LT01\r\n```\r\n",
    );

    let json = check(&project, &dir);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        0,
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn sql_the_real_sqruff_never_runs_on_an_exclude_rules_naming_more_than_rule_codes() {
    let Some(dir) = real_sqruff(
        "sql_the_real_sqruff_never_runs_on_an_exclude_rules_naming_more_than_rule_codes",
    ) else {
        return;
    };
    // sqruff expands `core` and `all` here, so lint and format would pass anything.
    for value in ["core", "all", "LT05, *"] {
        let project = sql_project("", "sql", "select a,b from t where x=1");
        project.write(".sqruff", &format!("[sqruff]\nexclude_rules = {value}\n"));

        let json = check(&project, &dir);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{value:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("`exclude_rules`")),
            "{value:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/not_checked"),
            1,
            "{value:?}: {json}"
        );
    }
}
