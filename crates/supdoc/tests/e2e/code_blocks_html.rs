//! Checking HTML code blocks, tagged `html` or `htm`, formatted by superhtml and linted by biome.

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

/// An `html` table with biome's lint step off, so superhtml's format step alone decides a block.
const HTML: &str = "[features.code-blocks]\n[features.code-blocks.html]\n\
                    [features.code-blocks.html.biome-lint]\nlint = \"off\"\n";

/// An `html` table with superhtml's format step off, so biome's lint step alone decides a block.
const LINT_ONLY: &str = "[features.code-blocks]\n[features.code-blocks.html]\n\
                         [features.code-blocks.html.superhtml]\nformat = \"off\"\n";

/// A recorded `basic` project with [`HTML`] and `docs/blocks.md` holding `doc`.
fn project_with(doc: &str) -> Project {
    config_project(HTML, doc)
}

/// A recorded `basic` project with `config` and `docs/blocks.md` holding `doc`.
fn config_project(config: &str, doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", config);
    project.write("docs/blocks.md", doc);
    project
}

/// A [`project_with`] one block tagged `tag` holding `body`, whose first body line is the doc's
/// line 4.
fn html_project(tag: &str, body: &str) -> Project {
    project_with(&format!("# Blocks\n\n```{tag}\n{body}\n```\n"))
}

/// A [`LINT_ONLY`] project with one block tagged `tag` holding `body`, whose first body line is
/// the doc's line 4.
fn lint_project(tag: &str, body: &str) -> Project {
    config_project(LINT_ONLY, &format!("# Blocks\n\n```{tag}\n{body}\n```\n"))
}

/// Writes an executable `#!/bin/sh` stub named `superhtml` in `dir`: it prints `version` on
/// stderr for `version`, as superhtml 0.7.1 does; for any other run it leaves `ran` beside itself, refuses (exit 5) arguments other
/// than `fmt --stdin --syntax-only`, and then runs `format`.
fn stub_superhtml(dir: &Path, version: &str, format: &str) {
    let path = dir.join("superhtml");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             [ \"$1\" = version ] && {{ echo '{version}' >&2; exit 0; }}\n\
             : > \"${{0%/*}}/ran\"\n\
             [ \"$*\" = 'fmt --stdin --syntax-only' ] || exit 5\n\
             {format}\n"
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// A verified superhtml's `version` line.
const VERIFIED: &str = "0.7.1";

/// Runs `check --json` with `PATH` set to `dir` on `project`.
fn check(project: &Project, dir: &Path) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", dir)])
        .json()
}

#[test]
fn html_every_tag_fails_unformatted_and_fix_writes_superhtmls_output() {
    for tag in ["html", "htm"] {
        let project = html_project(tag, "<div>\n<p>a</p>\n    </div>");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_superhtml(
            outside.path(),
            VERIFIED,
            "/bin/cat > /dev/null; printf '<div>\\n\\t<p>a</p>\\n</div>\\n'",
        );

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(1)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{tag}: {json}"
        );
        assert_eq!(at(&json, "/findings/0/location/line"), 3, "{tag}: {json}");
        assert_eq!(
            project.read("docs/blocks.md"),
            format!("# Blocks\n\n```{tag}\n<div>\n\t<p>a</p>\n</div>\n```\n"),
            "{tag}"
        );
    }
}

#[test]
fn html_a_blank_block_is_never_given_to_superhtml() {
    for doc in [
        "# Blocks\n\n```html\n```\n",
        "# Blocks\n\n```html\n\n```\n",
        "# Blocks\n\n```html\n  \n\t\n```\n",
    ] {
        let project = project_with(doc);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_superhtml(outside.path(), VERIFIED, "/bin/cat > /dev/null");

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
                .is_some_and(|message| message.contains("holds only whitespace")),
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
            "{doc:?}: superhtml was given the block"
        );
        assert_eq!(project.read("docs/blocks.md"), doc);
    }
}

#[test]
fn html_a_syntax_error_on_superhtmls_zero_based_row_is_reported_at_its_doc_line() {
    let project = html_project("html", "<p>a</p>\n<div>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_superhtml(
        outside.path(),
        VERIFIED,
        "printf '<stdin>:1:0: missing end tag\\n   <div>\\n   ^^^^^\\n' >&2; exit 1",
    );

    let json = project
        .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
        .assert_exit(1)
        .json();

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 5, "{json}");
}

