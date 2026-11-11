//! Checking XML code blocks, tagged `xml`, `svg`, `xsd`, `xsl`, `xslt` or `xhtml`, formatted by
//! `xmllint --format` and linted by xmllint.

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

/// The rules and doc lines of a `--json` check's `code-block-lint` findings, in order.
fn lint_rules(json: &serde_json::Value) -> Vec<(String, u64)> {
    array_at(json, "/findings")
        .iter()
        .filter(|finding| finding["kind"] == "code-block-lint")
        .filter_map(|finding| {
            let message = finding["message"].as_str()?;
            let (_, rest) = message.split_once("reports `")?;
            let (rule, _) = rest.split_once('`')?;
            Some((
                rule.to_owned(),
                finding.pointer("/location/line")?.as_u64()?,
            ))
        })
        .collect()
}

/// An `xml` table with only xmllint's lint step on, its format step off.
const XML: &str = "[features.code-blocks]\n[features.code-blocks.xml]\n\
                   [features.code-blocks.xml.xmllint-format]\nformat = \"off\"\n";

/// An `xml` table as written bare: `xmllint-format`'s format step and xmllint's lint step on.
const XML_BOTH: &str = "[features.code-blocks]\n[features.code-blocks.xml]\n";

/// An `xml` table with only `xmllint-format`'s format step on, xmllint's lint step off.
const XML_FORMAT: &str = "[features.code-blocks]\n[features.code-blocks.xml]\n\
                          [features.code-blocks.xml.xmllint]\nlint = \"off\"\n";

/// A recorded `basic` project with [`XML`] and `docs/blocks.md` holding one block tagged `tag`
/// holding `body`, whose first body line is the doc's line 4.
fn xml_project(tag: &str, body: &str) -> Project {
    project_with(XML, tag, body)
}

/// A recorded `basic` project with `config` and `docs/blocks.md` holding one block tagged `tag`
/// holding `body`, whose first body line is the doc's line 4.
fn project_with(config: &str, tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", config);
    project.write(
        "docs/blocks.md",
        &format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
    );
    project
}

/// Writes an executable `#!/bin/sh` stub named `xmllint` in `dir`. For `--version` it prints
/// `version` on stderr. Any other run appends its arguments, one per line, to `args` beside
/// itself, its environment to `env` and its stdin to `stdin`, then runs `lint`. Every run, the
/// version's too, appends a line to `ran`.
fn stub_xmllint(dir: &Path, version: &str, lint: &str) {
    let path = dir.join("xmllint");
    let script = format!(
        "#!/bin/sh\n\
         here=\"${{0%/*}}\"\n\
         echo run >> \"$here/ran\"\n\
         [ \"$1\" = --version ] && {{ echo \"$0: using libxml version {version}\" >&2; exit 0; }}\n\
         for arg in \"$@\"; do printf '%s\\n' \"$arg\" >> \"$here/args\"; done\n\
         /usr/bin/env >> \"$here/env\"\n\
         /bin/cat > \"$here/stdin\"\n\
         {lint}\n"
    );
    std::fs::write(&path, script).expect("the stub should be writable");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the stub should be made executable");
}

/// The libxml2 number a verified xmllint reports: 2.9.13.
const VERIFIED: &str = "20913";

/// A namespace error at the block's line 2, printed with exit 0, as xmllint does.
const NAMESPACE_EXIT_0: &str = "echo '-:2: namespace error : Namespace prefix q on c is not defined' >&2; \
                                echo '<q:c/></a>' >&2; echo '    ^' >&2; exit 0";

/// Runs `check --json` on `project` with only `dir` on `PATH`, plus `vars`.
fn check(project: &Project, dir: &Path, vars: &[(&str, &Path)]) -> serde_json::Value {
    let mut all = vec![("PATH", dir)];
    all.extend_from_slice(vars);
    project
        .supdoc_in_with(".", &["check", "--json"], &all)
        .json()
}

#[test]
fn xml_every_tag_is_linted_and_a_namespace_error_on_exit_0_fails_at_its_doc_line() {
    for tag in ["xml", "svg", "xsd", "xsl", "xslt", "xhtml"] {
        let project = xml_project(tag, "<a xmlns:p=\"u\">\n<q:c/></a>");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_xmllint(outside.path(), VERIFIED, NAMESPACE_EXIT_0);

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(1)
            .json();

        assert_eq!(
            lint_rules(&json),
            [("namespace error".to_owned(), 5)],
            "{tag}: {json}"
        );
        assert_eq!(finding_kinds(&json), ["code-block-lint"], "{tag}: {json}");
    }
}

