//! Contract tests for output (07 "Tests"): every format of a report, rendered from constructed
//! values.

use std::collections::BTreeSet;
use std::io::{self, Write};

use serde_json::Value;

use crate::finding::{Finding, Location, Severity};
use crate::output::fixtures::{
    Capture, absolute, captured, check_report, context, finding_on, paths, settings, source,
};
use crate::output::format::TextContext;
use crate::output::format::{OutputFormat, Verbosity, supports_unicode};
use crate::output::github::{annotation, escape_data, escape_property};
use crate::output::json::{Envelope, FatalJson, FindingJson};
use crate::output::jsonl;
use crate::output::paths::PathStyle;
use crate::output::report::Report;
use crate::output::sink::{Output, OutputError};
use crate::output::summary::Summary;
use crate::output::text::render_finding;

/// What `Output` wrote to stdout for `report` in `format`.
fn stdout_of(format: OutputFormat, verbosity: Verbosity, ok: bool) -> String {
    let (mut output, stdout, _stderr) = captured(settings(format, verbosity));
    output
        .report(&check_report(), ok)
        .expect("writing to a buffer should succeed");
    stdout.text()
}

/// The value at JSON pointer `pointer`.
fn at<'a>(value: &'a Value, pointer: &str) -> &'a Value {
    value
        .pointer(pointer)
        .unwrap_or_else(|| panic!("{value} should have a value at `{pointer}`"))
}

fn parse(line: &str) -> Value {
    serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("`{line}` should be valid JSON: {error}"))
}

#[test]
fn check_text_matches_golden() {
    let text = stdout_of(OutputFormat::Text, Verbosity::Normal, false);

    insta::assert_snapshot!(text);
}

#[test]
fn fatal_error_text_matches_golden() {
    let config = "extends = \"../../supdoc.confg.toml\"\n";
    let finding = finding_on(
        "apps/web/supdoc.config.toml",
        config,
        "\"../../supdoc.confg.toml\"",
        "config-extends-missing",
        Severity::Error,
        "apps/web/supdoc.config.toml extends `../../supdoc.confg.toml`, but no file exists at supdoc.confg.toml",
        "point `extends` at an existing supdoc.config.toml, relative to this file",
    );
    let (mut output, _stdout, stderr) = captured(settings(OutputFormat::Text, Verbosity::Normal));

    output
        .fatal(&finding, 78)
        .expect("writing to a buffer should succeed");

    insta::assert_snapshot!(stderr.text());
}

#[test]
fn every_report_json_validates_against_its_schema() {
    let report = check_report();
    let schema = serde_json::to_value(schemars::schema_for!(crate::output::fixtures::CheckFixture))
        .expect("a schema should serialize");
    let validator = jsonschema::validator_for(&schema).expect("the schema should compile");
    let envelope = parse(&stdout_of(OutputFormat::Json, Verbosity::Normal, false));

    let errors: Vec<String> = validator
        .iter_errors(at(&envelope, "/data"))
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "data should match its schema: {errors:?}"
    );

    let finding_schema = serde_json::to_value(schemars::schema_for!(FindingJson))
        .expect("a schema should serialize");
    let finding_validator =
        jsonschema::validator_for(&finding_schema).expect("the schema should compile");
    for finding in &at(&envelope, "/findings")
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        assert!(
            finding_validator.is_valid(finding),
            "{finding} should match the finding schema"
        );
    }
    assert_eq!(
        at(&envelope, "/findings").as_array().map(Vec::len),
        Some(report.loose.len())
    );

    let fatal_schema =
        serde_json::to_value(schemars::schema_for!(FatalJson)).expect("a schema should serialize");
    let fatal_validator =
        jsonschema::validator_for(&fatal_schema).expect("the schema should compile");
    let fatal = serde_json::to_value(FatalJson::new(
        report
            .loose
            .first()
            .expect("the fixture has a loose finding"),
        78,
    ))
    .expect("a fatal error should serialize");
    assert!(
        fatal_validator.is_valid(&fatal),
        "{fatal} should match its schema"
    );
}

