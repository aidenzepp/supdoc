//! `[limits]` as `config show --json` reports it: which of flag, environment variable, config
//! file or default set each ceiling.

use std::path::Path;

use crate::support::{Project, array_at, at};

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
fn the_key_path_limits_take_flag_then_variable_then_config_then_default() {
    let project = Project::fixture("basic");
    let show = |args: &[&str], env: &[(&str, &Path)]| {
        project.supdoc_in_with(".", args, env).assert_exit(0).json()
    };
    let base = ["config", "show", "--json"];
    let env = [
        ("SUPDOC_MAX_KEY_SEGMENTS", Path::new("500")),
        ("SUPDOC_MAX_KEY_PATH_SIZE", Path::new("2MiB")),
    ];

    let json = show(&base, &[]);
    assert_eq!(at(&json, "/data/limits/max_key_segments/value"), "200000");
    assert_eq!(at(&json, "/data/limits/max_key_path_size/value"), "32MiB");
    assert_eq!(at(&json, "/data/limits/max_key_segments/source"), "default");

    project.write(
        "supdoc.config.toml",
        "[limits]\nmax-key-segments = 1000\nmax-key-path-size = \"4MiB\"\n",
    );
    let json = show(&base, &[]);
    assert_eq!(at(&json, "/data/limits/max_key_segments/value"), "1000");
    assert_eq!(at(&json, "/data/limits/max_key_segments/source"), "config");
    assert_eq!(at(&json, "/data/limits/max_key_path_size/value"), "4MiB");
    assert_eq!(
        at(&json, "/data/limits/max_key_path_size/from"),
        "supdoc.config.toml"
    );

    let json = show(&base, &env);
    assert_eq!(at(&json, "/data/limits/max_key_segments/value"), "500");
    assert_eq!(at(&json, "/data/limits/max_key_segments/source"), "env");
    assert_eq!(at(&json, "/data/limits/max_key_path_size/value"), "2MiB");
    assert_eq!(at(&json, "/data/limits/max_key_path_size/source"), "env");

    let json = show(
        &[
            "config",
            "show",
            "--json",
            "--max-key-segments",
            "0",
            "--max-key-path-size",
            "9KiB",
        ],
        &env,
    );
    assert_eq!(at(&json, "/data/limits/max_key_segments/value"), "0");
    assert_eq!(at(&json, "/data/limits/max_key_segments/source"), "flag");
    assert_eq!(at(&json, "/data/limits/max_key_path_size/value"), "9KiB");
    assert_eq!(at(&json, "/data/limits/max_key_path_size/source"), "flag");
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

#[test]
fn a_code_block_source_is_read_under_the_flag_s_key_segment_limit() {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```rust uses(/Cargo.toml#package.version)\nfn add() {}\n```\n",
    );
    let over_the_limit = |args: &[&str]| -> Vec<String> {
        let command: Vec<&str> = args.iter().chain(&["check", "--json"]).copied().collect();
        let json = project.supdoc(&command).json();
        array_at(&json, "/findings")
            .iter()
            .filter(|finding| finding.to_string().contains("max-key-segments"))
            .map(ToString::to_string)
            .collect()
    };

    let within = over_the_limit(&[]);
    let over = over_the_limit(&["--max-key-segments", "2"]);

    assert!(
        within.is_empty(),
        "the default limit holds the file: {within:?}"
    );
    assert!(
        over.len() == 1
            && over
                .iter()
                .all(|finding| finding.contains("code-block-source-missing")),
        "the flag's limit should bound the source's read: {over:?}"
    );
}

/// The `max-file-size` the file size tests set: well above every file of the `basic` fixture and
/// below [`big_rust`].
const FILE_CAP: &str = "2KiB";

/// Rust over [`FILE_CAP`] that defines `big`, padded by a comment.
fn big_rust() -> String {
    format!("fn big() {{}}\n// {}\n", "x".repeat(4096))
}

/// The `basic` fixture plus `src/big.rs`, over [`FILE_CAP`], and a doc that links into it,
/// recorded under the default `max-file-size`.
fn project_with_big_target() -> Project {
    let project = Project::fixture("basic");
    project.write("src/big.rs", &big_rust());
    project.write("docs/big.md", "[big](../src/big.rs#big)\n");
    project.record();
    project
}

