//! The contracts only the running binary can show: what touches the disk, what reaches each
//! stream, color and glyphs, a closed pipe, and `ok` against the exit code (06, 07).

use std::io::{BufRead, BufReader};
use std::process::Stdio;

use crate::support::{Project, Run, strip_ansi};

/// Every read command, in the arguments a user would type (06 X3).
const READS: &[&[&str]] = &[
    &["check"],
    &["check", "docs/guide.md"],
    &["list"],
    &["list", "--state", "failed"],
    &["list", "src/lib.rs#add"],
    &["resolve", "src/lib.rs"],
    &["resolve", "src/lib.rs", "add"],
    &["resolve", "src/lib.rs", "ad"],
    &["config", "show"],
    &["namespace", "list"],
    &["schema"],
    &["schema", "check"],
    &["completions", "bash"],
];

/// Every write command run without `--yes`, or with `--dry-run`, which beats `--yes` (06 X4).
const PREVIEWS: &[&[&str]] = &[
    &["update"],
    &["update", "--dry-run", "--yes"],
    &["fix"],
    &["fix", "--dry-run", "--yes"],
    &["namespace", "rename", "core", "base"],
    &["namespace", "rename", "core", "base", "--dry-run", "--yes"],
    &["init"],
    &["init", "--dry-run"],
];

/// Every format a run can be asked for.
const FORMATS: &[&[&str]] = &[&[], &["--json"], &["--jsonl"], &["--output", "github"]];