#[test]
fn html_only_a_verified_superhtml_version_runs_the_format_step() {
    for (version, runs) in [
        ("0.7.9", true),
        ("0.8.0", false),
        ("0.6.4", false),
        ("1.7.0", false),
        ("superhtml dev", false),
    ] {
        let project = html_project("html", "<p>a</p>");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_superhtml(outside.path(), version, "/bin/cat");

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
                        .contains("isn't a version supdoc's checks are verified against (0.7)")),
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
fn html_the_real_superhtml_formats_a_fragment_with_tabs_and_fix_writes_its_output() {
    let Some(dir) = real_tool_dir(
        "superhtml",
        "html_the_real_superhtml_formats_a_fragment_with_tabs_and_fix_writes_its_output",
    ) else {
        return;
    };

    let clean = html_project("html", "<div>\n\t<p>a</p>\n</div>\n<p>b</p><p>c</p>");
    let clean = check(&clean, &dir);
    let messy = html_project("htm", "<div>\n  <p>a</p>\n    </div>");
    let json = check(&messy, &dir);
    let _fixed = messy.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    let again = check(&messy, &dir);

    assert!(finding_kinds(&clean).is_empty(), "{clean}");
    assert_eq!(at(&clean, "/data/summary/code_blocks/passed"), 1, "{clean}");
    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        messy.read("docs/blocks.md"),
        "# Blocks\n\n```htm\n<div>\n\t<p>a</p>\n</div>\n```\n"
    );
    assert!(finding_kinds(&again).is_empty(), "{again}");
}

#[test]
fn html_the_real_superhtml_reports_a_syntax_error_at_its_doc_line_past_invalid_elements() {
    let Some(dir) = real_tool_dir(
        "superhtml",
        "html_the_real_superhtml_reports_a_syntax_error_at_its_doc_line_past_invalid_elements",
    ) else {
        return;
    };
    // `<img>` with no `src` or `alt` is invalid, not a syntax error: only `<div>`, never closed,
    // stops formatting, on the block's third line, the doc's line 6.
    let project = html_project("html", "<p>a</p>\n<img>\n<div>");

    let json = check(&project, &dir);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

    assert_eq!(finding_kinds(&json), ["code-block-parse"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 6, "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```html\n<p>a</p>\n<img>\n<div>\n```\n"
    );
}

#[test]
fn html_the_real_superhtml_is_never_given_a_blank_block() {
    let Some(dir) = real_tool_dir(
        "superhtml",
        "html_the_real_superhtml_is_never_given_a_blank_block",
    ) else {
        return;
    };
    for doc in [
        "# Blocks\n\n```html\n```\n",
        "# Blocks\n\n```html\n  \n\t\n```\n",
    ] {
        let project = project_with(doc);

        let json = check(&project, &dir);
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{doc:?}: {json}"
        );
        assert_eq!(project.read("docs/blocks.md"), doc);
    }
}

/// Blocks holding an element whose content superhtml 0.7.1 rewrites though HTML keeps it as
/// written, each with the element's name and the doc line its start tag is on, when the body's
/// first line is the doc's line 4 (see [`html_project`]).
const REWRITTEN: [(&str, &str, u32); 7] = [
    (
        "<form>\n<textarea name=\"msg\">  Dear team,\n    indented line</textarea>\n</form>",
        "textarea",
        5,
    ),
    (
        "<script>\nconst banner = `Hello\n  world`;\n</script>",
        "script",
        4,
    ),
    (
        "<div>\n<style>\n.a::before { content: \"a\\\n  b\"; }\n</style>\n</div>",
        "style",
        5,
    ),
    ("<p>a</p>\n\n<xmp>  a   b </xmp>", "xmp", 6),
    (
        "<noscript>\n<pre>  Enable\n    scripts</pre>\n</noscript>",
        "noscript",
        4,
    ),
    (
        "<div>\n<script type=\"text/plain\"> keep  these edges </script>\n</div>",
        "script",
        5,
    ),
    (
        "<script>\nconst a = 1;\nconst b = 2;\n</script>",
        "script",
        4,
    ),
];

