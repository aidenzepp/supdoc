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
fn code_block_timeout_takes_flag_then_variable_then_config_then_default() {
    let project = Project::fixture("basic");
    let show = |args: &[&str], env: &[(&str, &Path)]| {
        project.supdoc_in_with(".", args, env).assert_exit(0).json()
    };
    let base = ["config", "show", "--json"];
    let env = [("SUPDOC_CODE_BLOCK_TIMEOUT", Path::new("5s"))];

    let json = show(&base, &[]);
    assert_eq!(at(&json, "/data/limits/code_block_timeout/value"), "10s");
    assert_eq!(
        at(&json, "/data/limits/code_block_timeout/source"),
        "default"
    );

    project.write(
        "supdoc.config.toml",
        "[limits]\ncode-block-timeout = \"30s\"\n",
    );
    let json = show(&base, &[]);
    assert_eq!(at(&json, "/data/limits/code_block_timeout/value"), "30s");
    assert_eq!(
        at(&json, "/data/limits/code_block_timeout/source"),
        "config"
    );

    let json = show(&base, &env);
    assert_eq!(at(&json, "/data/limits/code_block_timeout/value"), "5s");
    assert_eq!(at(&json, "/data/limits/code_block_timeout/source"), "env");

    let json = show(
        &["config", "show", "--json", "--code-block-timeout", "0"],
        &env,
    );
    assert_eq!(at(&json, "/data/limits/code_block_timeout/value"), "0");
    assert_eq!(at(&json, "/data/limits/code_block_timeout/source"), "flag");
}

#[test]
fn code_block_build_timeout_takes_flag_then_variable_then_config_then_default() {
    let project = Project::fixture("basic");
    let show = |args: &[&str], env: &[(&str, &Path)]| {
        project.supdoc_in_with(".", args, env).assert_exit(0).json()
    };
    let base = ["config", "show", "--json"];
    let env = [("SUPDOC_CODE_BLOCK_BUILD_TIMEOUT", Path::new("5s"))];

    let json = show(&base, &[]);
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/value"),
        "10m"
    );
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/source"),
        "default"
    );

    project.write(
        "supdoc.config.toml",
        "[limits]\ncode-block-build-timeout = \"30s\"\n",
    );
    let json = show(&base, &[]);
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/value"),
        "30s"
    );
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/source"),
        "config"
    );

    let json = show(&base, &env);
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/value"),
        "5s"
    );
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/source"),
        "env"
    );

    let json = show(
        &[
            "config",
            "show",
            "--json",
            "--code-block-build-timeout",
            "0",
        ],
        &env,
    );
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/value"),
        "0"
    );
    assert_eq!(
        at(&json, "/data/limits/code_block_build_timeout/source"),
        "flag"
    );
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
        at(&json, "/data/limits/max_ignore_size/from"),
        "supdoc.config.toml"
    );
}

/// The per-read budget the timeout tests set. It is far above what reading an ordinary file takes,
/// even on a loaded machine, so only `deep.rs` can outlast it; and far below the seconds
/// `deep.rs` needs, so it always does. A budget of 1 ms let some other read expire first.
const TIMEOUT: &str = "250ms";

/// Rust nested deeply enough that its parse outlasts [`TIMEOUT`] by orders of magnitude. The comment that opens it puts a
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

/// The `basic` fixture, recorded, plus a doc that links into `src/deep.rs#deep`. The source has no
/// link text, so only resolving the symbol (never reading `deep.rs` as a source) can time out.
fn project_with_deep_target() -> Project {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "src/deep.rs",
        &format!("fn deep() {{{}\n", "{".repeat(65_536)),
    );
    project.write("docs/deep.md", "[deep](../src/deep.rs#deep)\n");
    project
}

/// Asserts that `run` failed, reporting that reading `file` outlasted the per-read timeout.
fn assert_timed_out(run: &crate::support::Run, file: &str) {
    let _ = run.assert_exit(1);
    let printed = format!("{}{}", run.stdout, run.stderr);
    assert!(
        printed.contains(&format!(
            "reading {file} took longer than the {TIMEOUT} per-read parse timeout"
        )),
        "the run should report that reading {file} timed out:\n{printed}",
    );
}

