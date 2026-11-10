//! Checking XML code blocks, tagged `xml`, `svg`, `xsd`, `xsl`, `xslt` or `xhtml`, linted by
//! xmllint.

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

/// An `xml` table: no formatter, xmllint's lint step on.
const XML: &str = "[features.code-blocks]\n[features.code-blocks.xml]\n";

/// A recorded `basic` project with [`XML`] and `docs/blocks.md` holding one block tagged `tag`
/// holding `body`, whose first body line is the doc's line 4.
fn xml_project(tag: &str, body: &str) -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("supdoc.config.toml", XML);
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
        ("21504", false),
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
                    .is_some_and(|message| message
                        .contains("isn't a version supdoc's checks are verified against (2.9)")),
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
        let json = check(&xml_project("xml", body), &dir, &[]);
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
        let json = check(&xml_project("xml", body), &dir, &[]);
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
        let json = check(&xml_project("xml", body), &dir, &[]);
        assert_eq!(
            at(&json, "/data/summary/code_blocks/passed"),
            0,
            "{:?}: {json}",
            body.get(..40).unwrap_or(body)
        );
        assert!(
            !finding_kinds(&json).is_empty(),
            "{:?}: {json}",
            body.get(..40).unwrap_or(body)
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
    let project = xml_project(
        "xml",
        "<!DOCTYPE a [<!ENTITY e SYSTEM \"file:///etc/passwd\">]>\n<a>&e;</a>",
    );

    let json = check(&project, &dir, &[]);

    assert_eq!(finding_kinds(&json), ["code-block-not-checked"], "{json}");
    assert_eq!(at(&json, "/data/summary/code_blocks/passed"), 0, "{json}");
}