/// Asserts that `json`, a check of a [`REWRITTEN`] block holding `element` at doc line `line`,
/// gave only the warning that superhtml would change the element's content, naming its line,
/// with the format step not run.
fn assert_withheld(json: &serde_json::Value, element: &str, line: u32) {
    assert_eq!(
        finding_kinds(json),
        ["code-block-not-checked"],
        "{element}: {json}"
    );
    let message = at(json, "/findings/0/message");
    assert!(
        message.as_str().is_some_and(|message| {
            message.contains(&format!("`<{element}>` on line {line},"))
                && message.contains("superhtml would change the content")
        }),
        "{element}: {json}"
    );
    assert_eq!(
        at(json, "/data/summary/code_blocks/format_not_run"),
        1,
        "{element}: {json}"
    );
}

#[test]
fn html_a_block_whose_content_superhtml_rewrites_is_never_given_to_it() {
    for (body, element, line) in REWRITTEN {
        let project = html_project("html", body);
        let doc = project.read("docs/blocks.md");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_superhtml(
            outside.path(),
            VERIFIED,
            "/bin/cat > /dev/null; printf 'x\\n'",
        );

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

        assert_withheld(&json, element, line);
        assert!(
            !outside.path().join("ran").exists(),
            "{element}: superhtml was given the block"
        );
        assert_eq!(project.read("docs/blocks.md"), doc, "{element}");
    }
}

#[test]
fn html_the_real_superhtml_is_never_given_a_block_whose_content_it_rewrites() {
    let Some(dir) = real_tool_dir(
        "superhtml",
        "html_the_real_superhtml_is_never_given_a_block_whose_content_it_rewrites",
    ) else {
        return;
    };
    for (body, element, line) in REWRITTEN {
        let project = html_project("html", body);
        let doc = project.read("docs/blocks.md");

        let json = check(&project, &dir);
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

        assert_withheld(&json, element, line);
        assert_eq!(project.read("docs/blocks.md"), doc, "{element}");
    }
}

#[test]
fn html_the_real_superhtml_still_formats_a_block_with_a_one_line_script() {
    let Some(dir) = real_tool_dir(
        "superhtml",
        "html_the_real_superhtml_still_formats_a_block_with_a_one_line_script",
    ) else {
        return;
    };
    let project = html_project("html", "<div>\n<script>let s = 'a';</script>\n</div>");

    let json = check(&project, &dir);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    let again = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```html\n<div>\n\t<script>let s = 'a';</script>\n</div>\n```\n"
    );
    assert!(finding_kinds(&again).is_empty(), "{again}");
}

#[test]
fn html_the_real_superhtml_formats_a_one_line_script_body_and_the_fixed_block_stays_checkable() {
    let Some(dir) = real_tool_dir(
        "superhtml",
        "html_the_real_superhtml_formats_a_one_line_script_body_and_the_fixed_block_stays_checkable",
    ) else {
        return;
    };
    // One non-blank body line spread over three: superhtml moves and re-indents it, and the
    // block it writes holds the same shape, so the next check passes instead of withholding it.
    let project = html_project("html", "<div>\n<script>\n  a();\n</script>\n</div>");

    let json = check(&project, &dir);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
    let again = check(&project, &dir);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```html\n<div>\n\t<script>\n\t\ta();\n\t</script>\n</div>\n```\n"
    );
    assert!(finding_kinds(&again).is_empty(), "{again}");
    assert_eq!(at(&again, "/data/summary/code_blocks/passed"), 1, "{again}");
}

/// Writes an executable `#!/bin/sh` stub named `biome` in `dir`: it leaves `ran` beside itself,
/// refuses (exit 6) a run that isn't `biome lint` on a block file ending `blocks.md.1.<ext>`, and
/// prints `report`.
fn stub_biome(dir: &Path, ext: &str, report: &str) {
    let path = dir.join("biome");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             : > \"${{0%/*}}/ran\"\n\
             [ \"$1\" = lint ] || exit 6\n\
             for arg in \"$@\"; do file=\"$arg\"; done\n\
             case \"$file\" in */biome-lint/*/blocks.md.1.{ext}) ;; *) exit 6;; esac\n\
             echo '{report}'\n"
        ),
    )
    .expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// biome lint's report for a block it checked and found nothing in.
const CLEAN: &str = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[],"command":"lint"}"#;

/// biome 2.6.5's report on `<img src="a.png">`: one rule's finding on the block's line 1.
const ALT_TEXT: &str = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[{"severity":"error","message":"Provide a text alternative through the alt, aria-label, or aria-labelledby attribute.","category":"lint/a11y/useAltText","location":{"path":"x","start":{"line":1,"column":1},"end":{"line":1,"column":18}},"advices":[]}],"command":"lint"}"#;

