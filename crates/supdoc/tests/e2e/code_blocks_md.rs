//! Checking Markdown code blocks.

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

/// A recorded `basic` project with the markdown table (and `config` after it) and `docs/blocks.md`
/// holding `blocks`.
fn project_with_blocks(config: &str, blocks: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &format!("[features.code-blocks]\n[features.code-blocks.markdown]\n{config}"),
    );
    project.write("docs/blocks.md", &format!("# Blocks\n\n{blocks}\n"));
    project
}

/// A recorded project with one `markdown` block holding `body`.
fn project_with_block(config: &str, body: &str) -> Project {
    project_with_blocks(config, &format!("```markdown\n{body}\n```"))
}

/// Writes an executable `#!/bin/sh` stub named `name` running `script` in `dir`.
fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A `rumdl` stub that formats by echoing its input and lints by running `check`, a script that
/// sees rumdl's `check` arguments as `$@`.
fn rumdl_stub(dir: &Path, check: &str) {
    stub(
        dir,
        "rumdl",
        &format!("if [ \"$1\" = fmt ]; then /bin/cat; exit 0; fi\nshift\n{check}"),
    );
}

#[test]
fn a_nested_fence_inside_a_four_backtick_block_is_one_block() {
    let project = project_with_blocks(
        "",
        "````markdown\n# Example\n\n```rust\nfn main() {}\n```\n````",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(outside.path(), "echo '[]'");

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(array_at(&json, "/data/code_blocks").len(), 1, "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/language"),
        "markdown",
        "{json}"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_block_goes_to_rumdl_on_stdin_under_its_virtual_path_with_no_config() {
    let project = project_with_block("", "text");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(
        outside.path(),
        "case \"$*\" in\n\
         \"--no-cache --no-exclude --output-format=json --extend-disable=MD041,MD051,MD057 --no-config --stdin --stdin-filename \"*/docs/blocks.md) echo '[]'; exit 0;;\n\
         esac\nexit 3",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/lint"), "ran", "{json}");
}

#[test]
fn an_unformatted_block_is_reported_with_rumdl_named() {
    let project = project_with_block(
        "[features.code-blocks.markdown.rumdl]\nlint = \"off\"\n",
        "* a\n\n\n* b",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(
        outside.path(),
        "rumdl",
        "case \"$*\" in\n\
         \"fmt --no-cache --no-exclude --extend-disable=MD041,MD051,MD057 --no-config --stdin-filename \"*\"/docs/blocks.md -\") /bin/cat > /dev/null; printf '* a\\n\\n* b\\n';;\n\
         *) exit 3;;\nesac",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        at(&json, "/data/code_blocks/0/reason"),
        "`rumdl-format` would format it differently",
        "{json}"
    );
}

#[test]
fn lint_diagnostics_from_rumdl_are_lint_errors_at_their_doc_line_and_column() {
    let project = project_with_block("", "# T\n\n<div>x</div>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(
        outside.path(),
        "echo '[{\"file\":\"x\",\"line\":3,\"column\":1,\"rule\":\"MD033\",\"message\":\"Inline HTML found: <div>\"}]'\nexit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/state"), "failed", "{json}");
}

#[test]
fn rumdl_gets_the_validated_config_in_a_private_file_not_the_projects_bytes() {
    let project = project_with_block("", "text");
    project.write(
        "docs/rumdl.toml",
        "# the project's comment\n[global]\nline-length = 20\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    rumdl_stub(
        outside.path(),
        "for arg in \"$@\"; do case \"$arg\" in --config=*) cfg=\"${arg#--config=}\";; esac; done\n\
         case \"$cfg\" in */docs/rumdl.toml|'') exit 4;; esac\n\
         [ \"$(/bin/cat \"$cfg\")\" = \"$(printf '[global]\\nline-length = 20')\" ] || exit 5\n\
         echo '[]'",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn a_config_with_code_block_tools_makes_the_block_not_checked() {
    let project = project_with_block("", "text");
    project.write(
        ".rumdl.toml",
        "[code-block-tools]\nenabled = true\n\n[code-block-tools.tools.x]\ncommand = [\"sh\"]\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub(outside.path(), "rumdl", "exit 9");

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
    assert!(
        at(&json, "/data/code_blocks/0/reason")
            .as_str()
            .is_some_and(|reason| reason.contains(".rumdl.toml")),
        "{json}"
    );
}

#[test]
fn the_real_rumdl_passes_a_clean_block_and_fails_a_dirty_one() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_passes_a_clean_block_and_fails_a_dirty_one",
    ) else {
        return;
    };
    let clean = project_with_block("", "## Sub\n\ntext");
    let dirty = project_with_block("", "## Sub\n\n\n\ntext   ");

    let passes = clean
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let fails = dirty
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&passes, "/data/summary/code_blocks/passed"),
        1,
        "{passes}"
    );
    assert!(
        finding_kinds(&fails).contains(&"code-block-unformatted".to_owned()),
        "{fails}"
    );
}

#[test]
fn the_real_rumdl_still_checks_a_block_the_projects_exclude_would_skip() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_still_checks_a_block_the_projects_exclude_would_skip",
    ) else {
        return;
    };
    let project = project_with_block("", "## Sub\n\n\n\ntext   ");
    project.write(".rumdl.toml", "[global]\nexclude = [\"*.md\"]\n");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert!(
        finding_kinds(&json).contains(&"code-block-unformatted".to_owned()),
        "{json}"
    );
}