#[test]
fn xml_xmllint_gets_exactly_its_four_arguments_the_block_on_stdin_and_no_xml_variable() {
    let project = xml_project("xml", "<a>\n  <b>x</b>\n</a>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(outside.path(), VERIFIED, "exit 0");
    let catalog = outside.path().join("catalog.xml");

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[
                ("PATH", outside.path()),
                ("XML_CATALOG_FILES", &catalog),
                ("XMLLINT_INDENT", Path::new("\t")),
                ("SGML_CATALOG_FILES", &catalog),
            ],
        )
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    let read = |name: &str| {
        std::fs::read_to_string(outside.path().join(name))
            .unwrap_or_else(|error| panic!("the stub should have written `{name}`: {error}"))
    };
    assert_eq!(read("args"), "--noout\n--nonet\n--nocatalogs\n-\n");
    assert_eq!(read("stdin"), "<a>\n  <b>x</b>\n</a>\n");
    let env = read("env");
    assert!(
        !env.lines()
            .any(|line| line.starts_with("XML") || line.starts_with("SGML")),
        "an XML variable reached xmllint: {env}"
    );
}

#[test]
fn xml_a_block_naming_an_external_identifier_or_an_entity_never_reaches_xmllint() {
    let bodies = [
        "<!DOCTYPE a SYSTEM \"file:///etc/passwd\">\n<a/>",
        "<!DOCTYPE a PUBLIC \"-//X//DTD A//EN\" \"http://example.com/a.dtd\">\n<a/>",
        "<!doctype a\n  system 'a.dtd'>\n<a/>",
        "<! DocType a SyStEm 'a.dtd'><a/>",
        "<!DOCTYPE a [<!ENTITY e \"x\">]>\n<a>&e;</a>",
        "<!DOCTYPE a [<!entity % p SYSTEM \"p.dtd\"> %p;]>\n<a/>",
        "<?xml version=\"1.0\" encoding=\"UTF-7\"?>\n+ADw-+ACE-DOCTYPE a+AD4-<a/>",
    ];
    for body in bodies {
        let project = xml_project("xml", body);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_xmllint(outside.path(), VERIFIED, "exit 0");

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(0)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|message| message.contains("supdoc doesn't give to xmllint")
                    || message.contains("doesn't give it to xmllint")),
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/not_checked"),
            1,
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{body:?}: {json}"
        );
        assert!(
            !outside.path().join("ran").exists(),
            "{body:?}: xmllint ran"
        );
    }
}

#[test]
fn xml_stdout_or_stderr_supdoc_cant_read_is_a_tool_failure_not_a_pass() {
    let cases = [
        ("echo '<a/>'; exit 0", "stdout"),
        (
            "echo 'warning: failed to load external entity' >&2; exit 0",
            "warning",
        ),
        ("exit 1", "exit"),
    ];
    for (lint, said) in cases {
        let project = xml_project("xml", "<a/>");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_xmllint(outside.path(), VERIFIED, lint);

        let json = project
            .supdoc_in_with(".", &["check", "--json"], &[("PATH", outside.path())])
            .assert_exit(1)
            .json();

        assert_eq!(
            finding_kinds(&json),
            ["code-block-tool-failed"],
            "{said}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{said}: {json}"
        );
    }
}

#[test]
fn xml_only_a_verified_libxml2_minor_runs_the_lint_step() {
    for (version, runs) in [
        ("20913", true),
        ("20914-GITv2.9.14", true),
        ("21502", true),
        ("21600", false),
        ("21000", false),
        ("garbage", false),
    ] {
        let project = xml_project("xml", "<a xmlns:p=\"u\">\n<q:c/></a>");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_xmllint(outside.path(), version, NAMESPACE_EXIT_0);

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
                    .is_some_and(|message| message.contains(
                        "isn't a version supdoc's checks are verified against (2.9, 2.15)"
                    )),
                "{version}: {json}"
            );
            assert_eq!(
                at(&json, "/data/summary/code_blocks/lint_not_run"),
                1,
                "{version}: {json}"
            );
            assert!(
                !outside.path().join("args").exists(),
                "{version}: the lint step ran"
            );
        }
    }
}

