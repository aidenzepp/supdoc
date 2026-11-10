//! Checking HTML code blocks, tagged `html` or `htm`, formatted by superhtml.

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

/// An `html` table: superhtml formats, and no linter is known yet, so the format step alone
/// decides a block.
const HTML: &str = "[features.code-blocks]\n[features.code-blocks.html]\n";

/// A recorded `basic` project with [`HTML`] and `docs/blocks.md` holding `doc`.
fn project_with(doc: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", HTML);
    project.write("docs/blocks.md", doc);
    project
}

/// A [`project_with`] one block tagged `tag` holding `body`, whose first body line is the doc's
/// line 4.
fn html_project(tag: &str, body: &str) -> Project {
    project_with(&format!("# Blocks\n\n```{tag}\n{body}\n```\n"))
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
