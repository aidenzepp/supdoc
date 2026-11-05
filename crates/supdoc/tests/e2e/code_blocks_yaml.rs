//! Checking YAML code blocks with yamlfmt.

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

/// A recorded `basic` project with the yaml table and `docs/blocks.md` holding `blocks`.
fn project_with_blocks(blocks: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.yaml]\n",
    );
    project.write("docs/blocks.md", &format!("# Blocks\n\n{blocks}\n"));
    project
}

/// A recorded project with one `yaml` block holding `body`, which starts on doc line 4.
fn project_with_block(body: &str) -> Project {
    project_with_blocks(&format!("```yaml\n{body}\n```"))
}

/// Writes an executable `#!/bin/sh` stub `yamlfmt` running `script` in `dir`.
fn yamlfmt_stub(dir: &Path, script: &str) {
    let path = dir.join("yamlfmt");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A stub script that checks yamlfmt's arguments are `-in -no_global_conf -conf <file>`, that the
/// file is outside the project and holds `config`, then echoes its input.
fn echo_checking_config(config: &str) -> String {
    format!(
        "[ \"$1 $2 $3\" = '-in -no_global_conf -conf' ] || exit 4\n\
         [ $# -eq 4 ] || exit 5\n\
         case \"$4\" in */yamlfmt-config/*/.yamlfmt) ;; *) exit 6;; esac\n\
         [ \"$(/bin/cat \"$4\")\" = \"$(printf '{config}')\" ] || exit 7\n\
         /bin/cat"
    )
}

#[test]
fn the_block_goes_to_yamlfmt_on_stdin_with_an_empty_private_config_when_the_project_has_none() {
    let project = project_with_block("a: 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    yamlfmt_stub(outside.path(), &echo_checking_config(""));

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/code_blocks/0/language"), "yaml", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "ran", "{json}");
}

#[test]
fn a_yml_tag_is_yaml_too() {
    let project = project_with_blocks("```yml\na: 1\n```");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    yamlfmt_stub(outside.path(), &echo_checking_config(""));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn yamlfmt_gets_the_configs_checked_settings_in_a_private_file_not_the_projects_bytes() {
    let project = project_with_block("a: 1");
    project.write(".yamlfmt", "bogus: 1\n");
    project.write(
        "docs/.yamlfmt.yaml",
        "# the project's comment\nexclude:\n  - '**/*.md'\ngitignore_path: ../../secret\nformatter:\n  indent: 4\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    yamlfmt_stub(
        outside.path(),
        &echo_checking_config("formatter:\\n  indent: 4"),
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn a_config_supdoc_cant_read_leaves_the_block_not_checked_with_one_warning() {
    let project = project_with_blocks("```yaml\na: 1\n```\n\n```yaml\nb: 2\n```");
    project.write(".yamlfmt", "formatter:\n  type: basic\n  bogus: true\n");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    yamlfmt_stub(outside.path(), "exit 9");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/1/state"),
        "not_checked",
        "{json}"
    );
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason
            .as_str()
            .is_some_and(|reason| reason.contains(".yamlfmt") && reason.contains("formatter.bogus")),
        "{json}"
    );
}

#[test]
fn two_configs_in_one_directory_leave_the_block_not_checked() {
    let project = project_with_block("a: 1");
    project.write(".yamlfmt", "");
    project.write("yamlfmt.yml", "");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    yamlfmt_stub(outside.path(), "exit 9");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("at random")),
        "{json}"
    );
}

#[test]
fn output_of_only_whitespace_is_not_run_with_a_warning_never_a_fix() {
    let project = project_with_block("a: 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    yamlfmt_stub(outside.path(), "/bin/cat > /dev/null; printf '\\n'");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/state"),
        "not_checked",
        "{json}"
    );
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
}