#[test]
fn xml_the_real_xmllint_fails_each_malformed_block_at_its_doc_line_and_passes_a_good_one() {
    let Some(dir) = real_tool_dir(
        "xmllint",
        "xml_the_real_xmllint_fails_each_malformed_block_at_its_doc_line_and_passes_a_good_one",
    ) else {
        return;
    };
    let good = [
        "<a>\n  <b>x</b>\n</a>",
        "<!DOCTYPE a>\n<a/>",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    ];
    for body in good {
        let json = check(&project_with(XML_BOTH, "xml", body), &dir, &[]);
        assert!(finding_kinds(&json).is_empty(), "{body:?}: {json}");
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{body:?}: {json}"
        );
    }
    let bad = [
        ("<a>\n<b></a>", "parser error", 5),
        ("<a/>\n<b/>", "parser error", 5),
        ("hello", "parser error", 4),
        ("<a xmlns:p=\"u\">\n<q:c/></a>", "namespace error", 5),
        ("<a>&e;</a>", "parser error", 4),
        ("<a>\r\n<b>\r\n</a>", "parser error", 6),
        ("<?xml version=\"1.1\"?><a/>", "parser warning", 4),
    ];
    for (body, rule, line) in bad {
        let json = check(&project_with(XML_BOTH, "xml", body), &dir, &[]);
        let rules = lint_rules(&json);
        assert!(
            rules.first() == Some(&(rule.to_owned(), line)),
            "{body:?}: expected `{rule}` at line {line}, got {rules:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{body:?}: {json}"
        );
    }
}

#[test]
fn xml_the_real_xmllint_never_passes_an_odd_block() {
    let Some(dir) = real_tool_dir("xmllint", "xml_the_real_xmllint_never_passes_an_odd_block")
    else {
        return;
    };
    let long = format!("<a b=\"{}\">", "x".repeat(1024 * 1024));
    let bodies = ["", "   ", "\u{feff}", "<a>\u{fffd}<", long.as_str()];
    for body in bodies {
        let json = check(&project_with(XML_BOTH, "xml", body), &dir, &[]);
        let shown = body.get(..40).unwrap_or(body);
        assert_eq!(
            at(&json, "/data/summary/code_blocks/failed"),
            1,
            "{shown:?}: {json}"
        );
        let kinds = finding_kinds(&json);
        assert!(
            !kinds.is_empty()
                && kinds
                    .iter()
                    .all(|kind| kind == "code-block-lint" || kind == "code-block-tool-failed"),
            "{shown:?}: expected only lint or tool-failed findings, got {kinds:?}: {json}"
        );
    }
}

#[test]
fn xml_the_real_xmllint_never_sees_a_block_naming_a_file() {
    let Some(dir) = real_tool_dir(
        "xmllint",
        "xml_the_real_xmllint_never_sees_a_block_naming_a_file",
    ) else {
        return;
    };
    let blocks = [
        (
            "<!DOCTYPE a [<!ENTITY e SYSTEM \"file:///etc/passwd\">]>\n<a>&e;</a>",
            "external identifier",
        ),
        (
            "<!DOCTYPE a SYSTEM \"file:///etc/passwd\">\n<a/>",
            "external identifier",
        ),
        (
            "<!DOCTYPE a PUBLIC \"-//X//DTD A//EN\" \"http://example.com/a.dtd\">\n<a/>",
            "external identifier",
        ),
        (
            "<!DOCTYPE a [<!ENTITY e \"x\">]>\n<a>&e;</a>",
            "declares an entity",
        ),
        (
            "<?xml version=\"1.0\" encoding=\"UTF-7\"?>\n+ADw-+ACE-ENTITY e+AD4-<a/>",
            "encoding other than UTF-8",
        ),
    ];
    for (body, reason) in blocks {
        let json = check(&project_with(XML_BOTH, "xml", body), &dir, &[]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert!(
            at(&json, "/data/code_blocks/0/reason")
                .as_str()
                .is_some_and(|said| said.contains(reason)),
            "{body:?}: expected a reason naming {reason:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{body:?}: {json}"
        );
    }
}

/// What `stub_xmllint` runs when its format step should print `stdout` for any block: the format
/// run prints it, the lint run prints nothing; both exit 0.
fn formats_as(stdout: &str) -> String {
    format!("[ \"$1\" = --format ] && printf '%s' '{stdout}'; exit 0")
}

/// The stub's `args` file: one argument per line, every run's in order.
fn args_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("args")).unwrap_or_default()
}

