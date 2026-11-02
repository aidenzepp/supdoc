//! `[limits]` as `config show --json` reports it: which of flag, environment variable, config
//! file or default set each ceiling.

use std::path::Path;

use crate::support::{Project, at};

#[test]
fn a_flag_beats_its_environment_variable() {
    let project = Project::fixture("basic");
    let run = project.supdoc_in_with(
        ".",
        &["config", "show", "--json", "--parse-timeout", "7s"],
        &[("SUPDOC_PARSE_TIMEOUT", Path::new("5s"))],
    );
    let json = run.assert_exit(0).json();

    assert_eq!(at(&json, "/data/limits/parse_timeout/value"), "7s");
    assert_eq!(at(&json, "/data/limits/parse_timeout/source"), "flag");
}

#[test]
fn an_environment_variable_beats_the_config_file() {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "[limits]\nparse-timeout = \"30s\"\n");
    let run = project.supdoc_in_with(
        ".",
        &["config", "show", "--json"],
        &[("SUPDOC_PARSE_TIMEOUT", Path::new("5s"))],
    );
    let json = run.assert_exit(0).json();

    assert_eq!(at(&json, "/data/limits/parse_timeout/value"), "5s");
    assert_eq!(at(&json, "/data/limits/parse_timeout/source"), "env");
}

#[test]
fn a_flag_after_the_subcommand_still_counts_as_a_flag() {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "[limits]\nmax-lock-size = \"1MiB\"\n");
    let json = project
        .supdoc(&["config", "show", "--json", "--max-lock-size", "2MiB"])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/limits/max_lock_size/value"), "2MiB");
    assert_eq!(at(&json, "/data/limits/max_lock_size/source"), "flag");
}

#[test]
fn a_config_file_value_reports_the_file_that_set_it() {
    let project = Project::fixture("basic");
    project.write(
        "supdoc.config.toml",
        "[limits]\nmax-ignore-size = \"1KiB\"\n",
    );
    let json = project
        .supdoc(&["config", "show", "--json"])
        .assert_exit(0)
        .json();

    assert_eq!(at(&json, "/data/limits/max_ignore_size/value"), "1KiB");
    assert_eq!(
        at(&json, "/data/limits/max_ignore_size/file"),
        "supdoc.config.toml"
    );
}

/// Rust nested deeply enough that its parse outlasts 1 ms. The comment that opens it puts a
/// deadline check right after the parse, so even a read that only looks for links meets it.
fn deep_rust() -> String {
    format!(
        "// [x](../docs/guide.md)\nfn deep() {{{}\n",
        "{".repeat(65_536)
    )
}

/// The `basic` fixture, recorded, plus `src/deep.rs`, which no link points at.
fn project_with_deep_source() -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write("src/deep.rs", &deep_rust());
    project
}

/// Asserts that `run` failed, reporting that reading `file` outlasted the per-read timeout.
fn assert_timed_out(run: &crate::support::Run, file: &str) {
    let _ = run.assert_exit(1);
    let printed = format!("{}{}", run.stdout, run.stderr);
    assert!(
        printed.contains(&format!(
            "reading {file} took longer than the 1ms per-read parse timeout"
        )),
        "the run should report that reading {file} timed out:\n{printed}",
    );
}

#[test]
fn a_source_that_outlasts_the_parse_timeout_fails_check() {
    let project = project_with_deep_source();

    let run = project.supdoc(&["--parse-timeout", "1ms", "check"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn the_config_key_reaches_the_reader() {
    let project = project_with_deep_source();
    project.write("supdoc.config.toml", "[limits]\nparse-timeout = \"1ms\"\n");

    let run = project.supdoc(&["check"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn the_environment_variable_reaches_the_reader() {
    let project = project_with_deep_source();

    let run = project.supdoc_in_with(
        ".",
        &["check"],
        &[("SUPDOC_PARSE_TIMEOUT", Path::new("1ms"))],
    );

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn update_exits_1_on_a_read_timeout() {
    let project = project_with_deep_source();

    let run = project.supdoc(&["--parse-timeout", "1ms", "update", "--yes"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn fix_exits_1_on_a_read_timeout_and_never_says_nothing_to_fix() {
    let project = project_with_deep_source();

    for args in [
        &["--parse-timeout", "1ms", "fix"][..],
        &["--parse-timeout", "1ms", "fix", "--yes"],
    ] {
        let run = project.supdoc(args);

        assert_timed_out(&run, "src/deep.rs");
        assert!(
            !run.stderr.contains("Nothing to fix"),
            "a timed-out file was never checked for fixes:\n{}",
            run.stderr
        );
        assert!(
            run.stdout.contains("read-timeout"),
            "the finding should be printed:\n{}",
            run.stdout
        );
    }
}

#[test]
fn list_reports_a_timed_out_symbol_target_instead_of_matching_text() {
    let project = project_with_deep_source();
    project.write("docs/deep.md", "[deep](../src/deep.rs#deep)\n");

    let run = project.supdoc(&["--parse-timeout", "1ms", "list", "src/deep.rs#deep"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn rename_refuses_before_writing_when_a_source_times_out() {
    let project = Project::fixture("namespaced");
    project.record();
    project.write("src/deep.rs", &deep_rust());
    let before =
        ["supdoc.config.toml", "supdoc.lock", "docs/guide.md"].map(|file| project.read(file));

    let run = project.supdoc(&[
        "--parse-timeout",
        "1ms",
        "namespace",
        "rename",
        "auth",
        "authn",
        "--yes",
    ]);

    assert_timed_out(&run, "src/deep.rs");
    let after =
        ["supdoc.config.toml", "supdoc.lock", "docs/guide.md"].map(|file| project.read(file));
    assert_eq!(before, after, "a refused rename should write nothing");
}