/// Asserts that `run` refused `src/big.rs` and printed `read-too-large` for it.
fn assert_refused(run: &crate::support::Run, how: &str) {
    let output = all_output(run);
    assert!(
        output.contains("read-too-large") && output.contains("src/big.rs"),
        "`supdoc {}` ({how}) should report src/big.rs as read-too-large:\n{output}",
        run.args
    );
}

#[test]
fn every_reading_command_honors_the_max_file_size_flag_and_variable() {
    let project = project_with_big_target();
    let commands: [&[&str]; 5] = [
        &["check"],
        &["list"],
        &["update", "--yes"],
        &["fix"],
        &["resolve", "src/big.rs"],
    ];

    for command in commands {
        let within = project.supdoc(command);
        let _ = within.assert_exit(0);
        assert!(
            !all_output(&within).contains("read-too-large"),
            "the default limit holds src/big.rs:\n{}",
            all_output(&within)
        );
        let flagged: Vec<&str> = ["--max-file-size", FILE_CAP]
            .iter()
            .chain(command)
            .copied()
            .collect();
        assert_refused(&project.supdoc(&flagged), "flag");
        let from_env = project.supdoc_in_with(
            ".",
            command,
            &[("SUPDOC_MAX_FILE_SIZE", Path::new(FILE_CAP))],
        );
        assert_refused(&from_env, "variable");
    }
}

#[test]
fn the_max_file_size_config_key_reaches_the_reader_and_zero_lifts_it() {
    let project = project_with_big_target();
    project.write(
        "supdoc.config.toml",
        &format!("[limits]\nmax-file-size = \"{FILE_CAP}\"\n"),
    );

    assert_refused(&project.supdoc(&["check"]), "config");
    let _ = project
        .supdoc(&["--max-file-size", "0", "check"])
        .assert_exit(0);
}

#[test]
fn max_file_size_takes_flag_then_variable_then_config_then_default() {
    let project = Project::fixture("basic");
    let show = |args: &[&str], env: &[(&str, &Path)]| {
        project.supdoc_in_with(".", args, env).assert_exit(0).json()
    };
    let base = ["config", "show", "--json"];
    let env = [("SUPDOC_MAX_FILE_SIZE", Path::new("2MiB"))];

    let json = show(&base, &[]);
    assert_eq!(at(&json, "/data/limits/max_file_size/source"), "default");

    project.write("supdoc.config.toml", "[limits]\nmax-file-size = \"8MiB\"\n");
    let json = show(&base, &[]);
    assert_eq!(at(&json, "/data/limits/max_file_size/value"), "8MiB");
    assert_eq!(at(&json, "/data/limits/max_file_size/source"), "config");
    assert_eq!(
        at(&json, "/data/limits/max_file_size/from"),
        "supdoc.config.toml"
    );

    let json = show(&base, &env);
    assert_eq!(at(&json, "/data/limits/max_file_size/value"), "2MiB");
    assert_eq!(at(&json, "/data/limits/max_file_size/source"), "env");

    let json = show(&["config", "show", "--json", "--max-file-size", "0"], &env);
    assert_eq!(at(&json, "/data/limits/max_file_size/value"), "0");
    assert_eq!(at(&json, "/data/limits/max_file_size/source"), "flag");
}

/// Asserts that `fix`, previewed and applied, fails on `project` with `code` for `file` and never
/// says there is nothing to fix.
fn assert_fix_reports_the_read_error(project: &Project, args: &[&str], code: &str, file: &str) {
    for mode in [&[][..], &["--yes"]] {
        let command: Vec<&str> = args.iter().chain(&["fix"]).chain(mode).copied().collect();

        let run = project.supdoc(&command);

        let output = all_output(&run);
        let _ = run.assert_exit(1);
        assert!(
            output.contains(code) && output.contains(file),
            "`supdoc {}` should report {file} as {code}:\n{output}",
            run.args
        );
        assert!(
            !output.contains("Nothing to fix"),
            "{file} was never checked for fixes:\n{output}"
        );
    }
}