#[test]
fn xml_the_format_step_gets_exactly_its_four_arguments_after_the_lint_step_and_no_xml_variable() {
    let project = project_with(XML_BOTH, "xml", "<a>\n  <b>x</b>\n</a>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(
        outside.path(),
        VERIFIED,
        &formats_as("<?xml version=\"1.0\"?>\n<a>\n  <b>x</b>\n</a>\n"),
    );

    let json = project
        .supdoc_in_with(
            ".",
            &["check", "--json"],
            &[
                ("PATH", outside.path()),
                ("XMLLINT_INDENT", Path::new("\t")),
                ("XML_CATALOG_FILES", &outside.path().join("catalog.xml")),
            ],
        )
        .assert_exit(0)
        .json();

    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");
    assert_eq!(
        args_of(outside.path()),
        "--noout\n--nonet\n--nocatalogs\n-\n--format\n--nonet\n--nocatalogs\n-\n",
        "the lint step runs first, then the format step"
    );
    let env = std::fs::read_to_string(outside.path().join("env"))
        .expect("the stub should have written `env`");
    assert!(
        !env.lines()
            .any(|line| line.starts_with("XML") || line.starts_with("SGML")),
        "an XML variable reached xmllint: {env}"
    );
}

#[test]
fn xml_a_block_the_lint_step_rejects_is_never_formatted() {
    let project = project_with(XML_BOTH, "xml", "<a xmlns:p=\"u\">\n<q:c/></a>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(outside.path(), VERIFIED, NAMESPACE_EXIT_0);

    let json = check(&project, outside.path(), &[]);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

    assert_eq!(finding_kinds(&json), ["code-block-lint"], "{json}");
    assert_eq!(at(&json, "/data/code_blocks/0/format"), "not_run", "{json}");
    assert!(
        !args_of(outside.path()).contains("--format"),
        "the format step ran: {}",
        args_of(outside.path())
    );
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```xml\n<a xmlns:p=\"u\">\n<q:c/></a>\n```\n"
    );
}

#[test]
fn xml_the_added_declaration_comes_off_and_fix_writes_only_the_indented_block() {
    let project = project_with(XML_FORMAT, "xml", "<a>\n<b>x</b>\n</a>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(
        outside.path(),
        VERIFIED,
        &formats_as("<?xml version=\"1.0\"?>\n<a>\n  <b>x</b>\n</a>\n"),
    );

    let json = check(&project, outside.path(), &[]);
    let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

    assert_eq!(finding_kinds(&json), ["code-block-unformatted"], "{json}");
    assert_eq!(
        project.read("docs/blocks.md"),
        "# Blocks\n\n```xml\n<a>\n  <b>x</b>\n</a>\n```\n"
    );
}