#[test]
fn envelope_has_the_same_top_level_keys_for_every_command() {
    let expected: BTreeSet<&str> = [
        "schema_version",
        "command",
        "ok",
        "root",
        "data",
        "findings",
        "truncated",
    ]
    .into();
    for command in ["check", "list", "update"] {
        let mut report = check_report();
        report.command = command;
        let envelope = Envelope::new(&report, false, &absolute("/repo"));
        let value = serde_json::to_value(&envelope).expect("an envelope should serialize");
        let keys: BTreeSet<&str> = value
            .as_object()
            .expect("an envelope should be an object")
            .keys()
            .map(String::as_str)
            .collect();

        assert_eq!(keys, expected, "top-level keys for `{command}`");
        assert_eq!(at(&value, "/command"), command);
        assert_eq!(at(&value, "/schema_version"), 1);
        assert_eq!(at(&value, "/root"), "/repo");
    }
}

#[test]
fn json_nests_a_links_finding_and_keeps_only_loose_findings_at_the_top() {
    let envelope = parse(&stdout_of(OutputFormat::Json, Verbosity::Normal, false));

    assert_eq!(at(&envelope, "/data/links/0/finding/kind"), "link-failed");
    assert_eq!(at(&envelope, "/findings/0/kind"), "scan-unreadable-dir");
    assert!(envelope.pointer("/findings/0/location").is_none());
    assert_eq!(at(&envelope, "/truncated"), false);
}

#[test]
fn jsonl_lines_each_parse_and_end_with_summary() {
    let text = stdout_of(OutputFormat::Jsonl, Verbosity::Normal, false);
    let lines: Vec<Value> = text.lines().map(parse).collect();

    let types: Vec<&str> = lines
        .iter()
        .map(|line| at(line, "/type").as_str().expect("every line has a `type`"))
        .collect();
    assert_eq!(
        types,
        ["link", "link", "link", "link", "finding", "summary"],
        "one record per link, then loose findings, then the summary"
    );
    let summary = lines.last().expect("there should be a summary line");
    assert_eq!(at(summary, "/command"), "check");
    assert_eq!(at(summary, "/ok"), false);
    assert_eq!(at(summary, "/passed"), 38);
    assert_eq!(at(summary, "/failed"), 1);
    assert_eq!(at(summary, "/schema_version"), 1);
}

/// A report with data scalars, like `update`'s, and one `changes` array.
#[derive(serde::Serialize, schemars::JsonSchema)]
struct ScalarFixture {
    applied: bool,
    file: String,
    ok: String,
    changes: Vec<String>,
}

impl Report for ScalarFixture {
    fn command(&self) -> &'static str {
        "update"
    }

    fn render_text(&self, _context: &TextContext, _out: &mut dyn Write) -> io::Result<()> {
        Ok(())
    }
}

fn scalar_fixture_lines() -> Vec<Value> {
    let report = ScalarFixture {
        applied: false,
        file: "supdoc.lock".to_owned(),
        ok: "data's own ok".to_owned(),
        changes: vec!["one".to_owned(), "two".to_owned()],
    };
    jsonl::lines(&report, true)
}

#[test]
fn jsonl_summary_keeps_the_data_scalars_after_the_change_records() {
    let lines = scalar_fixture_lines();

    let types: Vec<&str> = lines
        .iter()
        .map(|line| at(line, "/type").as_str().expect("every line has a `type`"))
        .collect();
    assert_eq!(types, ["change", "change", "summary"]);
    let summary = lines.last().expect("there should be a summary line");
    assert_eq!(at(summary, "/applied"), false);
    assert_eq!(at(summary, "/file"), "supdoc.lock");
    assert!(
        summary.get("changes").is_none(),
        "arrays are records, never summary fields: {summary}"
    );
}

#[test]
fn jsonl_summary_envelope_keys_win_over_same_named_data_scalars() {
    let lines = scalar_fixture_lines();

    let summary = lines.last().expect("there should be a summary line");
    assert_eq!(at(summary, "/ok"), true);
    assert_eq!(at(summary, "/command"), "update");
    assert_eq!(at(summary, "/type"), "summary");
}