#[test]
fn the_real_rumdl_ignores_whether_a_links_target_exists() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_ignores_whether_a_links_target_exists",
    ) else {
        return;
    };
    let project = project_with_block("", "## Sub\n\nsee [guide](zz.md) and [part](#nope)");
    let path = [("PATH", dir.as_path())];

    let missing = project
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(0)
        .json();
    project.write("docs/zz.md", "# Zz\n");
    let present = project
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(0)
        .json();

    assert_eq!(
        at(&missing, "/data/summary/code_blocks/passed"),
        1,
        "{missing}"
    );
    assert_eq!(
        at(&present, "/data/summary/code_blocks/passed"),
        1,
        "{present}"
    );
}

#[test]
fn the_real_rumdl_applies_per_file_ignores_by_the_docs_place_under_the_configs_directory() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_applies_per_file_ignores_by_the_docs_place_under_the_configs_directory",
    ) else {
        return;
    };
    let long = "## Sub\n\nthis line is longer than twenty characters";
    let project = project_with_block("", long);
    project.write(
        ".rumdl.toml",
        "[per-file-ignores]\n\"docs/*.md\" = [\"MD013\"]\n\n[MD013]\nline-length = 20\n",
    );
    project.write(
        "other/blocks.md",
        &format!("# Blocks\n\n```markdown\n{long}\n```\n"),
    );
    let path = [("PATH", dir.as_path())];

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &path)
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(
        at(&json, "/findings/0/location/file"),
        "other/blocks.md",
        "{json}"
    );
}

/// A project like [`project_with_block`] whose root is its own git root, holding `config` at
/// `config_file`, and the block `body` in `docs/blocks.md`.
fn git_project(config_file: &str, config: &str, body: &str) -> Project {
    let project = project_with_block("", body);
    project.write(".git/HEAD", "ref: refs/heads/main\n");
    project.write(config_file, config);
    project
}

/// The kinds of findings and the passed count of a real-rumdl `check` of `project`, run with
/// `vars` beside `PATH`.
fn real_run(project: &Project, dir: &Path, vars: &[(&str, &Path)], exit: i32) -> serde_json::Value {
    let mut all = vec![("PATH", dir)];
    all.extend_from_slice(vars);
    project
        .supdoc_in_with(".", &["check", "--json", "-v"], &all)
        .assert_exit(exit)
        .json()
}

const TRAILING: &str = "## T\n\ntext   ";
const IGNORE_DOCS: &str = "[per-file-ignores]\n\"docs/*.md\" = [\"MD009\"]\n";

