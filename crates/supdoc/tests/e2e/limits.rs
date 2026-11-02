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