#[test]
fn github_lines_escape_percent_newline_colon_comma() {
    assert_eq!(
        escape_data("100%\r\nsure: yes, really"),
        "100%25%0D%0Asure: yes, really"
    );
    assert_eq!(escape_property("a:b,c%d\ne"), "a%3Ab%2Cc%25d%0Ae");

    let text = "see [x](a.rs#b)\n";
    let finding = finding_on(
        "docs/a,b: c.md",
        text,
        "[x](a.rs#b)",
        "link-failed",
        Severity::Error,
        "50% changed\nsecond line",
        "run `supdoc update --yes`",
    );
    let line = annotation(&paths(), &finding);

    assert_eq!(
        line,
        "::error file=docs/a%2Cb%3A c.md,line=1,col=5,title=supdoc link-failed::50%25 changed%0Asecond line. Run `supdoc update --yes`"
    );
    assert_eq!(line.lines().count(), 1, "an annotation is exactly one line");
}

#[test]
fn github_output_maps_severity_and_ends_with_the_summary_line() {
    let text = stdout_of(OutputFormat::Github, Verbosity::Normal, false);
    let lines: Vec<&str> = text.lines().collect();

    let starts = |index: usize, prefix: &str| {
        lines
            .get(index)
            .is_some_and(|line| line.starts_with(prefix))
    };
    assert!(starts(
        0,
        "::error file=docs/guide.md,line=3,col=5,title=supdoc link-failed::"
    ));
    assert!(starts(1, "::error file=docs/api.md,line=3,col=3,"));
    assert!(starts(
        2,
        "::warning file=supdoc.lock,title=supdoc link-obsolete::"
    ));
    assert_eq!(
        lines.last().copied(),
        Some("Links  1 failed, 1 broken, 38 passed, 1 obsolete")
    );
    let note = Finding::new("link-moved", Severity::Note, "moved");
    assert!(annotation(&paths(), &note).starts_with("::notice title=supdoc link-moved::moved"));
}

#[test]
fn an_obsolete_annotation_names_the_lockfile() {
    let style = PathStyle::new(
        absolute("/repo/apps/web"),
        absolute("/repo/apps/web"),
        Some(absolute("/repo")),
    );
    let obsolete = Finding::new(
        "link-obsolete",
        Severity::Warning,
        "supdoc.lock has an entry for `docs/old.md → src/old.rs#parse`, which no longer exists",
    )
    .with_help("run `supdoc update --state obsolete --yes`");

    assert!(
        annotation(&style, &obsolete)
            .starts_with("::warning file=apps/web/supdoc.lock,title=supdoc link-obsolete::"),
        "an obsolete annotation should point at the lockfile: {}",
        annotation(&style, &obsolete)
    );
}

#[test]
fn github_paths_are_relative_to_workspace() {
    let style = PathStyle::new(
        absolute("/repo/apps/web"),
        absolute("/repo/apps/web"),
        Some(absolute("/repo")),
    );

    assert_eq!(
        style.github(&source("docs/guide.md")),
        "apps/web/docs/guide.md"
    );
}

#[test]
fn github_paths_fall_back_to_the_project_root_without_a_workspace() {
    let style = PathStyle::new(absolute("/repo"), absolute("/repo/apps/web"), None);

    assert_eq!(style.github(&source("docs/guide.md")), "docs/guide.md");
}

#[test]
fn text_paths_are_relative_to_cwd() {
    let root = absolute("/repo/apps/web");
    let guide = source("docs/guide.md");
    let from = |cwd: &str| PathStyle::new(absolute(cwd), root.clone(), None).text(&guide);

    assert_eq!(from("/repo/apps/web"), "docs/guide.md");
    assert_eq!(from("/repo/apps/web/docs"), "guide.md");
    assert_eq!(from("/repo/apps/web/src"), "../docs/guide.md");
    assert_eq!(from("/repo"), "apps/web/docs/guide.md");
}

#[test]
fn json_paths_are_relative_to_the_project_root() {
    assert_eq!(PathStyle::json(&source("docs/guide.md")), "docs/guide.md");
}

#[test]
fn findings_on_stdout_fatal_errors_on_stderr() {
    let (mut output, stdout, stderr) = captured(settings(OutputFormat::Text, Verbosity::Normal));
    let fatal = Finding::new("config-not-found", Severity::Error, "no config file")
        .with_help("run `supdoc init`");

    output
        .report(&check_report(), false)
        .expect("writing to a buffer should succeed");
    output
        .fatal(&fatal, 78)
        .expect("writing to a buffer should succeed");

    assert!(stdout.text().contains("error[link-failed]"));
    assert!(!stdout.text().contains("config-not-found"));
    assert!(
        stderr
            .text()
            .contains("error[config-not-found]: no config file")
    );
    assert!(!stderr.text().contains("link-failed"));
}