#[test]
fn xml_a_declaration_the_block_writes_is_kept_and_output_without_the_added_one_fails() {
    let declared = "<?xml version=\"1.0\"?>\n<a/>";
    let project = project_with(XML_FORMAT, "xml", declared);
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(
        outside.path(),
        VERIFIED,
        &formats_as(&format!("{declared}\n")),
    );
    let json = check(&project, outside.path(), &[]);
    assert!(finding_kinds(&json).is_empty(), "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 1, "{json}");

    let project = project_with(XML_FORMAT, "xml", "<a/>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(outside.path(), VERIFIED, &formats_as("<a/>\n"));
    let json = check(&project, outside.path(), &[]);
    assert_eq!(finding_kinds(&json), ["code-block-tool-failed"], "{json}");
}

#[test]
fn xml_format_stderr_after_exit_0_or_a_failed_exit_is_a_tool_failure_never_a_rewrite() {
    let cases = [
        "[ \"$1\" = --format ] && { printf '<?xml version=\"1.0\"?>\\n<q:a/>\\n'; \
         echo '-:1: namespace error : Namespace prefix q on a is not defined' >&2; }; exit 0",
        "[ \"$1\" = --format ] && printf '<?xml version=\"1.0\"?>\\n<q:a/>\\n'; exit 1",
    ];
    for script in cases {
        let project = project_with(XML_FORMAT, "xml", "<q:a></q:a>");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_xmllint(outside.path(), VERIFIED, script);

        let json = check(&project, outside.path(), &[]);
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-tool-failed"],
            "{script}: {json}"
        );
        assert_eq!(
            project.read("docs/blocks.md"),
            "# Blocks\n\n```xml\n<q:a></q:a>\n```\n",
            "{script}"
        );
    }
}

#[test]
fn xml_a_block_xmllint_is_never_given_withholds_its_format_step_too() {
    let project = project_with(XML_BOTH, "xml", "<!DOCTYPE a SYSTEM \"a.dtd\">\n<a/>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(outside.path(), VERIFIED, &formats_as("<a/>\n"));

    let json = check(&project, outside.path(), &[]);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert!(
        at(&json, "/findings/0/message")
            .as_str()
            .is_some_and(|message| message.contains("so its format and lint steps didn't run")),
        "{json}"
    );
    assert!(!outside.path().join("ran").exists(), "xmllint ran");
}

#[test]
fn xml_an_unverified_xmllint_runs_neither_step() {
    let project = project_with(XML_BOTH, "xml", "<a/>");
    let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
    stub_xmllint(
        outside.path(),
        "21600",
        &formats_as("<?xml version=\"1.0\"?>\n<a/>\n"),
    );

    let json = check(&project, outside.path(), &[]);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(
        at(&json, "/data/summary/code_blocks/format_not_run"),
        1,
        "{json}"
    );
    assert_eq!(
        at(&json, "/data/summary/code_blocks/lint_not_run"),
        1,
        "{json}"
    );
    assert!(!outside.path().join("args").exists(), "a step ran");
}

#[test]
fn xml_an_xhtml_block_xml_space_or_unescaped_non_ascii_is_never_formatted() {
    let cases = [
        ("xhtml", "<html><br></br></html>", "tagged `xhtml`"),
        (
            "xml",
            "<a xml:space=\"preserve\"><b/><c/></a>",
            "`xml:space`",
        ),
        ("svg", "<svg><text>\u{e9}</text></svg>", "non-ASCII"),
        (
            "svg",
            "<svg><foreignObject><div></div></foreignObject></svg>",
            "`foreignObject`",
        ),
        (
            "xml",
            "<!DOCTYPE a [<!ELEMENT a ANY>]>\n<a/>",
            "internal subset",
        ),
        (
            "xml",
            "\u{feff}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a/>",
            "byte order mark",
        ),
    ];
    for (tag, body, reason) in cases {
        let project = project_with(XML_BOTH, tag, body);
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        stub_xmllint(
            outside.path(),
            VERIFIED,
            &formats_as("<?xml version=\"1.0\"?>\n<x/>\n"),
        );

        let json = check(&project, outside.path(), &[]);
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", outside.path())]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{tag}: {json}"
        );
        assert!(
            at(&json, "/findings/0/message")
                .as_str()
                .is_some_and(|said| said.contains(reason)),
            "{tag}: expected a warning naming {reason:?}: {json}"
        );
        if reason == "byte order mark" {
            assert!(
                at(&json, "/findings/0/help")
                    .as_str()
                    .is_some_and(|help| help.starts_with("remove the byte order mark")),
                "the help names the byte order mark, not a declaration the block has: {json}"
            );
        }
        assert_eq!(
            at(&json, "/data/summary/code_blocks/format_not_run"),
            1,
            "{tag}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{tag}: {json}"
        );
        assert!(
            !args_of(outside.path()).contains("--format"),
            "{tag}: the format step ran"
        );
        assert_eq!(
            project.read("docs/blocks.md"),
            format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
            "{tag}"
        );
    }
}

#[test]
fn xml_the_real_xmllint_formats_odd_blocks_keeping_their_content_and_fix_settles_them() {
    let Some(dir) = real_tool_dir(
        "xmllint",
        "xml_the_real_xmllint_formats_odd_blocks_keeping_their_content_and_fix_settles_them",
    ) else {
        return;
    };
    // Each block xmllint rewrites, and what supdoc fix writes: only whitespace already between
    // nodes is re-indented, `<a></a>` becomes `<a/>`, and a character reference becomes its
    // character, the same parsed text; every text, comment and attribute value is kept.
    let rewritten = [
        ("<a>\n<b>x</b>\n</a>", "<a>\n  <b>x</b>\n</a>"),
        ("<a></a>", "<a/>"),
        ("<a>\n<!-- c -->\n</a>", "<a>\n  <!-- c -->\n</a>"),
        ("<a>\n<?pi d?>\n</a>", "<a>\n  <?pi d?>\n</a>"),
        (
            "<a>\n<b>\n<c/>\n</b>\n</a>",
            "<a>\n  <b>\n    <c/>\n  </b>\n</a>",
        ),
        ("<a>&#65;</a>", "<a>A</a>"),
        (
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a>\n<b>\u{e9}</b>\n</a>",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a>\n  <b>\u{e9}</b>\n</a>",
        ),
    ];
    for (body, fixed) in rewritten {
        let project = project_with(XML_BOTH, "xml", body);

        let json = check(&project, &dir, &[]);
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);
        let again = check(&project, &dir, &[]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-unformatted"],
            "{body:?}: {json}"
        );
        assert_eq!(
            project.read("docs/blocks.md"),
            format!("# Blocks\n\n```xml\n{fixed}\n```\n"),
            "{body:?}"
        );
        assert!(
            finding_kinds(&again).is_empty(),
            "{body:?} after fix: {again}"
        );
        assert_eq!(
            at(&again, "/data/summary/code_blocks/passed"),
            1,
            "{body:?}: {again}"
        );
    }
    // Each block xmllint leaves as written: text, CDATA and attribute values xq used to damage.
    let kept = [
        "<a>]]&gt;</a>",
        "<a>  x  </a>",
        "<a><![CDATA[ <x>  y ]]></a>",
        "<a b=\"&lt;&#10;x\"/>",
        "<p>text <b>bold</b> tail</p>",
        "<a>&amp;&lt;</a>",
        "<!DOCTYPE a>\n<a/>",
        "<!-- top -->\n<a/>\n<!-- tail -->",
        "<?xml version=\"1.0\" standalone=\"yes\"?>\n<a/>",
    ];
    for body in kept {
        let project = project_with(XML_BOTH, "xml", body);

        let json = check(&project, &dir, &[]);

        assert!(finding_kinds(&json).is_empty(), "{body:?}: {json}");
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{body:?}: {json}"
        );
    }
}