#[test]
fn fix_fails_with_read_too_large_for_a_doc_over_the_cap() {
    let project = Project::fixture("basic");
    project.record();
    project.write("docs/big.md", &format!("# Big\n\n{}\n", "x".repeat(4096)));

    assert_fix_reports_the_read_error(
        &project,
        &["--max-file-size", FILE_CAP],
        "read-too-large",
        "docs/big.md",
    );
}

#[test]
fn fix_fails_with_read_too_large_for_a_target_over_the_cap() {
    let project = Project::fixture("basic");
    project.write("supdoc.config.toml", "include = [\"docs/**\"]\n");
    project.write("src/big.rs", &big_rust());
    project.write("docs/big.md", "[big](../src/big.rs#big)\n");
    project.record();

    assert_fix_reports_the_read_error(
        &project,
        &["--max-file-size", FILE_CAP],
        "read-too-large",
        "src/big.rs",
    );
}

#[test]
fn fix_fails_with_read_not_utf8_for_a_doc_that_is_not_utf8() {
    let project = Project::fixture("basic");
    project.record();
    std::fs::write(project.path("docs/bad.md"), b"# Bad\n\n\xff\xfe\n")
        .expect("the non-UTF-8 doc should be writable");

    assert_fix_reports_the_read_error(&project, &[], "read-not-utf8", "docs/bad.md");
}

#[test]
fn list_resolves_a_symbol_only_in_a_target_under_the_resolved_cap() {
    let project = project_with_big_target();

    let over = project.supdoc(&["--max-file-size", FILE_CAP, "list", "src/big.rs#nope"]);
    let raised = project.supdoc(&["--max-file-size", "8KiB", "list", "src/big.rs#nope"]);

    // Over the cap the file isn't read, and `list` says so rather than matching the symbol as text.
    let _ = over.assert_exit(1);
    assert!(
        over.stderr.contains("read-too-large"),
        "a target over the cap should be refused with read-too-large:\n{}",
        all_output(&over)
    );
    // Under a raised cap the file is read and the missing symbol is refused.
    let _ = raised.assert_exit(1);
    assert!(
        !all_output(&raised).contains("read-too-large")
            && !all_output(&raised).contains("No links to"),
        "a target under the raised cap should be read and its symbol looked up:\n{}",
        all_output(&raised)
    );
}

#[test]
fn a_code_block_source_is_read_under_the_flag_s_file_size_limit() {
    let project = Project::fixture("basic");
    project.record();
    project.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n# {}\n",
            "x".repeat(4096)
        ),
    );
    project.write("supdoc.config.toml", "[features.code-blocks]\n");
    project.write(
        "docs/blocks.md",
        "# Blocks\n\n```rust uses(/Cargo.toml#package.version)\nfn add() {}\n```\n",
    );
    let refused_sources = |args: &[&str]| -> Vec<String> {
        let command: Vec<&str> = args.iter().chain(&["check", "--json"]).copied().collect();
        let json = project.supdoc(&command).json();
        array_at(&json, "/findings")
            .iter()
            .map(ToString::to_string)
            .filter(|finding| {
                finding.contains("code-block-source-missing") && finding.contains("max-file-size")
            })
            .collect()
    };

    let within = refused_sources(&[]);
    let over = refused_sources(&["--max-file-size", FILE_CAP]);

    assert!(
        within.is_empty(),
        "the default limit holds the file: {within:?}"
    );
    assert_eq!(
        over.len(),
        1,
        "the flag's limit should bound the source's read: {over:?}"
    );
}

#[test]
fn max_file_size_long_help_gives_the_measured_memory_cost_and_short_help_does_not() {
    let project = Project::fixture("basic");

    let long = project.supdoc(&["check", "--help"]);
    let short = project.supdoc(&["check", "-h"]);

    let _ = long.assert_exit(0);
    for line in [
        "- YAML: about 740 MiB on one line, 315 MiB across lines",
        "- JavaScript, TypeScript: about 390 MiB",
        "- JSON: about 235 MiB",
        "- TOML: about 185 MiB",
    ] {
        assert!(
            long.stdout.contains(line),
            "`--help` should list {line:?}:\n{}",
            long.stdout
        );
    }
    let _ = short.assert_exit(0);
    assert!(
        short.stdout.contains("--max-file-size") && !short.stdout.contains("about 235 MiB"),
        "`-h` keeps the one-line help:\n{}",
        short.stdout
    );
}
