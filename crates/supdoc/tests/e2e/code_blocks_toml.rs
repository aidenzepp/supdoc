//! Checking TOML code blocks, tagged `toml`, formatted and linted by tombi.

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

/// A recorded `basic` project with a `toml` table and one `toml` block holding `body` in
/// `docs/blocks.md`.
fn toml_project(body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.toml]\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```toml\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `tombi` running `script` in `dir`.
fn stub_tombi(dir: &Path, script: &str) {
    let path = dir.join("tombi");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A stub that refuses (exit 5) a run without `--offline`, a stdin name other than
/// `<private>/docs/blocks.md.1.toml`, a `.tombi.toml` where it runs, `NO_COLOR` or a private
/// `$XDG_CONFIG_HOME/tombi/config.toml`, appends the
/// private config to `$XDG_CONFIG_HOME/seen` for the test to read, then has `tombi format`
/// run `format` and `tombi lint` run `lint`.
fn stub_for(format: &str, lint: &str) -> String {
    format!(
        "case \" $* \" in *' --offline '*) ;; *) exit 5;; esac\n\
         for arg in \"$@\"; do last=\"$name\"; name=\"$arg\"; done\n\
         case \"$last\" in */docs/blocks.md.1.toml) ;; *) exit 5;; esac\n\
         [ -f \"$XDG_CONFIG_HOME/tombi/config.toml\" ] && [ -f .tombi.toml ] || exit 5\n\
         [ \"$NO_COLOR\" = 1 ] || exit 5\n\
         /bin/cat \"$XDG_CONFIG_HOME/tombi/config.toml\" >> \"$XDG_CONFIG_HOME/seen\"\n\
         if [ \"$1\" = lint ]; then\n{lint}\nfi\n\
         {format}"
    )
}

/// A tombi lint that found nothing in the one text it was given.
const LINT_CLEAN: &str = "echo '[]'; echo '1 file linted successfully' >&2; exit 0";

#[test]
fn toml_a_block_both_steps_pass_is_passed_and_tombi_gets_a_private_config() {
    let project = toml_project("a = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_tombi(outside.path(), &stub_for("/bin/cat", LINT_CLEAN));

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", outside.path()), ("XDG_CACHE_HOME", cache.path())],
        )
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

#[test]
fn toml_the_project_config_reaches_tombi_without_its_files_table() {
    let project = toml_project("a = 1");
    project.write(
        "tombi.toml",
        "[files]\nexclude = [\"docs/**\"]\n[format.rules]\nindent-width = 4\n",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_tombi(outside.path(), &stub_for("/bin/cat", LINT_CLEAN));

    let _json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[("PATH", outside.path()), ("XDG_CACHE_HOME", cache.path())],
        )
        .assert_exit(0)
        .json();

    let seen = std::fs::read_dir(cache.path().join("supdoc/code-blocks/tombi-config"))
        .expect("the private config directory should exist")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("seen")).ok())
        .collect::<String>();
    assert_eq!(
        seen,
        "[format.rules]\nindent-width = 4\n[format.rules]\nindent-width = 4\n"
    );
}

#[test]
fn toml_a_block_naming_a_schema_file_is_never_given_to_tombi() {
    let project = toml_project("#:schema ../secret.json\na = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_tombi(outside.path(), "exit 9");

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(0)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/not_checked"),
        1,
        "{json}"
    );
}

#[test]
fn toml_a_config_that_names_schemas_leaves_the_blocks_unchecked() {
    for config in [
        "[[schemas]]\npath = \"s.json\"\ninclude = [\"*.toml\"]\n",
        "[schema.catalog]\npaths = [\"https://example.com/catalog.json\"]\n",
        "[extensions]\n\"tombi-toml/cargo\" = { enabled = true }\n",
    ] {
        let project = toml_project("a = 1");
        project.write("tombi.toml", config);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_tombi(outside.path(), "exit 9");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{config}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("`tombi.toml`")),
            "{config}: {json}"
        );
    }
}

#[test]
fn toml_a_format_syntax_error_is_a_parse_error_at_its_doc_line() {
    let project = toml_project("a = 1\nb = ");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let format = "/bin/cat\nprintf '\\033[1;31m  Error\\033[0m: expected value\\n    \\033[90mat\\033[0m \
                  %s:2:5\\n' \"$last\" >&2\nexit 1";
    stub_tombi(outside.path(), &stub_for(format, LINT_CLEAN));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn toml_a_lint_run_that_reports_nothing_linted_never_passes() {
    let project = toml_project("a = 1");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_tombi(outside.path(), &stub_for("/bin/cat", "echo '[]'; exit 0"));

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .json();

    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{json}"
    );
}

#[test]
fn toml_the_real_tombi_formats_and_finds_a_duplicate_key() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_the_real_tombi_formats_and_finds_a_duplicate_key",
    ) else {
        return;
    };

    let formatted = toml_project("[a]\nb = [1, 2]")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(0)
        .json();
    let unformatted = toml_project("[a]\nb=[1,2]")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();
    let unparsed = toml_project("a = 1\nb = ")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();
    let duplicate = toml_project("a = 1\na = 2")
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", &dir)])
        .assert_exit(1)
        .json();

    assert_eq!(
        at(&formatted, "/data/summary/code_blocks/passed"),
        1,
        "{formatted}"
    );
    assert_eq!(
        finding_kinds(&unformatted),
        ["code-block-unformatted"],
        "{unformatted}"
    );
    assert_eq!(finding_kinds(&unparsed), ["code-block-parse"], "{unparsed}");
    assert_eq!(at(&unparsed, "/findings/0/location/line"), 5, "{unparsed}");
    assert_eq!(
        finding_kinds(&duplicate),
        ["code-block-parse"],
        "{duplicate}"
    );
    assert_eq!(
        at(&duplicate, "/findings/0/location/line"),
        5,
        "{duplicate}"
    );
}

/// A recorded `basic` project formatting `sh` blocks with shfmt, holding one block `body`.
fn shell_project(body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        "[features.code-blocks]\n[features.code-blocks.shell]\n[features.code-blocks.shell.shellcheck]\nlint = \"off\"\n",
    );
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```sh\n{body}\n```\n"),
    );
    project
}

#[test]
fn a_formatter_that_prints_nothing_for_a_block_leaves_its_format_step_unrun_and_fix_it_alone() {
    for printed in ["", "printf '\\n'", "printf '  \\n\\n'"] {
        let project = shell_project("echo hi");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let path = outside.path().join("shfmt");
        std::fs::write(&path, format!("#!/bin/sh\n{printed}\nexit 0\n"))
            .expect("the stub should be writable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        let before = project.read("docs/blocks.md");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{printed:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/format_not_run"),
            1,
            "{printed:?}: {json}"
        );
        assert_eq!(project.read("docs/blocks.md"), before, "{printed:?}");
    }
}

/// Runs `check --json` with the real tombi in `dir` on `project`, with `vars` added.
fn real_check(project: &Project, dir: &Path, vars: &[(&str, &Path)]) -> serde_json::Value {
    let mut all = vec![("PATH", dir)];
    all.extend_from_slice(vars);
    project
        .supdoc_in_with(".", &["check", "--json"], &all)
        .json()
}

#[test]
fn toml_the_real_tombi_never_takes_a_config_from_home_or_above_the_cache() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_the_real_tombi_never_takes_a_config_from_home_or_above_the_cache",
    ) else {
        return;
    };
    let scratch = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let home = scratch.path().join("home");
    let cache = scratch.path().join("outer/cache");
    std::fs::create_dir_all(home.join(".config")).expect("the scratch home");
    std::fs::create_dir_all(&cache).expect("the scratch cache");
    std::fs::write(
        home.join("s.json"),
        r#"{"type":"object","required":["must_have"]}"#,
    )
    .expect("a schema");
    std::fs::write(
        home.join(".config/tombi.toml"),
        format!(
            "[[schemas]]\npath = \"{}\"\ninclude = [\"**/*.toml\"]\n",
            home.join("s.json").display()
        ),
    )
    .expect("a home config");
    std::fs::write(
        scratch.path().join("outer/tombi.toml"),
        "[lint.rules]\nkey-empty = \"error\"\n",
    )
    .expect("a config above the cache");
    let home_cache = home.join(".cache");

    let clean = real_check(&toml_project("\"\" = 1"), &dir, &[]);
    let under_home = real_check(&toml_project("\"\" = 1"), &dir, &[("HOME", &home)]);
    let above_cache = real_check(
        &toml_project("\"\" = 1"),
        &dir,
        &[("HOME", &home), ("XDG_CACHE_HOME", &cache)],
    );

    assert!(
        home_cache.is_dir(),
        "the run under home used {home_cache:?}"
    );
    for json in [&clean, &under_home, &above_cache] {
        assert_eq!(at(json, "/data/summary/code_blocks/passed"), 1, "{json}");
        assert!(finding_kinds(json).is_empty(), "{json}");
    }
}

#[test]
fn toml_the_real_tombi_never_replays_a_cached_result_across_a_config_change() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_the_real_tombi_never_replays_a_cached_result_across_a_config_change",
    ) else {
        return;
    };
    let project = toml_project("\"\" = 1");
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let vars = [("XDG_CACHE_HOME", cache.path())];

    project.write("tombi.toml", "[lint.rules]\nkey-empty = \"error\"\n");
    let strict = real_check(&project, &dir, &vars);
    project.write("tombi.toml", "[lint.rules]\nkey-empty = \"off\"\n");
    let relaxed = real_check(&project, &dir, &vars);
    project.write("tombi.toml", "[lint.rules]\nkey-empty = \"error\"\n");
    let strict_again = real_check(&project, &dir, &vars);

    assert_eq!(finding_kinds(&strict), ["code-block-lint"], "{strict}");
    assert!(finding_kinds(&relaxed).is_empty(), "{relaxed}");
    assert_eq!(
        finding_kinds(&strict_again),
        ["code-block-lint"],
        "{strict_again}"
    );
}

#[test]
fn toml_the_real_tombi_checks_a_block_an_override_switches_off() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_the_real_tombi_checks_a_block_an_override_switches_off",
    ) else {
        return;
    };
    let linted = toml_project("\"\" = 1");
    linted.write(
        "tombi.toml",
        "[lint.rules]\nkey-empty = \"error\"\n\n[[overrides]]\nfiles.include = [\"docs/**\"]\nlint.enabled = false\n",
    );
    let formatted = toml_project("a=1");
    formatted.write(
        "tombi.toml",
        "[[overrides]]\nfiles.include = [\"docs/**\"]\nformat.enabled = false\n",
    );

    let lint = real_check(&linted, &dir, &[]);
    let format = real_check(&formatted, &dir, &[]);
    let _fixed = formatted.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

    assert_eq!(finding_kinds(&lint), ["code-block-lint"], "{lint}");
    assert_eq!(
        finding_kinds(&format),
        ["code-block-unformatted"],
        "{format}"
    );
    assert_eq!(
        formatted.read("docs/blocks.md"),
        "# Blocks\n\n```toml\na = 1\n```\n"
    );
}

#[test]
fn toml_the_real_tombis_messages_carry_no_colour_and_no_private_path() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_the_real_tombis_messages_carry_no_colour_and_no_private_path",
    ) else {
        return;
    };

    let json = real_check(&toml_project("a = 1\nb = "), &dir, &[]);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    let message = at(&json, "/findings/0/message")
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!message.contains('\u{1b}'), "{message}");
    assert!(!message.contains("tombi-config"), "{message}");
    assert!(message.contains("docs/blocks.md.1.toml:2:5"), "{message}");
}

#[test]
fn toml_the_real_tombi_applies_a_kept_override_to_the_mirrored_doc() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_the_real_tombi_applies_a_kept_override_to_the_mirrored_doc",
    ) else {
        return;
    };
    let override_for = |include: &str| {
        let project = toml_project("\"\" = 1");
        project.write(
            "tombi.toml",
            &format!(
                "[[overrides]]\nfiles.include = [\"{include}\"]\nlint.rules.key-empty = \"error\"\n"
            ),
        );
        real_check(&project, &dir, &[])
    };

    let matching = override_for("docs/**");
    let elsewhere = override_for("other/**");

    assert_eq!(finding_kinds(&matching), ["code-block-lint"], "{matching}");
    assert!(finding_kinds(&elsewhere).is_empty(), "{elsewhere}");
}

/// A `toml_project` holding `body` whose `tombi.toml` is `config`.
fn configured(config: &str, body: &str) -> Project {
    let project = toml_project(body);
    project.write("tombi.toml", config);
    project
}

#[test]
fn toml_a_project_config_tombi_cant_load_leaves_both_steps_unrun() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_a_project_config_tombi_cant_load_leaves_both_steps_unrun",
    ) else {
        return;
    };
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");

    for config in [
        "[lint.rules]\nkey-empty = \"error\"\nbogus-rule = \"error\"\n",
        "[lint.rules]\nkey-empty = 5\n",
        "[lint.rules]\nkey-empty = \"error\"\n[lint]\ndisabled = true\n",
    ] {
        let project = configured(config, "\"\" = 2");
        for _run in 0..2 {
            let json = real_check(&project, &dir, &[("XDG_CACHE_HOME", cache.path())]);

            assert_eq!(
                finding_kinds(&json),
                ["code-block-not-checked"],
                "{config}: {json}"
            );
            assert!(
                at(&json, "/findings/0/message").as_str().is_some_and(|m| m
                    .starts_with("tombi couldn't load the project's config: ")
                    && !m.contains("tombi-config")),
                "{config}: {json}"
            );
            for count in ["format_not_run", "lint_not_run"] {
                assert_eq!(
                    at(&json, &format!("/data/summary/code_blocks/{count}")),
                    1,
                    "{config}: {json}"
                );
            }
            assert_eq!(
                at(&json, "/data/summary/code_blocks/passed"),
                0,
                "{config}: {json}"
            );
        }
    }
}

#[test]
fn toml_a_config_planted_beside_supdocs_copy_never_applies() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_a_config_planted_beside_supdocs_copy_never_applies",
    ) else {
        return;
    };
    let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    let vars = [("XDG_CACHE_HOME", cache.path())];
    let first = real_check(&toml_project("a = 1"), &dir, &vars);
    let configs = cache.path().join("supdoc/code-blocks/tombi-config");
    let strict = "[lint.rules]\nkey-empty = \"error\"\n";
    for entry in std::fs::read_dir(&configs).expect("the private configs should exist") {
        let entry = entry.expect("an entry");
        if entry.path().is_dir() {
            std::fs::write(entry.path().join("tombi.toml"), strict).expect("a plant");
            std::fs::create_dir_all(entry.path().join(".config")).expect("a plant dir");
            std::fs::write(entry.path().join(".config/tombi.toml"), strict).expect("a plant");
        }
    }
    std::fs::write(configs.join("tombi.toml"), strict).expect("a plant");

    let planted = real_check(&toml_project("\"\" = 3"), &dir, &vars);

    assert_eq!(at(&first, "/data/summary/code_blocks/passed"), 1, "{first}");
    assert_eq!(
        at(&planted, "/data/summary/code_blocks/passed"),
        1,
        "{planted}"
    );
    assert!(finding_kinds(&planted).is_empty(), "{planted}");
}

#[test]
fn toml_a_block_that_switches_a_tombi_step_off_has_that_step_unrun() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_a_block_that_switches_a_tombi_step_off_has_that_step_unrun",
    ) else {
        return;
    };
    let strict = "[lint.rules]\nkey-empty = \"error\"\n";
    let no_lint = configured(strict, "#:tombi lint.disabled = true\n\n\"\" = 2");
    let no_format = configured(strict, "#:tombi format.disabled = true\na=1");
    let before = no_format.read("docs/blocks.md");

    let lint = real_check(&no_lint, &dir, &[]);
    let format = real_check(&no_format, &dir, &[]);
    let _fixed = no_format.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

    assert_eq!(finding_kinds(&lint), ["code-block-not-checked"], "{lint}");
    assert_eq!(
        at(&lint, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{lint}"
    );
    assert_eq!(
        finding_kinds(&format),
        ["code-block-not-checked"],
        "{format}"
    );
    assert_eq!(
        at(&format, "/data/summary/code_blocks/format_not_run"),
        1,
        "{format}"
    );
    assert_eq!(no_format.read("docs/blocks.md"), before);
}

#[test]
fn toml_a_rule_level_tombi_comment_in_the_block_is_honoured() {
    let Some(dir) = real_tool_dir(
        "tombi",
        "toml_a_rule_level_tombi_comment_in_the_block_is_honoured",
    ) else {
        return;
    };
    let project = configured(
        "[lint.rules]\nkey-empty = \"error\"\n",
        "# tombi: lint.rules.key-empty.disabled = true\n\"\" = 2",
    );

    let json = real_check(&project, &dir, &[]);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}