/// biome 2.6.5's report on valid HTML omitting `</li>` then holding `<img src="a.png">` (probe
/// V6-PR5, residual 1): two `parse` diagnostics beside a rule's finding.
const OMITTED_END_TAG: &str = r#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[{"severity":"error","message":"Expected a matching closing tag but instead found </ul>.","category":"parse","location":{"path":"x","start":{"line":4,"column":1}},"advices":[]},{"severity":"error","message":"Expected a closing tag but instead found the end of the file.","category":"parse","location":{"path":"x","start":{"line":6,"column":1}},"advices":[]},{"severity":"error","message":"Provide a text alternative.","category":"lint/a11y/useAltText","location":{"path":"x","start":{"line":5,"column":1}},"advices":[]}],"command":"lint"}"#;

/// biome 2.6.5's report when `html.linter.enabled: false` leaves the block out (probe V6-PR5, S5).
const LINTED_NOTHING: &str =
    r#"{"summary":{"changed":0,"unchanged":0},"diagnostics":[],"command":"lint"}"#;

/// Runs `check --json -v` with `PATH` set to `dir` on `project`, asserting the exit `code`.
fn check_exit(project: &Project, dir: &Path, code: i32) -> serde_json::Value {
    project
        .supdoc_in_with(".", &["check", "--json", "-v"], &[("PATH", dir)])
        .assert_exit(code)
        .json()
}

/// Asserts `json` holds one block whose lint step didn't run for a reason containing `reason`,
/// with one `code-block-not-checked` warning and no block passed.
fn assert_lint_not_run(json: &serde_json::Value, reason: &str, case: &str) {
    assert_eq!(
        finding_kinds(json),
        ["code-block-not-checked"],
        "{case}: {json}"
    );
    assert_eq!(
        at(json, "/data/code_blocks/0/lint"),
        "not_run",
        "{case}: {json}"
    );
    assert!(
        at(json, "/data/code_blocks/0/reason")
            .as_str()
            .is_some_and(|said| said.contains(reason)),
        "{case}: {json}"
    );
    assert_eq!(
        at(json, "/data/summary/code_blocks/passed"),
        0,
        "{case}: {json}"
    );
}

#[test]
fn html_each_tag_is_linted_from_a_file_of_its_own_extension_and_a_clean_block_passes() {
    for tag in ["html", "htm"] {
        let project = lint_project(tag, "<div class=\"a\">hi</div>");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_biome(outside.path(), tag, CLEAN);

        let json = check_exit(&project, outside.path(), 0);

        assert!(finding_kinds(&json).is_empty(), "{tag}: {json}");
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {json}"
        );
    }
}

#[test]
fn html_a_biome_rules_finding_is_a_code_block_lint_error_at_its_doc_line() {
    let project = lint_project("html", "<img src=\"a.png\">");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), "html", ALT_TEXT);

    let json = check_exit(&project, outside.path(), 1);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/findings/0/location/line"), 4, "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("`biome-lint` reports `a11y/useAltText`")),
        "{json}"
    );
}

#[test]
fn html_a_block_biome_cant_parse_is_not_linted_never_a_finding_and_never_passed() {
    let project = lint_project("html", "<ul>\n<li>one\n<li>two\n</ul>\n<img src=\"a.png\">");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), "html", OMITTED_END_TAG);

    let json = check_exit(&project, outside.path(), 0);

    assert_lint_not_run(&json, "biome can't parse the HTML block", "parse");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("optional end tag")),
        "{json}"
    );
}

