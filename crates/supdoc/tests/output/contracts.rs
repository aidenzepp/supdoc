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

/// One way to make `supdoc` print a message that carries a `help`, and the code it should have.
struct HelpCase {
    /// The code (`kind` in JSON) the run should print.
    kind: &'static str,
    /// Builds the project the run happens in, starting from [`Project::clean`].
    setup: fn(&Project),
    /// The arguments, before `--json`.
    args: &'static [&'static str],
    /// Whether the run needs `HOME` pointing at an empty directory inside the project.
    home: bool,
}

/// Every `(kind, help)` pair under `value`: any object with both a `kind` and a `help` string.
fn collect_helps(value: &serde_json::Value, found: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            if let (Some(kind), Some(help)) = (
                map.get("kind").and_then(serde_json::Value::as_str),
                map.get("help").and_then(serde_json::Value::as_str),
            ) {
                found.push((kind.to_owned(), help.to_owned()));
            }
            for child in map.values() {
                collect_helps(child, found);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_helps(child, found);
            }
        }
        _ => {}
    }
}

/// Whether `help` quotes at least one non-empty backtick span: a command or a concrete value.
fn quotes_a_command_or_value(help: &str) -> bool {
    help.split('`')
        .skip(1)
        .step_by(2)
        .any(|span| !span.trim().is_empty())
}

/// Whether `help` sends the reader elsewhere instead of saying what to do (07 G3).
fn points_elsewhere(help: &str) -> bool {
    const POINTERS: &[&str] = &[
        "see the docs",
        "see docs",
        "see the documentation",
        "documentation",
        "refer to",
        "consult",
        "readme",
        "http",
    ];
    let lowered = help.to_lowercase();
    help.split_whitespace().count() < 3 || POINTERS.iter().any(|pointer| lowered.contains(pointer))
}

/// The codes whose remedy is a command or a value the user can type, so their help must quote it
/// (07 G3's examples: `supdoc update docs/guide.md --yes`, `did you mean log_in?`).
const QUOTED_REMEDIES: &[&str] = &[
    "link/failed",
    "link/new",
    "link/obsolete",
    "resolve/not-found",
];