/// `args` followed by `format`.
fn with_format<'a>(args: &[&'a str], format: &[&'a str]) -> Vec<&'a str> {
    args.iter().chain(format).copied().collect()
}

/// Runs each of `commands` in every format against a dirty project, and fails naming the first
/// run that changed a file.
fn assert_writes_nothing(commands: &[&[&str]]) {
    let project = Project::dirty();
    let before = project.files();

    for args in commands {
        for format in FORMATS {
            let run = project.run(&with_format(args, format));
            let after = project.files();

            assert!(
                before == after,
                "`supdoc {}` changed the project; files now: {:?}",
                run.args,
                after.keys().collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn read_commands_never_write() {
    assert_writes_nothing(READS);
}

#[test]
fn write_commands_write_nothing_without_yes_or_with_dry_run() {
    assert_writes_nothing(PREVIEWS);
}

#[test]
fn init_dry_run_writes_nothing_in_an_empty_directory() {
    let project = Project::empty();

    let run = project.run(&["init", "--dry-run"]);

    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(
        project.files().is_empty(),
        "`init --dry-run` should write nothing: {:?}",
        project.files().keys().collect::<Vec<_>>()
    );
}

/// Successful runs under `--json`, each of which must print one JSON document and nothing else.
const JSON_SUCCESSES: &[&[&str]] = &[
    &["check"],
    &["list"],
    &["resolve", "src/lib.rs"],
    &["config", "show"],
    &["namespace", "list"],
    &["namespace", "rename", "core", "base"],
    &["update"],
    &["fix"],
    &["schema", "check"],
    &["completions", "bash"],
];

#[test]
fn json_output_has_nothing_but_json_on_stdout() {
    let project = Project::clean();

    for args in JSON_SUCCESSES {
        let run = project.run(&with_format(args, &["--json"]));

        assert_eq!(run.code, Some(0), "`supdoc {}`: {}", run.args, run.stderr);
        assert_eq!(
            run.stdout.lines().count(),
            1,
            "`supdoc {}` should print one JSON line: {}",
            run.args,
            run.stdout
        );
        let document: serde_json::Value =
            serde_json::from_str(&run.stdout).unwrap_or_else(|error| {
                panic!(
                    "`supdoc {}` stdout should be JSON ({error}): {}",
                    run.args, run.stdout
                )
            });
        assert!(document.is_object(), "`supdoc {}`: {document}", run.args);
        assert_eq!(
            run.stderr, "",
            "`supdoc {}` should leave stderr empty on success under --json",
            run.args
        );
    }
}

#[test]
fn jsonl_output_has_nothing_but_json_lines_on_stdout() {
    let project = Project::clean();

    for args in JSON_SUCCESSES {
        let run = project.run(&with_format(args, &["--jsonl"]));

        assert_eq!(run.code, Some(0), "`supdoc {}`: {}", run.args, run.stderr);
        for line in run.stdout.lines() {
            let record: Result<serde_json::Value, _> = serde_json::from_str(line);
            assert!(
                record.is_ok_and(|record| record.is_object()),
                "`supdoc {}` printed a line that isn't a JSON object: {line}",
                run.args
            );
        }
        assert_eq!(run.stderr, "", "`supdoc {}` stderr", run.args);
    }
}

/// `ok` from the JSON envelope on stdout, or from the JSONL summary line, if there is one.
fn reported_ok(run: &Run) -> Option<bool> {
    let last = run.stdout.lines().last()?;
    let value: serde_json::Value = serde_json::from_str(last).unwrap_or_else(|error| {
        panic!(
            "`supdoc {}` stdout should end in JSON ({error}): {}",
            run.args, run.stdout
        )
    });
    Some(
        value
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(|| panic!("`supdoc {}` should report `ok`: {value}", run.args)),
    )
}

#[test]
fn ok_is_true_exactly_when_exit_is_0() {
    let clean = Project::clean();
    let dirty = Project::dirty();
    let runs: Vec<Run> = [&clean, &dirty]
        .into_iter()
        .flat_map(|project| {
            [
                &["check"][..],
                &["check", "--deny-warnings"],
                &["list"],
                &["update"],
                &["fix"],
            ]
            .into_iter()
            .flat_map(move |args| {
                [&["--json"][..], &["--jsonl"]]
                    .into_iter()
                    .map(move |format| project.run(&with_format(args, format)))
            })
        })
        .collect();

    let mut seen = Vec::new();
    for run in &runs {
        if let Some(ok) = reported_ok(run) {
            assert_eq!(
                ok,
                run.code == Some(0),
                "`supdoc {}` reported ok={ok} but exited {:?}",
                run.args,
                run.code
            );
            seen.push(ok);
        }
    }
    assert!(
        seen.contains(&true) && seen.contains(&false),
        "the runs should cover both a passing and a failing report: {seen:?}"
    );
}

#[test]
fn piping_into_head_exits_cleanly() {
    let project = Project::clean();

    for args in [&["schema"][..], &["completions", "bash"]] {
        let mut child = project
            .command(args, &[])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the supdoc binary should start");
        let stdout = child.stdout.take().expect("stdout should be piped");
        let mut first = String::new();
        let _read = BufReader::new(stdout)
            .read_line(&mut first)
            .expect("the first line should be readable");
        let output = child.wait_with_output().expect("supdoc should finish");

        assert!(
            !first.is_empty(),
            "`supdoc {}` should print a line",
            args.join(" ")
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "`supdoc {}` should exit 0 when the reader hangs up: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "`supdoc {}` should print nothing about the closed pipe: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Runs a failing `check` with `env` and returns stdout.
fn check_stdout(project: &Project, args: &[&str], env: &[(&str, &str)]) -> String {
    let run = project.run_with(&with_format(&["check"], args), env);
    assert_eq!(
        run.code,
        Some(1),
        "the dirty check should fail: {}",
        run.stderr
    );
    run.stdout
}

#[test]
fn color_always_colors_stdout_and_no_color_turns_auto_off() {
    let project = Project::dirty();

    let always = check_stdout(&project, &["--color", "always"], &[("TERM", "xterm")]);
    let no_color = check_stdout(&project, &[], &[("TERM", "xterm"), ("NO_COLOR", "1")]);
    let never = check_stdout(
        &project,
        &[],
        &[("TERM", "xterm"), ("SUPDOC_COLOR", "never")],
    );

    assert!(
        always.contains('\u{1b}'),
        "--color always should emit ANSI: {always}"
    );
    assert!(
        !no_color.contains('\u{1b}'),
        "NO_COLOR should turn color off: {no_color}"
    );
    assert!(
        !never.contains('\u{1b}'),
        "SUPDOC_COLOR=never should turn color off: {never}"
    );
}

#[test]
fn dumb_terminal_and_color_never_use_ascii_glyphs() {
    let project = Project::failing();

    let fancy = check_stdout(&project, &["--color", "always"], &[("TERM", "xterm")]);
    let dumb = check_stdout(&project, &["--color", "always"], &[("TERM", "dumb")]);
    let never = check_stdout(&project, &["--color", "never"], &[("TERM", "xterm")]);

    assert!(
        !strip_ansi(&fancy).is_ascii(),
        "a capable terminal with color should draw with Unicode: {fancy}"
    );
    assert!(
        strip_ansi(&dumb).is_ascii(),
        "TERM=dumb should be plain ASCII: {dumb}"
    );
    assert!(
        never.is_ascii(),
        "--color never should be plain ASCII: {never}"
    );
}

#[test]
#[ignore = "known defect, need filed to mind: LinkKey's Display draws `→` into obsolete and moved messages"]
fn obsolete_and_moved_messages_use_ascii_glyphs_on_a_dumb_terminal() {
    for project in [Project::dirty(), Project::moved()] {
        let run = project.run_with(&["check", "--color", "never"], &[("TERM", "dumb")]);

        assert!(
            run.stdout.is_ascii(),
            "TERM=dumb should be plain ASCII, finding messages included: {}",
            run.stdout
        );
    }
}

#[test]
fn completions_and_schema_work_with_a_broken_config() {
    let project = Project::empty();
    project.write("supdoc.config.toml", "include = 3\n");
    let broken = project.run(&["check"]);
    assert_eq!(
        broken.code,
        Some(78),
        "the fixture config should be broken: {}",
        broken.stderr
    );

    for args in [
        &["completions", "bash"][..],
        &["schema", "check"],
        &["completions", "zsh", "--config", "missing.toml"],
        &["schema", "--config", "missing.toml"],
    ] {
        let run = project.run(args);

        assert_eq!(run.code, Some(0), "`supdoc {}`: {}", run.args, run.stderr);
        assert!(
            !run.stdout.is_empty(),
            "`supdoc {}` should print its output",
            run.args
        );
    }
}