#[test]
fn xml_the_real_xmllint_never_formats_a_block_whose_meaning_or_spelling_it_would_change() {
    let Some(dir) = real_tool_dir(
        "xmllint",
        "xml_the_real_xmllint_never_formats_a_block_whose_meaning_or_spelling_it_would_change",
    ) else {
        return;
    };
    // xmllint indents inside `xml:space="preserve"`, writes `é` as `&#xE9;` without a UTF-8
    // declaration, writes `<br></br>` as `<br/>`, which XHTML served as HTML misreads, and
    // rewrites an internal subset. It adds whitespace where the block has none (between adjacent
    // tspans it renders as a space), and drops whitespace beside CDATA or a comment, which
    // supdoc's comparison of the two trees catches.
    let cases = [
        ("xml", "<a xml:space=\"preserve\"><b/><c/></a>"),
        ("xml", "<a xml:space=\"preserve\"><b>\n <c/></b></a>"),
        ("xml", "<a><b>\u{e9}</b></a>"),
        ("xml", "<?xml version=\"1.0\"?>\n<a b=\"\u{2713}\"/>"),
        ("xhtml", "<html><body><br></br></body></html>"),
        (
            "svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><foreignObject>\
             <div xmlns=\"http://www.w3.org/1999/xhtml\"></div></foreignObject></svg>",
        ),
        ("xml", "<!DOCTYPE a [<!ELEMENT a ( b | c )* >]>\n<a/>"),
        (
            "svg",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><text><tspan>Hel</tspan>\
             <tspan>lo</tspan></text></svg>",
        ),
        ("xml", "<a>\n  <![CDATA[x]]>\n</a>"),
        ("xml", "<a>\n  <!-- c -->\n  text\n</a>"),
        ("xml", "<a><b>x</b></a>"),
        ("xml", "<p><b>x</b> <i>y</i></p>"),
    ];
    for (tag, body) in cases {
        let project = project_with(XML_BOTH, tag, body);

        let json = check(&project, &dir, &[]);
        let _fixed = project.supdoc_in_with(".", &["fix", "--yes"], &[("PATH", &dir)]);

        assert_eq!(
            finding_kinds(&json),
            ["code-block-not-checked"],
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/format_not_run"),
            1,
            "{body:?}: {json}"
        );
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            1,
            "{body:?}: {json}"
        );
        assert_eq!(
            project.read("docs/blocks.md"),
            format!("# Blocks\n\n```{tag}\n{body}\n```\n"),
            "{body:?}"
        );
    }
}