#[test]
fn the_real_rumdl_resolves_a_nested_configs_patterns_against_the_git_root() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_resolves_a_nested_configs_patterns_against_the_git_root",
    ) else {
        return;
    };
    let project = git_project("docs/rumdl.toml", IGNORE_DOCS, TRAILING);

    let json = real_run(&project, &dir, &[], 0);

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_real_rumdl_resolves_dot_config_patterns_against_the_git_root() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_resolves_dot_config_patterns_against_the_git_root",
    ) else {
        return;
    };
    let project = git_project(".config/rumdl.toml", IGNORE_DOCS, TRAILING);

    let json = real_run(&project, &dir, &[], 0);

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_real_rumdl_resolves_a_nested_configs_patterns_against_its_directory_without_git() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_resolves_a_nested_configs_patterns_against_its_directory_without_git",
    ) else {
        return;
    };
    let project = project_with_block("", TRAILING);
    project.write(
        "docs/rumdl.toml",
        "[per-file-ignores]\n\"blocks.md\" = [\"MD009\"]\n",
    );

    let json = real_run(&project, &dir, &[], 0);

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_real_rumdl_keeps_the_order_of_a_per_file_flavor_table() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_keeps_the_order_of_a_per_file_flavor_table",
    ) else {
        return;
    };
    let project = project_with_blocks(
        "",
        "````markdown\n# T\n\n!!! note \"Title\"\n\n    indented content here\n\n```py\nx\n```\n````",
    );
    project.write(
        ".rumdl.toml",
        "[per-file-flavor]\n\"docs/**\" = \"mkdocs\"\n\"**\" = \"standard\"\n",
    );

    let json = real_run(&project, &dir, &[], 0);

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_real_rumdl_matches_a_per_file_entry_naming_the_doc_exactly() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_matches_a_per_file_entry_naming_the_doc_exactly",
    ) else {
        return;
    };
    let project = git_project(
        ".rumdl.toml",
        "[per-file-ignores]\n\"docs/blocks.md\" = [\"MD009\"]\n",
        TRAILING,
    );

    let json = real_run(&project, &dir, &[], 0);

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn the_real_rumdl_applies_per_file_ignores_when_the_cache_home_is_inside_a_git_repo() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "the_real_rumdl_applies_per_file_ignores_when_the_cache_home_is_inside_a_git_repo",
    ) else {
        return;
    };
    let repo = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    std::fs::create_dir_all(repo.path().join(".git")).expect("the git dir should be creatable");
    std::fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n")
        .expect("HEAD should be writable");
    let cache = repo.path().join("cache");
    let project = git_project(".rumdl.toml", IGNORE_DOCS, TRAILING);

    let json = real_run(&project, &dir, &[("XDG_CACHE_HOME", cache.as_path())], 0);

    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

/// The block's state, format and lint, the passed count and the findings of a real-rumdl check of
/// a block rumdl would otherwise fail, with `.rumdl.toml` holding `config`.
fn outcome_under(dir: &Path, config: &str) -> serde_json::Value {
    let project = project_with_block("", "## T\n\n\n\ntext   ");
    project.write(".rumdl.toml", config);
    let json = real_run(&project, dir, &[], 0);
    serde_json::json!({
        "state": at(&json, "/data/code_blocks/0/state"),
        "format": at(&json, "/data/code_blocks/0/format"),
        "lint": at(&json, "/data/code_blocks/0/lint"),
        "passed": at(&json, "/data/summary/code_blocks/passed"),
        "findings": finding_kinds(&json),
    })
}

/// What a project that disables every rule itself gets: both steps run, the block passes, and
/// nothing is reported.
fn disable_all_outcome() -> serde_json::Value {
    serde_json::json!({
        "state": "passed",
        "format": "ran",
        "lint": "ran",
        "passed": 1,
        "findings": [],
    })
}

#[test]
fn a_config_enabling_only_the_rules_supdoc_relaxes_passes_as_disabling_every_rule_does() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "a_config_enabling_only_the_rules_supdoc_relaxes_passes_as_disabling_every_rule_does",
    ) else {
        return;
    };

    let outcome = outcome_under(&dir, "[global]\nenable = [\"MD041\"]\n");

    assert_eq!(outcome, disable_all_outcome());
}

#[test]
fn a_config_whose_own_extend_disable_leaves_only_relaxed_rules_passes_alike() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "a_config_whose_own_extend_disable_leaves_only_relaxed_rules_passes_alike",
    ) else {
        return;
    };

    let outcome = outcome_under(
        &dir,
        "[global]\nenable = [\"MD041\", \"MD009\"]\nextend-disable = [\"MD009\"]\n",
    );

    assert_eq!(outcome, disable_all_outcome());
}

#[test]
fn a_nested_repository_does_not_take_the_outer_repositorys_config() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "a_nested_repository_does_not_take_the_outer_repositorys_config",
    ) else {
        return;
    };
    let outer = git_project(
        ".rumdl.toml",
        "[global]\ndisable = [\"MD009\", \"MD041\"]\n",
        TRAILING,
    );
    outer.write("inner/.git/HEAD", "ref: refs/heads/main\n");
    outer.write(
        "inner/docs/blocks.md",
        &format!("# Blocks\n\n```markdown\n{TRAILING}\n```\n"),
    );

    let json = real_run(&outer, &dir, &[], 1);

    assert_eq!(at(&json, "/data/code_blocks/0/state"), "passed", "{json}");
    assert_eq!(
        finding_kinds(&json),
        ["code-block-unformatted", "code-block-lint"],
        "{json}"
    );
    assert!(
        array_at(&json, "/findings")
            .iter()
            .all(|finding| finding.pointer("/location/file")
                == Some(&serde_json::json!("inner/docs/blocks.md"))),
        "{json}"
    );
}