#[test]
fn fatal_errors_are_json_on_stderr_under_json_formats() {
    for format in [OutputFormat::Json, OutputFormat::Jsonl] {
        let (mut output, stdout, stderr) = captured(settings(format, Verbosity::Normal));
        let text = "extends = \"x.toml\"\n";
        let fatal = finding_on(
            "supdoc.config.toml",
            text,
            "\"x.toml\"",
            "config-extends-missing",
            Severity::Error,
            "no file",
            "point `extends` at a file",
        );

        output
            .fatal(&fatal, 78)
            .expect("writing to a buffer should succeed");

        let error = parse(stderr.text().trim_end());
        assert_eq!(at(&error, "/error"), true);
        assert_eq!(at(&error, "/kind"), "config-extends-missing");
        assert_eq!(at(&error, "/exit_code"), 78);
        assert_eq!(at(&error, "/location/file"), "supdoc.config.toml");
        assert_eq!(at(&error, "/location/column"), 11);
        assert_eq!(stdout.text(), "", "nothing goes to stdout for {format:?}");
    }
}

#[test]
fn preview_on_stdout_nothing_written_note_on_stderr() {
    let (mut output, stdout, stderr) = captured(settings(OutputFormat::Text, Verbosity::Normal));

    output
        .report(&check_report(), false)
        .expect("writing to a buffer should succeed");
    output
        .note("Nothing written. Run the same command with --yes to apply.")
        .expect("writing to a buffer should succeed");

    assert!(!stdout.text().contains("Nothing written"));
    assert_eq!(
        stderr.text(),
        "Nothing written. Run the same command with --yes to apply.\n"
    );
}

#[test]
fn quiet_shows_only_errors_and_no_summary() {
    let text = stdout_of(OutputFormat::Text, Verbosity::Quiet, false);

    assert!(text.contains("error[link-failed]"));
    assert!(text.contains("error[resolve-not-found]"));
    assert!(
        !text.contains("warning["),
        "warnings are hidden under -q:\n{text}"
    );
    assert!(!text.contains("Links"), "no summary under -q:\n{text}");

    let (mut output, _stdout, stderr) = captured(settings(OutputFormat::Text, Verbosity::Quiet));
    output
        .note("Nothing written.")
        .expect("writing to a buffer should succeed");
    output
        .diag(&Finding::new("scan-x", Severity::Warning, "hidden"))
        .expect("writing to a buffer should succeed");
    output
        .diag(&Finding::new("scan-y", Severity::Error, "shown"))
        .expect("writing to a buffer should succeed");
    assert!(stderr.text().contains("error[scan-y]: shown"));
    assert!(!stderr.text().contains("Nothing written"));
    assert!(!stderr.text().contains("hidden"));
}

#[test]
fn verbose_lists_passed_links() {
    let normal = stdout_of(OutputFormat::Text, Verbosity::Normal, false);
    let verbose = stdout_of(OutputFormat::Text, Verbosity::Verbose, false);

    assert!(!normal.contains("passed  docs/setup.md"));
    assert!(verbose.contains("passed  docs/setup.md -> src/config.rs"));
}

#[test]
fn dumb_terminal_uses_ascii_glyphs() {
    assert!(supports_unicode(true, Some("xterm-256color")));
    assert!(supports_unicode(true, None));
    assert!(!supports_unicode(true, Some("dumb")));
    assert!(!supports_unicode(false, Some("xterm-256color")));

    let render = |unicode: bool| {
        let mut context = context(Verbosity::Normal);
        context.unicode = unicode;
        let mut bytes = Vec::new();
        render_finding(
            &context,
            check_report()
                .links
                .first()
                .expect("the fixture has links")
                .raw
                .as_ref()
                .expect("the first link carries a finding"),
            &mut bytes,
        )
        .expect("writing to a buffer should succeed");
        String::from_utf8(bytes).expect("output should be UTF-8")
    };

    assert!(render(false).is_ascii(), "ASCII mode should be pure ASCII");
    assert!(
        !render(true).is_ascii(),
        "Unicode mode should draw box characters"
    );
}

