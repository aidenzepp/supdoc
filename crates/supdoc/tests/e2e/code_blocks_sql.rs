//! Checking SQL code blocks, tagged `sql` or a dialect's name, formatted by `sqruff fix` under a
//! private config that keeps only the layout and capitalisation rules.

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

/// Writes an executable `#!/bin/sh` stub named `sqruff` in `dir`: it prints `version` for
/// `--version`; otherwise it refuses (exit 5) a run without `--ignore-local-config` or a
/// `--config` naming a file, appends that config to `seen` and the `--dialect` to `dialects`
/// beside itself, and prints the block back unchanged.
fn stub_sqruff(dir: &Path, version: &str) {
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
         /bin/cat \"$config\" >> \"${{0%/*}}/seen\"\n\
         echo \"$dialect\" >> \"${{0%/*}}/dialects\"\n\
         /bin/cat\n"
    );
    let path = dir.join("sqruff");
    std::fs::write(&path, script).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// What every private config opens with.
const FORCED: &str = "[sqruff]\ntemplater = raw\nrules = layout,capitalisation\n";

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
    assert_eq!(
        seen,
        format!(
            "{FORCED}exclude_rules = LT05\n[sqruff:rules:capitalisation.keywords]\n\
             capitalisation_policy = lower\n"
        )
    );
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
        assert!(
            !outside.path().join("seen").exists(),
            "{config:?}: sqruff ran"
        );
    }
}

#[test]
fn sql_only_a_verified_sqruff_version_runs_the_format_step() {
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
            assert!(
                !outside.path().join("seen").exists(),
                "{version}: the format step ran"
            );
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
        assert!(
            !outside.path().join("seen").exists(),
            "{directive}: sqruff was given the block"
        );
    }
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
    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");

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

    assert!(finding_kinds(&json).is_empty(), "{json}");
}

#[test]
fn sql_the_real_sqruff_passes_a_block_whose_only_violation_it_cant_fix() {
    let Some(dir) =
        real_sqruff("sql_the_real_sqruff_passes_a_block_whose_only_violation_it_cant_fix")
    else {
        return;
    };
    // sqruff exits 0 and prints the block unchanged, still reporting the long line on stderr.
    let project = sql_project(
        "",
        "sql",
        "SELECT a\nFROM t;\n-- this is a very long trailing comment that goes on and on and on \
         past the eighty column limit",
    );

    let json = check(&project, &dir);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
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