#[test]
fn a_project_that_disables_every_rule_itself_keeps_that_choice() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "a_project_that_disables_every_rule_itself_keeps_that_choice",
    ) else {
        return;
    };
    let outcome = outcome_under(&dir, "[global]\ndisable = [\"all\"]\n");

    assert_eq!(outcome, disable_all_outcome());
}

/// A git project whose `.rumdl.toml` disables the rules [`TRAILING`] breaks, with the same block
/// and a plain doc holding it in `inner/docs`, and `inner/.git` a symlink to `target` (relative to
/// `inner`).
fn project_with_git_link(target: &str) -> Project {
    let project = git_project(
        ".rumdl.toml",
        "[global]\ndisable = [\"MD009\", \"MD041\"]\n",
        TRAILING,
    );
    project.write(
        "inner/docs/blocks.md",
        &format!("# Blocks\n\n```markdown\n{TRAILING}\n```\n"),
    );
    project.write("inner/docs/probe.md", &format!("{TRAILING}\n"));
    std::os::unix::fs::symlink(target, project.path("inner/.git"))
        .expect("the .git link should be creatable");
    project
}

/// Whether the real rumdl in `dir`, run from `project`'s root, passes `inner/docs/probe.md`.
fn rumdl_passes_probe(project: &Project, dir: &Path) -> bool {
    std::process::Command::new(dir.join("rumdl"))
        .args(["check", "--no-cache", "inner/docs/probe.md"])
        .current_dir(project.path("."))
        .output()
        .expect("the real rumdl should run")
        .status
        .success()
}

#[test]
fn a_dangling_git_link_does_not_stop_the_config_search() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "a_dangling_git_link_does_not_stop_the_config_search",
    ) else {
        return;
    };
    let project = project_with_git_link("../nowhere");

    let json = real_run(&project, &dir, &[], 0);

    assert!(
        rumdl_passes_probe(&project, &dir),
        "rumdl stopped at the dangling link"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 2, "{json}");
    assert_eq!(finding_kinds(&json), Vec::<String>::new(), "{json}");
}

#[test]
fn a_git_link_to_a_directory_still_stops_the_config_search() {
    let Some(dir) = real_tool_dir(
        "rumdl",
        "a_git_link_to_a_directory_still_stops_the_config_search",
    ) else {
        return;
    };
    let project = project_with_git_link("../.git");

    let json = real_run(&project, &dir, &[], 1);

    assert!(
        !rumdl_passes_probe(&project, &dir),
        "rumdl walked past the live link"
    );
    assert_eq!(
        finding_kinds(&json),
        ["code-block-unformatted", "code-block-lint"],
        "{json}"
    );
}

/// Each Markdown tag prettier reads, with the parser and virtual-path extension it gets.
const CASES: [(&str, &str, &str); 2] = [("markdown", "markdown", "md"), ("md", "markdown", "md")];

/// A Markdown block prettier leaves as it is.
const FORMATTED: &str = "- item\n- two";

/// A Markdown block prettier reformats.
const MISFORMATTED: &str = "*  item\n*  two";

/// A Markdown block holding `prettier-ignore`.
const IGNORED: &str = "<!-- prettier-ignore -->\n*  item";

/// The config that selects prettier for markdown blocks, with rumdl's step off so prettier is the
/// block's one tool.
const PRETTIER_CONFIG: &str = "[features.code-blocks]\n[features.code-blocks.markdown]\n\
    formatter = \"prettier\"\n[features.code-blocks.markdown.rumdl]\nlint = \"off\"\n";

/// A recorded `basic` project selecting prettier for markdown, with one block tagged `tag` holding
/// `body` in `docs/blocks.md`, which starts on doc line 4.
fn prettier_project(tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", PRETTIER_CONFIG);
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
    );
    project
}

/// Writes a `prettier` stub in `dir` that appends its working directory and arguments to
/// `<dir>/prettier-runs`, one line per call, prints `3.9.10` for `--version` and echoes its input
/// for any other call.
fn prettier_stub(dir: &Path) {
    let path = dir.join("prettier");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$(pwd -P) $*\" >> '{}'\n\
             if [ \"$1\" = --version ]; then echo 3.9.10; exit 0; fi\n/bin/cat\n",
            dir.join("prettier-runs").display()
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// The calls the [`prettier_stub`] in `dir` recorded that format a block: every one but
/// `--version`.
fn prettier_formats(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("prettier-runs"))
        .unwrap_or_default()
        .lines()
        .filter(|run| !run.ends_with(" --version"))
        .map(str::to_owned)
        .collect()
}