#[test]
fn color_adds_ansi_codes_and_plain_does_not() {
    let finding = check_report().loose.remove(0);
    let render = |color: bool| {
        let mut context = context(Verbosity::Normal);
        context.color = color;
        let mut bytes = Vec::new();
        render_finding(&context, &finding, &mut bytes).expect("writing to a buffer should succeed");
        String::from_utf8(bytes).expect("output should be UTF-8")
    };

    assert!(render(true).contains('\u{1b}'));
    assert!(!render(false).contains('\u{1b}'));
}

#[test]
fn summary_omits_zero_counts_and_orders_worst_first() {
    let summary = Summary {
        passed: 38,
        failed: 1,
        broken: 1,
        new: 0,
        obsolete: 1,
    };
    assert_eq!(
        summary.line(false, true),
        "Links  1 failed · 1 broken · 38 passed · 1 obsolete"
    );
    assert_eq!(
        Summary {
            passed: 40,
            ..Summary::default()
        }
        .line(false, true),
        "Links  40 passed"
    );
    assert_eq!(
        Summary {
            new: 2,
            failed: 3,
            ..Summary::default()
        }
        .line(false, true),
        "Links  3 failed · 2 new"
    );
    assert_eq!(Summary::default().line(false, true), "Links  0 passed");
}

#[test]
fn ascii_summary_separates_counts_with_commas() {
    let summary = Summary {
        failed: 1,
        passed: 2,
        ..Summary::default()
    };

    assert_eq!(summary.line(false, false), "Links  1 failed, 2 passed");
}

#[test]
fn summary_counts_states_and_colors_only_when_asked() {
    use crate::state::LinkState;

    let summary = Summary::tally([LinkState::Passed, LinkState::Passed, LinkState::Failed]);

    assert_eq!((summary.passed, summary.failed), (2, 1));
    assert!(summary.line(true, true).contains('\u{1b}'));
    assert!(!summary.line(false, true).contains('\u{1b}'));
}

/// A writer whose reader has hung up.
struct ClosedPipe;