#[test]
fn a_source_that_outlasts_the_parse_timeout_fails_check() {
    let project = project_with_deep_source();

    let run = project.supdoc(&["--parse-timeout", TIMEOUT, "check"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn the_config_key_reaches_the_reader() {
    let project = project_with_deep_source();
    project.write(
        "supdoc.config.toml",
        &format!("[limits]\nparse-timeout = \"{TIMEOUT}\"\n"),
    );

    let run = project.supdoc(&["check"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn the_environment_variable_reaches_the_reader() {
    let project = project_with_deep_source();

    let run = project.supdoc_in_with(
        ".",
        &["check"],
        &[("SUPDOC_PARSE_TIMEOUT", Path::new(TIMEOUT))],
    );

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn update_exits_1_on_a_read_timeout() {
    let project = project_with_deep_source();

    let run = project.supdoc(&["--parse-timeout", TIMEOUT, "update", "--yes"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn fix_exits_1_on_a_read_timeout_and_never_says_nothing_to_fix() {
    let project = project_with_deep_source();

    for args in [
        &["--parse-timeout", TIMEOUT, "fix"][..],
        &["--parse-timeout", TIMEOUT, "fix", "--yes"],
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
    let project = project_with_deep_target();

    let run = project.supdoc(&["--parse-timeout", TIMEOUT, "list", "src/deep.rs#deep"]);

    assert_timed_out(&run, "src/deep.rs");
}

#[test]
fn list_prints_the_timeout_even_when_a_filter_hides_the_timed_out_links() {
    let project = project_with_deep_target();

    for args in [
        &["--parse-timeout", TIMEOUT, "list", "docs/guide.md"][..],
        &["--parse-timeout", TIMEOUT, "list", "--state", "passed"],
        &["--parse-timeout", TIMEOUT, "list", "--limit", "1"],
    ] {
        let run = project.supdoc(args);

        assert_timed_out(&run, "src/deep.rs");
    }
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
        TIMEOUT,
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

/// Everything `run` wrote, stdout then stderr, for checks that don't care which stream.
fn all_output(run: &crate::support::Run) -> String {
    format!("{}{}", run.stdout, run.stderr)
}

/// Text just past the default 256 KiB ceiling for configs and ignore files.
fn oversized(first_line: &str) -> String {
    format!("{first_line}\n{}\n", "#".repeat(300_000))
}

#[test]
fn an_oversized_dot_ignore_at_the_root_is_refused_with_advice_to_shorten_it() {
    let project = Project::fixture("basic");
    project.write(".ignore", &oversized("gen/"));

    let run = project.supdoc(&["check"]);

    let _ = run.assert_exit(78);
    let output = all_output(&run);
    assert!(
        output.contains("ignore-too-large") && output.contains("or shorten the file"),
        "a root `.ignore` over the cap should be refused, and not sent to `exclude`:\n{output}"
    );
}

#[test]
fn an_oversized_nested_gitignore_suggests_excluding_its_directory() {
    let project = Project::fixture("basic");
    project.write("docs/.gitignore", &oversized("gen/"));

    let run = project.supdoc(&["check"]);

    let _ = run.assert_exit(78);
    let output = all_output(&run);
    assert!(
        output.contains("ignore-too-large") && output.contains("add its directory to `exclude`"),
        "a nested `.gitignore` over the cap should suggest excluding its directory:\n{output}"
    );
}

#[test]
fn an_oversized_git_exclude_file_is_refused() {
    let project = Project::fixture("basic");
    project.write(".git/HEAD", "ref: refs/heads/main\n");
    for dir in [".git/objects", ".git/refs"] {
        std::fs::create_dir_all(project.path(dir))
            .expect("the fixture git directory should be creatable");
    }
    project.write(".git/info/exclude", &oversized("gen/"));

    let run = project.supdoc(&["check"]);

    let _ = run.assert_exit(78);
    assert!(
        all_output(&run).contains("ignore-too-large"),
        "an oversized `.git/info/exclude` should be refused:\n{}",
        all_output(&run)
    );
}

#[test]
fn zero_lifts_the_ignore_size_ceiling() {
    let project = Project::fixture("basic");
    project.record();
    project.write(".gitignore", &oversized("gen/"));

    let _ = project
        .supdoc(&["--max-ignore-size", "0", "check"])
        .assert_exit(0);
}

#[test]
fn an_oversized_config_is_refused_and_zero_lifts_the_ceiling() {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "supdoc.config.toml",
        &oversized("# padded past the ceiling"),
    );

    let refused = project.supdoc(&["check"]);
    let lifted = project.supdoc_in_with(
        ".",
        &["check"],
        &[("SUPDOC_MAX_CONFIG_SIZE", Path::new("0"))],
    );

    let _ = refused.assert_exit(78);
    assert!(
        all_output(&refused).contains("config-too-large"),
        "a config over the cap should be refused:\n{}",
        all_output(&refused)
    );
    let _ = lifted.assert_exit(0);
}

#[test]
fn a_bare_number_limit_gets_help_showing_the_unit_form() {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "[limits]\nparse-timeout = 7\n");

    let run = project.supdoc(&["check"]);

    let _ = run.assert_exit(78);
    let output = all_output(&run);
    assert!(
        output.contains(
            "Write the limit as a whole number with its unit in quotes: a duration such as \
             `parse-timeout = \"10s\"` (also `ms` or `m`). `0` means no limit"
        ),
        "a bare number should get the unit-form help:\n{output}"
    );
}

#[test]
fn a_float_limit_gets_help_showing_both_unit_forms() {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "[limits]\nmax-lock-size = 7.5\n");

    let run = project.supdoc(&["check"]);

    let _ = run.assert_exit(78);
    let output = all_output(&run);
    assert!(
        output.contains("a size such as `max-lock-size = \"16MiB\"` (also `B` or `KiB`)"),
        "a float should get the unit-form help:\n{output}"
    );
}

#[test]
fn max_config_size_in_a_config_file_points_at_the_variable_and_the_flag() {
    let project = Project::fixture("basic");
    project.write(
        "supdoc.config.toml",
        "[limits]\nmax-config-size = \"1MiB\"\n",
    );

    let run = project.supdoc(&["check"]);

    let _ = run.assert_exit(78);
    let output = all_output(&run);
    assert!(
        output.contains("Set it with `SUPDOC_MAX_CONFIG_SIZE=1MiB` or `--max-config-size 1MiB`"),
        "the help should name the variable and the flag:\n{output}"
    );
}

/// Runs `supdoc args` from the root of `project` with `HOME` and `XDG_CONFIG_HOME` in a fixture
/// directory beneath it, whose `git/ignore` is the global excludes file `prepare` writes, so no
/// git config or home of the real user is read.
fn with_global_excludes(
    project: &Project,
    args: &[&str],
    prepare: impl FnOnce(&Path),
) -> crate::support::Run {
    let home = project.path("home");
    let xdg = home.join("xdg");
    std::fs::create_dir_all(xdg.join("git"))
        .expect("the fixture git directory should be creatable");
    prepare(&xdg.join("git/ignore"));
    let system = home.join("no-system-gitconfig");
    project.supdoc_in_with(
        ".",
        args,
        &[
            ("HOME", &home),
            ("XDG_CONFIG_HOME", &xdg),
            ("GIT_CONFIG_SYSTEM", &system),
        ],
    )
}

/// Writes an oversized global excludes file at `file`.
fn oversized_global(file: &Path) {
    std::fs::write(file, oversized("gen/")).expect("the global excludes file should be writable");
}

#[test]
fn an_oversized_global_excludes_file_fails_check_with_78() {
    let project = Project::fixture("basic");

    let run = with_global_excludes(&project, &["check"], oversized_global);

    let _ = run.assert_exit(78);
    let output = all_output(&run);
    assert!(
        output.contains("ignore-too-large")
            && output.contains("the global excludes file")
            && output.contains("or shorten the file"),
        "a global excludes file over the cap should be refused by name:\n{output}"
    );
}

#[test]
fn an_oversized_global_excludes_file_stops_rename_before_it_writes() {
    let project = Project::fixture("namespaced");
    project.record();
    let files = ["supdoc.config.toml", "supdoc.lock", "docs/guide.md"];
    let before = files.map(|file| project.read(file));

    let run = with_global_excludes(
        &project,
        &["namespace", "rename", "auth", "authn", "--yes"],
        oversized_global,
    );

    let _ = run.assert_exit(78);
    assert!(
        all_output(&run).contains("ignore-too-large"),
        "rename should refuse the oversized global excludes file:\n{}",
        all_output(&run)
    );
    let after = files.map(|file| project.read(file));
    assert_eq!(before, after, "a refused rename should write nothing");
}

#[cfg(unix)]
#[test]
fn an_unreadable_global_excludes_file_warns_by_name_and_the_run_goes_on() {
    use std::os::unix::fs::PermissionsExt as _;

    let project = Project::fixture("basic");
    let mut file = std::path::PathBuf::new();

    let run = with_global_excludes(&project, &["--json", "list"], |global| {
        std::fs::write(global, "docs/\n").expect("the global excludes file should be writable");
        std::fs::set_permissions(global, std::fs::Permissions::from_mode(0o000))
            .expect("the global excludes file should be lockable");
        file = global.to_path_buf();
    });
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))
        .expect("the global excludes file should be unlockable");

    let _ = run.assert_exit(0);
    let output = all_output(&run);
    assert!(
        output.contains("scan-ignore-file-unreadable")
            && output.contains("the global excludes file")
            && !output.contains("chmod"),
        "an unreadable global excludes file should warn by name, without a chmod into the user's \
         home:\n{output}"
    );
}

#[test]
fn a_missing_global_excludes_file_is_silent() {
    let project = Project::fixture("basic");

    let run = with_global_excludes(&project, &["--json", "list"], |_| {});

    let _ = run.assert_exit(0);
    assert!(
        !all_output(&run).contains("global excludes"),
        "no global excludes file is nothing to report:\n{}",
        all_output(&run)
    );
}