#[test]
fn a_yaml_syntax_error_is_code_block_parse_at_its_doc_line() {
    let project = project_with_block("a: 1\nb: [\nc: 2");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    yamlfmt_stub(
        outside.path(),
        "/bin/cat > /dev/null; echo \"yaml: line 2: did not find expected node content\" >&2; exit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

/// A real-yamlfmt `check --json` of `project` with `PATH` at `dir` and `vars`, expecting `exit`.
fn real_run(project: &Project, dir: &Path, vars: &[(&str, &Path)], exit: i32) -> serde_json::Value {
    let mut all = vec![("PATH", dir)];
    all.extend_from_slice(vars);
    project
        .supdoc_in_with(".", &["check", "--json"], &all)
        .assert_exit(exit)
        .json()
}

#[test]
fn the_real_yamlfmt_passes_a_clean_block_and_fails_a_dirty_one() {
    let Some(dir) = real_tool_dir(
        "yamlfmt",
        "the_real_yamlfmt_passes_a_clean_block_and_fails_a_dirty_one",
    ) else {
        return;
    };
    let clean = project_with_block("a:\n  b: 1\nc: [x, y]");
    let dirty = project_with_block("a:\n      b: 1");

    let passes = real_run(&clean, &dir, &[], 0);
    let fails = real_run(&dirty, &dir, &[], 1);

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
    assert_eq!(
        at(&fails, "/findings/0/fix/replacement").as_str(),
        Some("a:\n  b: 1"),
        "{fails}"
    );
}

#[test]
fn the_real_yamlfmt_never_applies_a_config_in_the_home_directory_or_above_the_cache() {
    let Some(dir) = real_tool_dir(
        "yamlfmt",
        "the_real_yamlfmt_never_applies_a_config_in_the_home_directory_or_above_the_cache",
    ) else {
        return;
    };
    let scratch = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let planted = "formatter:\n  indent: 8\n";
    for config in [".config/yamlfmt/.yamlfmt", ".yamlfmt", "cache/.yamlfmt"] {
        let path = scratch.path().join(config);
        std::fs::create_dir_all(path.parent().expect("a parent"))
            .expect("the planted dir should be creatable");
        std::fs::write(path, planted).expect("the planted config should be writable");
    }
    let cache = scratch.path().join("cache");
    let project = project_with_block("a:\n  b: 1");

    let json = real_run(
        &project,
        &dir,
        &[
            ("HOME", scratch.path()),
            ("XDG_CONFIG_HOME", &scratch.path().join(".config")),
            ("XDG_CACHE_HOME", &cache),
        ],
        0,
    );

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_real_yamlfmt_applies_the_projects_settings_and_never_its_exclude() {
    let Some(dir) = real_tool_dir(
        "yamlfmt",
        "the_real_yamlfmt_applies_the_projects_settings_and_never_its_exclude",
    ) else {
        return;
    };
    let project = project_with_block("a:\n    b: 1");
    project.write(
        "docs/yamlfmt.yaml",
        "exclude: ['**']\nregex_exclude: ['.*']\nformatter:\n  indent: 4\n",
    );
    let skipped = project_with_block("a:\n      b: 1");
    skipped.write(".yamlfmt", "exclude: ['**']\nregex_exclude: ['.*']\n");

    let passes = real_run(&project, &dir, &[], 0);
    let fails = real_run(&skipped, &dir, &[], 1);

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert_eq!(finding_kinds(&fails), ["code-block-unformatted"], "{fails}");
}

#[test]
fn the_real_yamlfmt_still_formats_a_block_marked_yamlfmt_ignore() {
    let Some(dir) = real_tool_dir(
        "yamlfmt",
        "the_real_yamlfmt_still_formats_a_block_marked_yamlfmt_ignore",
    ) else {
        return;
    };
    let project = project_with_block("# !yamlfmt!:ignore\na:\n      b: 1");

    let json = real_run(&project, &dir, &[], 1);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
}

#[test]
fn the_real_yamlfmts_rewrite_of_a_placeholder_is_never_a_fix() {
    let Some(dir) = real_tool_dir(
        "yamlfmt",
        "the_real_yamlfmts_rewrite_of_a_placeholder_is_never_a_fix",
    ) else {
        return;
    };
    let project = project_with_block("{<n>, ...}");

    let json = real_run(&project, &dir, &[], 1);

    assert_eq!(
        finding_kinds(&json),
        ["code-block-unformatted", "code-block-fix-unsafe"],
        "{json}"
    );
    assert!(json.pointer("/findings/0/fix").is_none(), "{json}");
}

#[test]
fn the_real_yamlfmts_syntax_error_is_code_block_parse_at_its_doc_line() {
    let Some(dir) = real_tool_dir(
        "yamlfmt",
        "the_real_yamlfmts_syntax_error_is_code_block_parse_at_its_doc_line",
    ) else {
        return;
    };
    // yamlfmt names the tab's line, the block's third: doc line 6.
    let project = project_with_block("a: 1\nb:\n\tc: 2");

    let json = real_run(&project, &dir, &[], 1);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    let message = at(&json, "/findings/0/message");
    assert!(
        message.as_str().is_some_and(
            |message| !message.contains('\u{1b}') && !message.contains("yamlfmt-config")
        ),
        "{json}"
    );
}