impl Write for ClosedPipe {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer that fails for a reason other than a closed pipe.
struct FullDisk;

impl Write for FullDisk {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("no space left on device"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_closed_pipe_is_a_quiet_clean_stop() {
    let stderr = Capture::default();
    let mut output = Output::new(
        settings(OutputFormat::Text, Verbosity::Normal),
        Box::new(ClosedPipe),
        Box::new(stderr.clone()),
    );

    output
        .report(&check_report(), false)
        .expect("a closed pipe should not be an error");

    assert!(output.is_closed());
    output
        .note("later")
        .expect("writes after a closed pipe are dropped");
    assert_eq!(stderr.text(), "", "nothing more is written once closed");
}

#[test]
fn other_write_failures_are_reported() {
    let mut output = Output::new(
        settings(OutputFormat::Text, Verbosity::Normal),
        Box::new(FullDisk),
        Box::new(Capture::default()),
    );

    let error = output
        .report(&check_report(), false)
        .expect_err("a failed write should be an error");

    assert!(matches!(error, OutputError::Write(_)));
    assert!(!output.is_closed());
}

#[test]
fn a_finding_without_a_snippet_still_shows_its_location() {
    let finding = Finding::new(
        "read-missing",
        Severity::Error,
        "src/gone.rs does not exist",
    )
    .with_location(Location {
        file: source("docs/guide.md"),
        range: 10..20,
        line: 4,
        column: 7,
    })
    .with_help("restore the file or run `supdoc fix docs/guide.md`");
    let mut bytes = Vec::new();

    render_finding(&context(Verbosity::Normal), &finding, &mut bytes)
        .expect("writing to a buffer should succeed");

    let text = String::from_utf8(bytes).expect("output should be UTF-8");
    assert!(text.contains("--> docs/guide.md:4:7"), "{text}");
    assert!(text.contains("= help: restore the file"), "{text}");
}

#[test]
fn control_characters_from_files_are_rendered_escaped_in_text() {
    let text = "see \u{1b}[31m[x](a.rs#b)\n";
    let finding = finding_on(
        "docs/a.md",
        text,
        "[x](a.rs#b)",
        "link-failed",
        Severity::Error,
        "bad \u{1b}]0;pwned\u{7} message",
        "try \u{1b}[2J instead",
    );
    let mut bytes = Vec::new();

    render_finding(&context(Verbosity::Normal), &finding, &mut bytes)
        .expect("writing to a buffer should succeed");

    let rendered = String::from_utf8(bytes).expect("output should be UTF-8");
    assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    assert!(!rendered.contains('\u{7}'), "{rendered:?}");
    assert!(
        rendered.contains("bad \\u{1b}]0;pwned\\u{7} message"),
        "{rendered}"
    );
    assert!(rendered.contains("try \\u{1b}[2J instead"), "{rendered}");
    assert!(
        rendered.contains("see \\u{1b}[31m[x](a.rs#b)"),
        "{rendered}"
    );
}

#[test]
fn a_message_cannot_forge_a_workflow_command_under_github_format() {
    let hostile = Finding::new(
        "link-failed",
        Severity::Error,
        "oops\n::error::x\r::warning::y",
    );
    let (mut output, stdout, stderr) = captured(settings(OutputFormat::Github, Verbosity::Normal));

    output
        .diag(&hostile)
        .expect("writing to a buffer should succeed");
    output
        .fatal(&hostile, 1)
        .expect("writing to a buffer should succeed");
    output
        .note("done\n::error::forged")
        .expect("writing to a buffer should succeed");

    let emitted = stderr.text();
    assert_eq!(stdout.text(), "");
    let commands: Vec<&str> = emitted
        .lines()
        .filter(|line| line.starts_with("::"))
        .collect();
    assert_eq!(emitted.lines().count(), 3, "{emitted}");
    assert_eq!(
        commands,
        vec![
            "::error title=supdoc link-failed::oops%0A::error::x%0D::warning::y",
            "::error title=supdoc link-failed::oops%0A::error::x%0D::warning::y",
        ]
    );
    assert!(emitted.contains("done%0A::error::forged"), "{emitted}");
}

fn rendered_text(finding: &Finding) -> String {
    let mut bytes = Vec::new();
    render_finding(&context(Verbosity::Normal), finding, &mut bytes)
        .expect("writing to a buffer should succeed");
    String::from_utf8(bytes).expect("output should be UTF-8")
}

#[test]
fn bidi_and_invisible_characters_are_rendered_escaped_in_every_text_field() {
    let hostile = [
        ('\u{202E}', "\\u{202e}"),
        ('\u{2066}', "\\u{2066}"),
        ('\u{200B}', "\\u{200b}"),
        ('\u{2028}', "\\u{2028}"),
    ];
    for (character, escaped) in hostile {
        let text = format!("see a{character}b [x](a.rs#b)\n");
        let finding = finding_on(
            &format!("docs/a{character}.md"),
            &text,
            "[x](a.rs#b)",
            "link-failed",
            Severity::Error,
            &format!("bad{character}message"),
            &format!("help{character}text"),
        );

        let with_snippet = rendered_text(&finding);
        let without_snippet = rendered_text(&Finding {
            snippet: None,
            ..finding
        });

        for rendered in [with_snippet, without_snippet] {
            assert!(!rendered.contains(character), "{escaped}: {rendered:?}");
            for field in ["bad", "help", "docs/a"] {
                assert!(
                    rendered.contains(&format!("{field}{escaped}")),
                    "{escaped} in {field}: {rendered}"
                );
            }
        }
    }
}

#[test]
fn a_snippet_escapes_bidi_characters_and_carriage_returns() {
    let text = "see a\u{202E}b\r [x](a.rs#b)\n";
    let finding = finding_on(
        "docs/a.md",
        text,
        "[x](a.rs#b)",
        "link-failed",
        Severity::Error,
        "bad",
        "help",
    );

    let rendered = rendered_text(&finding);

    assert!(!rendered.contains('\u{202E}'), "{rendered:?}");
    assert!(!rendered.contains('\r'), "{rendered:?}");
    assert!(
        rendered.contains("see a\\u{202e}b\\r [x](a.rs#b)"),
        "{rendered}"
    );
}

#[test]
fn newlines_in_a_message_or_help_do_not_start_a_new_output_line() {
    let finding = Finding::new("link-failed", Severity::Error, "bad\nerror: forged\rmore")
        .with_help("try\nhelp: forged\r");

    let rendered = rendered_text(&finding);

    assert!(!rendered.contains('\r'), "{rendered:?}");
    assert!(
        rendered.contains("bad\\nerror: forged\\rmore"),
        "{rendered}"
    );
    assert!(rendered.contains("try\\nhelp: forged\\r"), "{rendered}");
    assert!(
        rendered
            .lines()
            .all(|line| !line.starts_with("error: forged") && !line.starts_with("help: forged")),
        "{rendered}"
    );
}

#[test]
fn a_note_cannot_start_a_new_line_or_hide_text_in_text_format() {
    let (mut output, _stdout, stderr) = captured(settings(OutputFormat::Text, Verbosity::Normal));

    output
        .note("done\n\u{202E}forged\r\u{2028}")
        .expect("writing to a buffer should succeed");

    assert_eq!(stderr.text(), "done\\n\\u{202e}forged\\r\\u{2028}\n");
}

/// Output for a run in `/repo/apps/web`, with `/repo` as the GitHub workspace, that has only
/// the provisional root (the current directory) until a command calls `set_root`.
fn output_in_subdirectory(format: OutputFormat) -> (Output, Capture) {
    let mut settings = settings(format, Verbosity::Normal);
    settings.paths = PathStyle::new(
        absolute("/repo/apps/web"),
        absolute("/repo/apps/web"),
        Some(absolute("/repo")),
    );
    let (output, stdout, _stderr) = captured(settings);
    (output, stdout)
}

#[test]
fn set_root_keeps_text_paths_relative_to_the_cwd() {
    let (mut output, stdout) = output_in_subdirectory(OutputFormat::Text);
    output.set_root(absolute("/repo"));

    output
        .report(&check_report(), false)
        .expect("writing to a buffer should succeed");

    let text = stdout.text();
    assert!(
        text.contains("../../docs/guide.md"),
        "text paths should climb from /repo/apps/web to the root's docs/guide.md:\n{text}"
    );
}

#[test]
fn set_root_makes_github_paths_workspace_relative() {
    let (mut output, stdout) = output_in_subdirectory(OutputFormat::Github);
    output.set_root(absolute("/repo"));

    output
        .report(&check_report(), false)
        .expect("writing to a buffer should succeed");

    let text = stdout.text();
    assert!(
        text.contains("file=docs/guide.md,"),
        "GitHub paths should be relative to the workspace /repo:\n{text}"
    );
    assert!(
        !text.contains("apps/web/docs"),
        "no GitHub path should keep the provisional root:\n{text}"
    );
}

#[test]
fn set_root_makes_json_root_and_paths_project_relative() {
    let (mut output, stdout) = output_in_subdirectory(OutputFormat::Json);
    output.set_root(absolute("/repo"));

    output
        .report(&check_report(), false)
        .expect("writing to a buffer should succeed");

    let envelope = parse(stdout.text().trim_end());
    assert_eq!(at(&envelope, "/root"), "/repo");
    assert_eq!(
        PathStyle::json(&source("docs/guide.md")),
        "docs/guide.md",
        "JSON paths are the project-relative source paths"
    );
    assert!(
        envelope.to_string().contains("\"docs/guide.md\""),
        "the envelope should name docs/guide.md relative to the root: {envelope}"
    );
}

#[test]
fn an_artifact_is_raw_in_text_and_github_and_wrapped_in_json() {
    let artifact = "# not a report\n";
    let report = ScalarFixture {
        applied: false,
        file: artifact.to_owned(),
        ok: String::new(),
        changes: Vec::new(),
    };
    for format in [OutputFormat::Text, OutputFormat::Github] {
        let (mut output, stdout, stderr) = captured(settings(format, Verbosity::Quiet));

        output
            .artifact(&report, artifact, true)
            .expect("writing to a buffer should succeed");

        assert_eq!(stdout.text(), artifact, "{format:?} should write it raw");
        assert_eq!(stderr.text(), "", "{format:?} should not touch stderr");
    }
    for format in [OutputFormat::Json, OutputFormat::Jsonl] {
        let (mut output, stdout, _stderr) = captured(settings(format, Verbosity::Quiet));

        output
            .artifact(&report, artifact, true)
            .expect("writing to a buffer should succeed");

        assert!(
            stdout.text().contains("\"file\":\"# not a report\\n\""),
            "{format:?} should carry the artifact as report data: {}",
            stdout.text()
        );
    }
}