/// Runs a `check --json` on `project` with `path` as the only `PATH` entry, `trust` as
/// `SUPDOC_TRUST` and its cache in a fresh directory, asserting it exits `exit`.
fn prettier_check(project: &Project, path: &Path, trust: &str, exit: i32) -> serde_json::Value {
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[
                ("PATH", path),
                ("SUPDOC_TRUST", Path::new(trust)),
                ("XDG_CACHE_HOME", cache.path()),
            ],
        )
        .assert_exit(exit)
        .json()
}

#[test]
fn prettier_formats_a_markdown_block_from_the_root_by_its_tags_parser() {
    for (tag, parser, extension) in CASES {
        let project = prettier_project(tag, FORMATTED);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        prettier_stub(outside.path());

        let json = prettier_check(&project, outside.path(), "1", 0);

        let root = project
            .path(".")
            .canonicalize()
            .expect("the project root should resolve");
        let runs = prettier_formats(outside.path());
        let [run] = runs.as_slice() else {
            panic!("{tag}: prettier formats the block once: {runs:?}");
        };
        let (cwd, args) = run
            .split_once(' ')
            .expect("the record names a cwd and args");
        assert_eq!(
            Path::new(cwd),
            root,
            "{tag}: prettier runs from the root: {run}"
        );
        let overrides = format!(
            "--ignore-path= --with-node-modules --no-require-pragma --no-check-ignore-pragma \
             --no-insert-pragma --range-start=0 --range-end=Infinity --parser={parser} \
             --stdin-filepath "
        );
        let path = args.strip_prefix(&overrides);
        assert!(
            path.is_some_and(|path| path.ends_with(&format!("/docs/blocks.md.1.{extension}"))),
            "{tag}: prettier gets its overrides, the tag's parser and the block's path: {run}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {json}"
        );
    }
}

#[test]
fn prettier_untrusted_leaves_a_markdown_block_format_not_run_naming_trust() {
    let project = prettier_project(CASES[0].0, FORMATTED);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(outside.path());

    let json = prettier_check(&project, outside.path(), "0", 0);

    assert!(prettier_formats(outside.path()).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
    let reason = at(&json, "/data/code_blocks/0/reason");
    assert!(
        reason.as_str().is_some_and(|reason| reason
            .starts_with("prettier runs on Markdown blocks only in the project tier")),
        "{json}"
    );
}

#[test]
fn prettier_ignore_in_a_markdown_block_is_format_not_run_naming_format_off() {
    let project = prettier_project(CASES[0].0, IGNORED);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    prettier_stub(outside.path());

    let json = prettier_check(&project, outside.path(), "1", 0);

    assert!(prettier_formats(outside.path()).is_empty(), "{json}");
    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
}

#[test]
fn prettier_the_real_one_passes_a_formatted_markdown_block_and_fails_a_misformatted_one() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_passes_a_formatted_markdown_block_and_fails_a_misformatted_one",
    ) else {
        return;
    };
    for (tag, _, _) in CASES {
        let passes = prettier_check(&prettier_project(tag, FORMATTED), &dir, "1", 0);
        let fails = prettier_check(&prettier_project(tag, MISFORMATTED), &dir, "1", 1);

        assert_eq!(
            at(&passes, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {passes}"
        );
        assert_eq!(
            finding_kinds(&fails),
            ["code-block-unformatted"],
            "{tag}: {fails}"
        );
    }
}

#[test]
fn prettier_the_real_one_fails_a_misformatted_markdown_block_the_projects_ignore_files_cover() {
    let Some(dir) = real_tool_dir(
        "prettier",
        "prettier_the_real_one_fails_a_misformatted_markdown_block_the_projects_ignore_files_cover",
    ) else {
        return;
    };
    for ignore in [".gitignore", ".prettierignore"] {
        let project = prettier_project(CASES[0].0, MISFORMATTED);
        // Covers the block's virtual path, never the doc itself, which supdoc would then skip.
        project.write(ignore, "docs/blocks.md.*\n");

        let json = prettier_check(&project, &dir, "1", 1);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{ignore} never turns the block into a pass: {json}"
        );
    }
}