#[test]
fn html_a_linter_the_projects_biome_config_turns_off_is_not_run_never_passed() {
    let project = lint_project("html", "<img src=\"a.png\">");
    project.write("biome.json", r#"{"html":{"linter":{"enabled":false}}}"#);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), "html", LINTED_NOTHING);

    let json = check_exit(&project, outside.path(), 0);

    assert_lint_not_run(&json, "biome lint checked no file", "linter off");
}

#[test]
fn html_a_biome_ignore_that_may_silence_more_than_named_rules_is_never_given_to_biome() {
    for comment in [
        "<!-- biome-ignore-all lint: why -->",
        "<!-- biome-ignore lint: why -->",
        "<!-- biome-ignore-start lint/a11y: why -->",
        "<!-- BIOME_IGNORE lint/a11y/useAltText lint: why -->",
    ] {
        let project = lint_project("html", &format!("{comment}\n<img src=\"a.png\">"));
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_biome(outside.path(), "html", CLEAN);

        let json = check_exit(&project, outside.path(), 0);

        assert_lint_not_run(&json, "`biome-ignore` comment", comment);
        assert!(
            !outside.path().join("ran").exists(),
            "{comment}: biome was given the block"
        );
    }
}

#[test]
fn html_a_biome_ignore_naming_only_rules_leaves_the_block_to_biome() {
    let project = lint_project(
        "html",
        "<!-- biome-ignore lint/a11y/useAltText: decorative -->\n<img src=\"a.png\">",
    );
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_biome(outside.path(), "html", CLEAN);

    let json = check_exit(&project, outside.path(), 0);

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert!(
        outside.path().join("ran").exists(),
        "biome wasn't given the block"
    );
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
}

/// The real biome's `PATH` directory, or `None` to skip `test`.
fn real_biome(test: &str) -> Option<std::path::PathBuf> {
    real_tool_dir("biome", test)
}

#[test]
fn html_the_real_biome_reports_a_rules_finding_at_its_doc_line_and_passes_a_clean_block() {
    let Some(dir) = real_biome(
        "html_the_real_biome_reports_a_rules_finding_at_its_doc_line_and_passes_a_clean_block",
    ) else {
        return;
    };
    for tag in ["html", "htm"] {
        let found = check_exit(&lint_project(tag, "<p>a</p>\n<img src=\"a.png\">"), &dir, 1);
        let clean = check_exit(&lint_project(tag, "<div class=\"a\">hi</div>"), &dir, 0);

        assert_eq!(finding_kinds(&found), ["code-block-lint"], "{tag}: {found}");
        assert_eq!(at(&found, "/findings/0/location/line"), 5, "{tag}: {found}");
        assert!(
            at(&found, "/findings/0/message")
                .as_str()
                .is_some_and(|m| m.contains("a11y/useAltText")),
            "{tag}: {found}"
        );
        assert_eq!(
            at(&clean, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {clean}"
        );
    }
}

#[test]
fn html_the_real_biome_never_lints_or_passes_valid_html_omitting_optional_end_tags() {
    let Some(dir) = real_biome(
        "html_the_real_biome_never_lints_or_passes_valid_html_omitting_optional_end_tags",
    ) else {
        return;
    };
    for body in [
        "<ul>\n<li>one\n<li>two\n</ul>\n<img src=\"a.png\">",
        "<p>a\n<p>b",
        "<table><tr><td>x<td>y</table>",
    ] {
        let json = check_exit(&lint_project("html", body), &dir, 0);

        assert_lint_not_run(&json, "biome can't parse the HTML block", body);
    }
}

#[test]
fn html_the_real_biome_with_the_html_linter_off_never_passes_a_block() {
    let Some(dir) = real_biome("html_the_real_biome_with_the_html_linter_off_never_passes_a_block")
    else {
        return;
    };
    let project = lint_project("html", "<img src=\"a.png\">");
    project.write("biome.json", r#"{"html":{"linter":{"enabled":false}}}"#);

    let json = check_exit(&project, &dir, 0);

    assert_lint_not_run(&json, "biome lint checked no file", "linter off");
}

#[test]
fn html_the_real_biome_honors_a_rules_biome_ignore_and_never_runs_under_one_silencing_all() {
    let Some(dir) = real_biome(
        "html_the_real_biome_honors_a_rules_biome_ignore_and_never_runs_under_one_silencing_all",
    ) else {
        return;
    };
    let named = lint_project(
        "html",
        "<!-- biome-ignore lint/a11y/useAltText: decorative -->\n<img src=\"a.png\">\n<button></button>",
    );
    let all = lint_project(
        "html",
        "<!-- biome-ignore-all lint: why -->\n<img src=\"a.png\">",
    );

    let named = check_exit(&named, &dir, 1);
    let all = check_exit(&all, &dir, 0);

    assert_eq!(finding_kinds(&named), ["code-block-lint"], "{named}");
    assert!(
        at(&named, "/findings/0/message")
            .as_str()
            .is_some_and(|m| m.contains("a11y/useButtonType")),
        "only the named rule is silenced: {named}"
    );
    assert_lint_not_run(&all, "`biome-ignore` comment", "all");
}