/// Every message with a `help` that a user can be made to see, one way to reach each.
const HELP_CASES: &[HelpCase] = &[
    HelpCase {
        kind: "link/failed",
        setup: |p| {
            p.write(
                "src/lib.rs",
                "pub fn add() {}\npub fn mul() {}\npub fn sub() {}\n",
            );
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "link/new",
        setup: |p| p.write("docs/new.md", "# N\n\nSee [add](../src/lib.rs#add).\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "link/obsolete",
        setup: |p| {
            std::fs::remove_file(p.path().join("docs/old.md")).expect("old.md should be removable");
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "resolve/not-found",
        setup: |p| p.write("docs/guide.md", "[x](../src/lib.rs#ad)\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "link/unknown-namespace",
        setup: |p| p.write("docs/guide.md", "[x](nons:lib.rs#add)\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "link/invalid-symbol",
        setup: |p| p.write("docs/guide.md", "[x](../src/lib.rs#a..b)\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "link/invalid-path",
        setup: |p| p.write("docs/guide.md", "[x](../src//lib.rs#add)\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "link/outside-root",
        setup: |p| p.write("docs/guide.md", "[x](../../../elsewhere.rs#add)\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "link/text-fragment-unsupported",
        setup: |p| p.write("docs/guide.md", "[x](../src/lib.rs#:~:text=add)\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "read/not-utf8",
        setup: |p| {
            std::fs::write(p.path().join("src/bin.rs"), [0xff_u8, 0xfe])
                .expect("bin.rs should be writable");
            p.write("docs/guide.md", "[x](../src/bin.rs#add)\n");
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "read/unsupported-language",
        setup: |p| {
            p.write("src/data.xyz", "x");
            p.write("docs/guide.md", "[x](../src/data.xyz#add)\n");
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "read/syntax-errors",
        setup: |p| p.write("src/bad.rs", "fn ( {{{{"),
        args: &["resolve", "src/bad.rs"],
        home: false,
    },
    HelpCase {
        kind: "read/missing",
        setup: |p| p.write("docs/guide.md", "[x](../src/gone.rs#add)\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "config/parse",
        setup: |p| p.write("supdoc.config.toml", "include = 3\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "config/not-found",
        setup: |_| {},
        args: &["check", "--config", "nope.toml"],
        home: false,
    },
    HelpCase {
        kind: "config/extends-missing",
        setup: |p| {
            p.write(
                "supdoc.config.toml",
                "extends = \"sub/supdoc.config.toml\"\n",
            );
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "config/extends-loop",
        setup: |p| {
            p.write(
                "supdoc.config.toml",
                "extends = \"sub/supdoc.config.toml\"\n",
            );
            p.write(
                "sub/supdoc.config.toml",
                "extends = \"../supdoc.config.toml\"\n",
            );
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "config/extends-directory",
        setup: |p| {
            std::fs::create_dir_all(p.path().join("sub/supdoc.config.toml"))
                .expect("the directory should be creatable");
            p.write(
                "supdoc.config.toml",
                "extends = \"sub/supdoc.config.toml\"\n",
            );
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "config/namespace-outside-root",
        setup: |p| {
            p.write(
                "supdoc.config.toml",
                "[namespaces]\ncore = \"../elsewhere\"\n",
            );
        },
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "lock/parse",
        setup: |p| p.write("supdoc.lock", "not = [toml"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "lock/newer-version",
        setup: |p| p.write("supdoc.lock", "version = 999\n"),
        args: &["check"],
        home: false,
    },
    HelpCase {
        kind: "scan/explicit-path-excluded",
        setup: |p| p.write("supdoc.config.toml", "exclude = [\"docs/old.md\"]\n"),
        args: &["check", "docs/old.md"],
        home: false,
    },
    HelpCase {
        kind: "scan/explicit-path-ignored",
        setup: |p| p.write(".gitignore", "docs/old.md\n"),
        args: &["check", "docs/old.md"],
        home: false,
    },
    HelpCase {
        kind: "cli/path-missing",
        setup: |_| {},
        args: &["check", "docs/nope.md"],
        home: false,
    },
    HelpCase {
        kind: "cli/path-outside-project",
        setup: |_| {},
        args: &["check", "/"],
        home: false,
    },
    HelpCase {
        kind: "cli/init-exists",
        setup: |_| {},
        args: &["init"],
        home: false,
    },
    HelpCase {
        kind: "cli/rename-unknown-namespace",
        setup: |_| {},
        args: &["namespace", "rename", "nope", "other"],
        home: false,
    },
    HelpCase {
        kind: "cli/rename-target-exists",
        setup: |p| {
            p.write(
                "supdoc.config.toml",
                "[namespaces]\ncore = \"src\"\nbase = \"docs\"\n",
            );
        },
        args: &["namespace", "rename", "core", "base"],
        home: false,
    },
    HelpCase {
        kind: "resolve/not-found",
        setup: |_| {},
        args: &["resolve", "src/lib.rs", "ad"],
        home: false,
    },
    HelpCase {
        kind: "cli/skill-exists",
        setup: |p| p.write(".agents/skills/supdoc/SKILL.md", "different"),
        args: &["skill", "install"],
        home: false,
    },
    HelpCase {
        kind: "cli/skill-no-home",
        setup: |_| {},
        args: &["skill", "install", "--global"],
        home: false,
    },
];

#[test]
fn every_help_names_a_command_or_value() {
    let mut problems = Vec::new();
    for case in HELP_CASES {
        let project = Project::clean();
        (case.setup)(&project);
        let home_dir = project.path().join("home");
        let home = home_dir.to_str().expect("the temp path should be UTF-8");
        let mut args = case.args.to_vec();
        args.push("--json");
        let env: &[(&str, &str)] = if case.home { &[("HOME", home)] } else { &[] };
        let run = project.run_with(&args, env);

        let mut helps = Vec::new();
        for stream in [&run.stdout, &run.stderr] {
            for line in stream.lines() {
                if let Ok(document) = serde_json::from_str::<serde_json::Value>(line) {
                    collect_helps(&document, &mut helps);
                }
            }
        }
        if !helps.iter().any(|(kind, _)| kind == case.kind) {
            problems.push(format!(
                "`supdoc {}` should print `{}` with a help; saw {helps:?}; stderr: {}",
                run.args,
                case.kind,
                run.stderr.trim()
            ));
        }
        for (kind, help) in &helps {
            if points_elsewhere(help) {
                problems.push(format!(
                    "the help of `{kind}` (from `supdoc {}`) should say what to do, not send the reader elsewhere (07 G3): {help:?}",
                    run.args
                ));
            }
            if QUOTED_REMEDIES.contains(&kind.as_str()) && !quotes_a_command_or_value(help) {
                problems.push(format!(
                    "the help of `{kind}` (from `supdoc {}`) should quote a runnable command or concrete value (07 G3): {help:?}",
                    run.args
                ));
            }
        }
    }
    problems.sort();
    problems.dedup();
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
