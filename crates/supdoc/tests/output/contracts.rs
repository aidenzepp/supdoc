//! The contracts only the running binary can show: what touches the disk, what reaches each
//! stream, color and glyphs, a closed pipe, and `ok` against the exit code.

use std::io::{BufRead, BufReader};
use std::process::Stdio;

use crate::support::{Project, Run, strip_ansi};

/// Every read command, in the arguments a user would type.
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

/// Every write command run without `--yes`, or with `--dry-run`, which beats `--yes`.
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
        let mut last = None;
        for line in run.stdout.lines() {
            let record: Result<serde_json::Value, _> = serde_json::from_str(line);
            assert!(
                record.as_ref().is_ok_and(serde_json::Value::is_object),
                "`supdoc {}` printed a line that isn't a JSON object: {line}",
                run.args
            );
            last = record.ok();
        }
        assert_eq!(
            last.as_ref().and_then(|record| record.get("type")),
            Some(&serde_json::json!("summary")),
            "`supdoc {}` should end its JSONL stream with the summary line:\n{}",
            run.args,
            run.stdout
        );
        assert_eq!(run.stderr, "", "`supdoc {}` stderr", run.args);
    }
}

#[test]
fn jsonl_carries_every_data_field_that_json_has() {
    let commands: Vec<&[&str]> = JSON_SUCCESSES
        .iter()
        .copied()
        .filter(|args| !matches!(args.first(), Some(&"schema" | &"completions")))
        .collect();
    for project in [Project::clean(), Project::dirty()] {
        for args in &commands {
            let json = project.run(&with_format(args, &["--json"]));
            let jsonl = project.run(&with_format(args, &["--jsonl"]));

            let document: serde_json::Value = serde_json::from_str(&json.stdout)
                .unwrap_or_else(|error| panic!("`supdoc {}` --json: {error}", json.args));
            let Some(data) = document.get("data").and_then(serde_json::Value::as_object) else {
                continue;
            };
            let lines: Vec<serde_json::Value> = jsonl
                .stdout
                .lines()
                .map(|line| {
                    serde_json::from_str(line)
                        .unwrap_or_else(|error| panic!("`supdoc {}`: {error}", jsonl.args))
                })
                .collect();
            let summary = lines
                .last()
                .expect("a JSONL stream ends with its summary line");
            let records = lines
                .iter()
                .filter(|line| !matches!(line["type"].as_str(), Some("summary" | "finding")))
                .count();
            for (key, value) in data {
                assert!(
                    summary.get(key).is_some() || value.is_array(),
                    "`supdoc {}`: `--json` data key `{key}` is missing from the `--jsonl` summary:\n{}",
                    jsonl.args,
                    jsonl.stdout
                );
            }
            let array_elements: usize = data
                .iter()
                .filter(|(key, _)| summary.get(*key).is_none())
                .filter_map(|(_, value)| value.as_array().map(Vec::len))
                .sum();
            assert_eq!(
                records, array_elements,
                "`supdoc {}`: `--jsonl` should carry one record per element of the `--json` arrays \
                 it left off the summary:\n{}",
                jsonl.args, jsonl.stdout
            );
        }
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

/// Every help names a fix. Unix only, since some codes are reached through file
/// permissions, `sh` and a file-size limit.
#[cfg(unix)]
mod helps {
    use std::collections::BTreeSet;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use crate::support::Project;

    /// How a [`HelpCase`] runs `supdoc`, beyond its arguments.
    #[derive(Clone, Copy)]
    enum Via {
        /// In the project root with an empty environment.
        Plain,
        /// With `HOME` empty, which is no home directory: every run otherwise gets an empty one.
        EmptyHome,
        /// With `CLAUDE_CONFIG_DIR` set to a relative path and `HOME` to a directory in the project.
        RelativeClaudeDir,
        /// With standard output sent to a file under a zero file-size limit, so every write to it
        /// fails, while standard error stays a pipe.
        FullStdout,
        /// From a current directory that a shell removed after entering it.
        RemovedCwd,
        /// From a new subdirectory of the project, which has a config at its root.
        Subdirectory,
        /// With `PATH` set to a directory outside the project holding a copy of each file the
        /// setup wrote under the project's `tools` directory, made executable.
        OutsideTools,
        /// With `PATH` set to the project's `bin` directory.
        ProjectPath,
        /// As a copy of the binary installed in a cargo home inside the project, whose record
        /// says cargo installed it from crates.io, with `PATH` set to the project's `bin`
        /// directory, where a setup may have put a fake `cargo`.
        CargoInstalled,
        /// With `XDG_STATE_HOME` set to a file in the project, so no state directory can be made.
        StateHomeIsFile,
    }

    /// One way to make `supdoc` print a message with a given code, and the help it carries, if any.
    struct HelpCase {
        /// The code (`kind` in JSON) the run should print.
        kind: &'static str,
        /// Builds the project the run happens in, starting from [`Project::clean`].
        setup: fn(&Project),
        /// The arguments, before `--json`.
        args: &'static [&'static str],
        /// How the binary is run.
        via: Via,
        /// Whether `setup` removes permissions, so the case can't happen for a user they don't
        /// restrict (root).
        needs_permissions: bool,
    }

    /// A [`HelpCase`] run in the project root with an empty environment.
    const fn case(
        kind: &'static str,
        setup: fn(&Project),
        args: &'static [&'static str],
    ) -> HelpCase {
        HelpCase {
            kind,
            setup,
            args,
            via: Via::Plain,
            needs_permissions: false,
        }
    }

    /// A [`HelpCase`] whose `setup` removes permissions: skipped for a user they don't restrict.
    const fn restricted_case(
        kind: &'static str,
        setup: fn(&Project),
        args: &'static [&'static str],
    ) -> HelpCase {
        HelpCase {
            kind,
            setup,
            args,
            via: Via::Plain,
            needs_permissions: true,
        }
    }

    /// A [`HelpCase`] for a report that can't be written: `args` run with a stdout that refuses
    /// every byte.
    const fn unwritable(kind: &'static str, args: &'static [&'static str]) -> HelpCase {
        HelpCase {
            kind,
            setup: |_| {},
            args,
            via: Via::FullStdout,
            needs_permissions: false,
        }
    }

    /// A code no run can be made to print, and why.
    struct Unreachable {
        /// The code.
        kind: &'static str,
        /// What it would take to print it.
        why: &'static str,
    }

    /// Codes with no binary scenario and no public constructor; their helps are checked by review.
    const UNREACHABLE: &[Unreachable] = &[
        Unreachable {
            kind: "cli-schema-serialize",
            why: "a built-in JSON Schema must fail to serialize, which is a supdoc bug",
        },
        Unreachable {
            kind: "code-block-missing-name",
            why: "needs a clippy run, which `check` holds until the trust lift; unit-tested in \
                  code_blocks::lint",
        },
    ];

    /// A code that prints no help on purpose, and why.
    struct Helpless {
        /// The code.
        kind: &'static str,
        /// Why the message has no next step to give.
        why: &'static str,
    }

    /// Codes a [`HelpCase`] prints without a help; every other printed case code must carry one.
    const HELPLESS: &[Helpless] = &[Helpless {
        kind: "check-moved",
        why: "a note that a link followed a moved file; it asks nothing of the reader",
    }];

    /// Codes whose rendered help is pinned word for word, so any rewording is a deliberate update.
    const PINNED_HELPS: &[(&str, &str)] = &[(
        "link-outside-root",
        "Link to a file inside the project root, or move `supdoc.config.toml` up to a directory \
         that holds both",
    )];

    /// A code whose message a test can build through the library, with the code and help it got.
    struct UnitCase {
        /// The code the constructor should give.
        kind: &'static str,
        /// Builds the message and returns its code and help.
        build: fn() -> (String, Option<String>),
    }

    /// Codes no binary run can print reliably, built through the library's test seam instead:
    /// tree-sitter returning no tree, a built-in query that fails to compile, and a doc edited
    /// between planning and writing a fix or a rename.
    const UNIT_CASES: &[UnitCase] = &[
        UnitCase {
            kind: "read-internal",
            build: || {
                let message =
                    supdoc::testing::read_internal().expect("the fixture path should be absolute");
                (message.code, message.help)
            },
        },
        UnitCase {
            kind: "read-cartridge-load",
            build: || {
                let message = supdoc::testing::read_cartridge_load();
                (message.code, message.help)
            },
        },
        UnitCase {
            kind: "fix-file-changed",
            build: file_changed_during_fix,
        },
        UnitCase {
            kind: "cli-rename-file-changed",
            build: || {
                let message = supdoc::testing::rename_file_changed();
                (message.code, message.help)
            },
        },
    ];

    /// The finding for a doc that changed between planning and applying a fix.
    fn file_changed_during_fix() -> (String, Option<String>) {
        let project = Project::clean();
        project.write("docs/api.md", "# API\n\nSee [ad](../src/lib.rs#ad).\n");
        let real = std::fs::canonicalize(project.path()).expect("the project root should resolve");
        let message = supdoc::testing::fix_file_changed(&real, || {
            project.write(
                "docs/api.md",
                "# API\n\nEdited after the scan.\n\nSee [ad](../src/lib.rs#ad).\n",
            );
        })
        .expect("a doc edited after the scan should be skipped");
        (message.code, message.help)
    }

    /// Every `(kind, help)` pair under `value`: any object with a `kind` string, and its `help` string
    /// when it has one.
    fn collect_helps(value: &serde_json::Value, found: &mut Vec<(String, Option<String>)>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(kind) = map.get("kind").and_then(serde_json::Value::as_str) {
                    let help = map.get("help").and_then(serde_json::Value::as_str);
                    found.push((kind.to_owned(), help.map(str::to_owned)));
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

    /// Whether `help` quotes at least one backtick span that is a command or a concrete value: a
    /// bare flag such as `--config` names neither.
    fn quotes_a_command_or_value(help: &str) -> bool {
        help.split('`').skip(1).step_by(2).any(|span| {
            let span = span.trim();
            let bare_flag = span.starts_with('-') && !span.contains(char::is_whitespace);
            !span.is_empty() && !bare_flag
        })
    }

    /// The crate's own issues page, the one URL a help may name: exit-70 helps send the reader there.
    const ISSUES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues");

    /// Whether `help` sends the reader elsewhere instead of saying what to do.
    fn points_elsewhere(help: &str) -> bool {
        const POINTERS: &[&str] = &[
            "see the docs",
            "see docs",
            "see the documentation",
            "documentation",
            "refer to",
            "consult",
            "readme",
        ];
        let lowered = help.to_lowercase().replace(ISSUES_URL, "");
        lowered.contains("http") || POINTERS.iter().any(|pointer| lowered.contains(pointer))
    }

    /// What is wrong with `help`, if anything: it must quote a runnable command or a
    /// concrete value (`supdoc update docs/guide.md --yes`, `did you mean log_in?`) and must not send
    /// the reader elsewhere.
    fn help_problem(help: &str) -> Option<&'static str> {
        if help.split_whitespace().count() < 3 {
            Some("help is too short to name a next step")
        } else if points_elsewhere(help) {
            Some("should say what to do, not send the reader elsewhere")
        } else if !quotes_a_command_or_value(help) {
            Some("should quote a runnable command or concrete value in backticks")
        } else {
            None
        }
    }

    /// Every code in supdoc's production source: each `"area-name"` string literal outside
    /// `tests.rs`, `fixtures.rs`, comments and the `#[cfg(test)] mod tests {` that ends a file.
    fn source_codes() -> BTreeSet<String> {
        let mut codes = BTreeSet::new();
        let mut pending = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("a source directory should be readable") {
                let path = entry.expect("a source entry should be readable").path();
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs")
                    && name != "tests.rs"
                    && name != "fixtures.rs"
                {
                    let text =
                        std::fs::read_to_string(&path).expect("a source file should be UTF-8");
                    let lines: Vec<&str> = text.lines().collect();
                    let production = lines
                        .iter()
                        .enumerate()
                        .take_while(|(index, line)| {
                            let opens_test_module = line.trim_start().starts_with("mod tests {")
                                && index
                                    .checked_sub(1)
                                    .and_then(|previous| lines.get(previous))
                                    .is_some_and(|previous| previous.trim() == "#[cfg(test)]");
                            !opens_test_module
                        })
                        .map(|(_, line)| *line)
                        .filter(|line| !line.trim_start().starts_with("//"));
                    for line in production {
                        let literals = line.split('"').skip(1).step_by(2);
                        codes.extend(literals.filter(|text| is_code(text)).map(str::to_owned));
                    }
                }
            }
        }
        codes
    }

    /// Config keys that look like codes but are not.
    const NOT_CODES: &[&str] = &["code-block-timeout", "code-block-build-timeout"];

    /// Whether `text` has the shape of a code: `area-name` in kebab-case, whose first word is one
    /// of supdoc's code areas, such as `link-failed` or `cli-usage`.
    fn is_code(text: &str) -> bool {
        const AREAS: &[&str] = &[
            "check", "cli", "code", "config", "fix", "ignore", "link", "lock", "read", "resolve",
            "scan",
        ];
        let Some((area, name)) = text.split_once('-') else {
            return false;
        };
        // Every `code` code is `code-block-<name>`; `code-blocks` is the feature and table name.
        !NOT_CODES.contains(&text)
            && AREAS.contains(&area)
            && (area != "code" || name.starts_with("block-"))
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    #[test]
    fn code_blocks_is_a_name_not_a_code() {
        assert!(!is_code("code-blocks"), "`code-blocks` is the feature name");
        assert!(is_code("code-block-stale"), "`code-block-stale` is a code");
        assert!(
            !is_code("code-block-timeout"),
            "`code-block-timeout` is a `[limits]` key"
        );
    }

    /// A lockfile entry for `docs/old.md`'s link, with a hash that disagrees with the recorded one.
    const DISAGREEING_ENTRY: &str = "\n[[link]]\nsource = \"docs/old.md\"\ntarget = \"src/lib.rs\"\n\
         symbol = \"sub\"\nhash = \"0000000000000000000000000000000000000000000000000000000000000000\"\n";

    /// Sets `relative` in `project` to the Unix permission `mode`.
    fn chmod(project: &Project, relative: &str, mode: u32) {
        std::fs::set_permissions(
            project.path().join(relative),
            std::fs::Permissions::from_mode(mode),
        )
        .expect("the fixture permissions should be settable");
    }

    /// Whether mode `000` keeps this user out of a directory, which it doesn't for root.
    fn permissions_restrict_this_user() -> bool {
        let dir = tempfile::tempdir().expect("the probe directory should be creatable");
        let file = dir.path().join("probe");
        std::fs::write(&file, "").expect("the probe file should be writable");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000))
            .expect("the probe directory should be lockable");
        let restricted = std::fs::symlink_metadata(&file).is_err();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("the probe directory should be unlockable");
        restricted
    }

    /// Gives the owner full access to everything under `dir` again, so the project can be removed.
    fn unlock(dir: &Path) {
        if dir.is_symlink() {
            return;
        }
        if dir.is_dir() {
            let _ignored = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                unlock(&entry.path());
            }
        } else {
            let _ignored = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o644));
        }
    }

    /// A fixture `supdoc.config.toml` that adds `extra` to the clean project's `core` namespace.
    fn config_with(project: &Project, extra: &str) {
        project.write(
            "supdoc.config.toml",
            &format!("[namespaces]\ncore = \"src\"\n{extra}"),
        );
    }

    /// Records, in the project's `supdoc.lock`, a `uses(...)` target `src/lib.rs#add` for
    /// `docs/blocks.md` with a hash no symbol has.
    fn record_use(project: &Project) {
        let lock = project.path().join("supdoc.lock");
        let text = std::fs::read_to_string(&lock).expect("the clean project's lock should read");
        let entry = format!(
            "\n[[code-blocks.uses]]\nsource = \"docs/blocks.md\"\ntarget = \"src/lib.rs\"\n\
             symbol = \"add\"\nhash = \"{}\"\n",
            "0".repeat(64)
        );
        std::fs::write(&lock, text.replace("version = 1", "version = 2") + &entry)
            .expect("the lock should be writable");
    }

    /// A doc whose one link `supdoc fix` can repair: `ad` is a misspelling of `add`.
    fn fixable(project: &Project) {
        project.write("docs/api.md", "# API\n\nSee [ad](../src/lib.rs#ad).\n");
    }

    /// A shell-table project with one `sh` block whose `shfmt`, outside the project, runs
    /// `script`.
    fn shfmt_runs(project: &Project, script: &str) {
        config_with(
            project,
            "[features.code-blocks]\n[features.code-blocks.shell]\n",
        );
        project.write("docs/blocks.md", "# B\n\n```sh\necho hi\n```\n");
        project.write("tools/shfmt", &format!("#!/bin/sh\n{script}\n"));
    }

    /// Every message with a `help` that a user can be made to see, one way to reach each code.
    const HELP_CASES: &[HelpCase] = &[
        HelpCase {
            kind: "code-block-unformatted",
            setup: |p| shfmt_runs(p, "echo formatted"),
            args: &["check"],
            via: Via::OutsideTools,
            needs_permissions: false,
        },
        HelpCase {
            kind: "code-block-tool-failed",
            setup: |p| shfmt_runs(p, "echo 'shfmt: bad input' >&2\nexit 1"),
            args: &["check"],
            via: Via::OutsideTools,
            needs_permissions: false,
        },
        HelpCase {
            kind: "code-block-parse",
            setup: |p| shfmt_runs(p, "echo \"$0:1:1: syntax error\" >&2\nexit 1"),
            args: &["check"],
            via: Via::OutsideTools,
            needs_permissions: false,
        },
        HelpCase {
            kind: "code-block-lint",
            setup: |p| {
                config_with(
                    p,
                    "[features.code-blocks]\n[features.code-blocks.python]\n[features.code-blocks.python.ruff-format]\nformat = \"off\"\n",
                );
                p.write("docs/blocks.md", "# B\n\n```py\nx == None\n```\n");
                p.write(
                    "tools/ruff",
                    "#!/bin/sh\necho '[{\"code\":\"E711\",\"message\":\"Comparison to None\",\
                     \"location\":{\"row\":1,\"column\":3}}]'\nexit 1\n",
                );
            },
            args: &["check"],
            via: Via::OutsideTools,
            needs_permissions: false,
        },
        HelpCase {
            kind: "code-block-fix-unsafe",
            setup: |p| shfmt_runs(p, "printf 'echo hi\\n```\\n'"),
            args: &["check"],
            via: Via::OutsideTools,
            needs_permissions: false,
        },
        HelpCase {
            kind: "code-block-tool-timeout",
            setup: |p| shfmt_runs(p, "/bin/sleep 30"),
            args: &["check", "--code-block-timeout", "200ms"],
            via: Via::OutsideTools,
            needs_permissions: false,
        },
        case(
            "code-block-not-trusted",
            |p| {
                config_with(p, "[features.code-blocks]\n[features.code-blocks.rust]\n");
                p.write("docs/blocks.md", "# B\n\n```rust\nfn main() {}\n```\n");
            },
            &["check"],
        ),
        case(
            "code-block-tool-missing",
            |p| {
                config_with(p, "[features.code-blocks]\n[features.code-blocks.shell]\n");
                p.write("docs/blocks.md", "# B\n\n```sh\necho hi\n```\n");
            },
            &["check"],
        ),
        HelpCase {
            kind: "code-block-not-checked",
            setup: |p| {
                use std::os::unix::fs::PermissionsExt as _;
                config_with(p, "[features.code-blocks]\n[features.code-blocks.shell]\n");
                p.write("docs/blocks.md", "# B\n\n```sh\necho hi\n```\n");
                p.write("bin/shfmt", "#!/bin/sh\n");
                std::fs::set_permissions(
                    p.path().join("bin/shfmt"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .expect("the stub should be made executable");
            },
            args: &["check"],
            via: Via::ProjectPath,
            needs_permissions: false,
        },
        case(
            "code-block-unknown-attribute",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write("docs/blocks.md", "# B\n\n```sh hidden\necho hi\n```\n");
            },
            &["check"],
        ),
        case(
            "code-block-unknown-attribute",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```rust,ignore\nfn main() {}\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-missing-reason",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write("docs/blocks.md", "# B\n\n```sh ignore\necho hi\n```\n");
            },
            &["check"],
        ),
        case(
            "code-block-missing-reason",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write("docs/blocks.md", "# B\n\n```sh lint(off)\necho hi\n```\n");
            },
            &["check"],
        ),
        case(
            "code-block-unknown-attribute",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```sh no_lint(slow)\necho hi\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-unknown-attribute",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```sh lint(partial)\necho hi\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-unknown-attribute",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```toml format(full)\na = 1\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-unknown-attribute",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```rust from=../src/lib.rs#add\nfn main() {}\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-source-missing",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```rust from(../src/lib.rs#addd)\nfn add() {}\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-stale",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```rust from(../src/lib.rs#add)\nfn add() {}\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-uses-changed",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                record_use(p);
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```rust uses(../src/lib.rs#add)\nfn add() {}\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-uses-new",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                p.write(
                    "docs/blocks.md",
                    "# B\n\n```rust uses(../src/lib.rs#add)\nfn add() {}\n```\n",
                );
            },
            &["check"],
        ),
        case(
            "code-block-uses-obsolete",
            |p| {
                config_with(p, "[features.code-blocks]\n");
                record_use(p);
            },
            &["check"],
        ),
        case(
            "link-failed",
            |p| {
                p.write(
                    "src/lib.rs",
                    "pub fn add() {}\npub fn mul() {}\npub fn sub() {}\n",
                );
            },
            &["check"],
        ),
        case(
            "link-new",
            |p| p.write("docs/new.md", "# N\n\nSee [add](../src/lib.rs#add).\n"),
            &["check"],
        ),
        case(
            "link-obsolete",
            |p| {
                std::fs::remove_file(p.path().join("docs/old.md"))
                    .expect("old.md should be removable");
            },
            &["check"],
        ),
        case(
            "resolve-not-found",
            |p| p.write("docs/guide.md", "[x](../src/lib.rs#ad)\n"),
            &["check"],
        ),
        case(
            "resolve-not-found",
            |p| p.write("docs/guide.md", "[x](../src/lib.rs#zzzzzz)\n"),
            &["check"],
        ),
        case(
            "resolve-ambiguous",
            |p| {
                p.write(
                    "src/two.rs",
                    "pub struct Foo;\npub struct Bar;\nimpl Foo {\n    pub fn new() {}\n}\nimpl Bar {\n    pub fn new() {}\n}\n",
                );
            },
            &["resolve", "src/two.rs", "new"],
        ),
        case(
            "link-unknown-namespace",
            |p| p.write("docs/guide.md", "[x](nons:lib.rs#add)\n"),
            &["check"],
        ),
        case(
            "link-unknown-namespace",
            |p| p.write("docs/guide.md", "[x](cor:lib.rs#add)\n"),
            &["check"],
        ),
        case(
            "link-unknown-namespace",
            |_| {},
            &["resolve", "nons:lib.rs"],
        ),
        case(
            "link-invalid-symbol",
            |p| p.write("docs/guide.md", "[x](../src/lib.rs#a..b)\n"),
            &["check"],
        ),
        case(
            "link-invalid-path",
            |p| p.write("docs/guide.md", "[x](../src//lib.rs#add)\n"),
            &["check"],
        ),
        case(
            "link-outside-root",
            |p| p.write("docs/guide.md", "[x](../../../elsewhere.rs#add)\n"),
            &["check"],
        ),
        case(
            "link-text-fragment-unsupported",
            |p| p.write("docs/guide.md", "[x](../src/lib.rs#:~:text=add)\n"),
            &["check"],
        ),
        case(
            "scan-case-mismatch",
            |p| p.write("docs/guide.md", "[x](../src/LIB.rs#add)\n"),
            &["check"],
        ),
        case(
            "check-moved",
            |p| {
                std::fs::rename(
                    p.path().join("docs/old.md"),
                    p.path().join("docs/legacy.md"),
                )
                .expect("old.md should be renamable");
            },
            &["check"],
        ),
        case(
            "check-move-ambiguous",
            |p| {
                std::fs::remove_file(p.path().join("docs/old.md"))
                    .expect("old.md should be removable");
                p.write("docs/a.md", "# A\n\nSee [sub](../src/lib.rs#sub).\n");
                p.write("docs/b.md", "# B\n\nSee [sub](../src/lib.rs#sub).\n");
            },
            &["check"],
        ),
        case(
            "read-not-utf8",
            |p| {
                std::fs::write(p.path().join("src/bin.rs"), [0xff_u8, 0xfe])
                    .expect("bin.rs should be writable");
                p.write("docs/guide.md", "[x](../src/bin.rs#add)\n");
            },
            &["check"],
        ),
        case(
            "read-too-large",
            |p| {
                let big = vec![b'/'; 16 * 1024 * 1024 + 1];
                std::fs::write(p.path().join("src/big.rs"), big)
                    .expect("big.rs should be writable");
                p.write("docs/guide.md", "[x](../src/big.rs#add)\n");
            },
            &["check"],
        ),
        case(
            "read-timeout",
            |p| {
                // Deep nesting stalls the symbols query far past 250 ms (SEC-1).
                let deep = format!("fn add() {{{}\n", "{".repeat(65_536));
                p.write("src/deep.rs", &deep);
                p.write("docs/guide.md", "[x](../src/deep.rs#add)\n");
            },
            &["--parse-timeout", "250ms", "check"],
        ),
        case(
            "read-unsupported-language",
            |p| p.write("src/data.xyz", "x"),
            &["resolve", "src/data.xyz"],
        ),
        case(
            "read-syntax-errors",
            |p| p.write("src/bad.rs", "fn ( {{{{"),
            &["resolve", "src/bad.rs"],
        ),
        case(
            "read-missing",
            |p| p.write("docs/guide.md", "[x](../src/gone.rs#add)\n"),
            &["check"],
        ),
        case(
            "read-not-a-file",
            |p| {
                let made = std::process::Command::new("mkfifo")
                    .arg(p.path().join("src/p.rs"))
                    .status()
                    .expect("mkfifo should run");
                assert!(made.success(), "mkfifo should create the FIFO");
            },
            &["resolve", "src/p.rs"],
        ),
        restricted_case("read-io", |p| chmod(p, "src/lib.rs", 0o000), &["check"]),
        restricted_case("read-io", |p| chmod(p, "docs/old.md", 0o000), &["check"]),
        case(
            "config-parse",
            |p| p.write("supdoc.config.toml", "include = 3\n"),
            &["check"],
        ),
        case(
            "config-parse",
            |p| p.write("supdoc.config.toml", "exlude = [\"x\"]\n"),
            &["check"],
        ),
        case(
            "config-parse",
            |p| p.write("supdoc.config.toml", "[namespaces]\nx = \"src\"\n"),
            &["check"],
        ),
        case(
            "config-parse",
            |p| p.write("supdoc.config.toml", "[namespaces]\ncore = \"/src\"\n"),
            &["check"],
        ),
        case(
            "config-parse",
            |p| p.write("supdoc.config.toml", "include = [\"../shared\"]\n"),
            &["check"],
        ),
        case(
            "config-parse",
            |p| p.write("supdoc.config.toml", "[limits]\nparse-timeout = 7\n"),
            &["check"],
        ),
        case(
            "config-parse",
            |p| p.write("supdoc.config.toml", "[limits]\nmax-lock-size = 7.5\n"),
            &["check"],
        ),
        case(
            "config-parse",
            |p| {
                p.write(
                    "supdoc.config.toml",
                    "[limits]\nmax-config-size = \"1MiB\"\n",
                );
            },
            &["check"],
        ),
        case(
            "config-too-large",
            |p| config_with(p, &"#".repeat(300_000)),
            &["check"],
        ),
        case(
            "config-not-found",
            |_| {},
            &["check", "--config", "nope.toml"],
        ),
        restricted_case(
            "config-read",
            |p| chmod(p, "supdoc.config.toml", 0o000),
            &["check"],
        ),
        case(
            "config-extends-missing",
            |p| {
                p.write(
                    "supdoc.config.toml",
                    "extends = \"sub/supdoc.config.toml\"\n",
                );
            },
            &["check"],
        ),
        case(
            "config-extends-loop",
            |p| {
                p.write(
                    "supdoc.config.toml",
                    "extends = \"sub/supdoc.config.toml\"\n",
                );
                p.write(
                    "sub/supdoc.config.toml",
                    "extends = \"../supdoc.config.toml\"\n",
                );
            },
            &["check"],
        ),
        case(
            "config-extends-directory",
            |p| {
                std::fs::create_dir_all(p.path().join("sub/supdoc.config.toml"))
                    .expect("the directory should be creatable");
                p.write(
                    "supdoc.config.toml",
                    "extends = \"sub/supdoc.config.toml\"\n",
                );
            },
            &["check"],
        ),
        case(
            "config-extends-not-config",
            |p| {
                p.write("base.toml", "");
                p.write("supdoc.config.toml", "extends = \"base.toml\"\n");
            },
            &["check"],
        ),
        case(
            "config-symlink-not-config",
            |p| {
                let config = p.path().join("supdoc.config.toml");
                let _removed = std::fs::remove_file(&config);
                p.write("base.toml", "");
                std::os::unix::fs::symlink("base.toml", &config)
                    .expect("the symlink should be creatable");
            },
            &["check"],
        ),
        case(
            "config-namespace-outside-root",
            |p| {
                p.write(
                    "supdoc.config.toml",
                    "[namespaces]\ncore = \"../elsewhere\"\n",
                );
            },
            &["check"],
        ),
        case(
            "lock-parse",
            |p| p.write("supdoc.lock", "not = [toml"),
            &["check"],
        ),
        case(
            "lock-newer-version",
            |p| p.write("supdoc.lock", "version = 999\n"),
            &["check"],
        ),
        case(
            "lock-conflict",
            |p| p.write("supdoc.lock", "version = 1\n<<<<<<< HEAD\n"),
            &["check"],
        ),
        case(
            "lock-duplicate",
            |p| {
                let path = p.path().join("supdoc.lock");
                let recorded = std::fs::read_to_string(&path).expect("the lock should be readable");
                p.write("supdoc.lock", &format!("{recorded}{DISAGREEING_ENTRY}"));
            },
            &["check"],
        ),
        restricted_case("lock-read", |p| chmod(p, "supdoc.lock", 0o000), &["check"]),
        case(
            "lock-read",
            |p| {
                let lock = p.path().join("supdoc.lock");
                std::fs::remove_file(&lock).expect("the lockfile should be removable");
                std::fs::create_dir(&lock).expect("the directory should be creatable");
            },
            &["check"],
        ),
        case(
            "lock-too-large",
            |_| {},
            &["--max-lock-size", "16B", "check"],
        ),
        case(
            "lock-outside-root",
            |p| {
                let lock = p.path().join("supdoc.lock");
                std::fs::remove_file(&lock).expect("the lockfile should be removable");
                std::os::unix::fs::symlink("../outside.lock", &lock)
                    .expect("the symlink should be creatable");
            },
            &["check"],
        ),
        restricted_case(
            "lock-write",
            |p| {
                p.write("docs/new.md", "# N\n\nSee [add](../src/lib.rs#add).\n");
                chmod(p, ".", 0o555);
            },
            &["update", "--yes"],
        ),
        case(
            "lock-settings-mismatch",
            |p| config_with(p, "[features.hash]\nline-endings = \"exact\"\n"),
            &["check"],
        ),
        case(
            "scan-explicit-path-excluded",
            |p| p.write("supdoc.config.toml", "exclude = [\"docs/old.md\"]\n"),
            &["check", "docs/old.md"],
        ),
        case(
            "scan-explicit-path-excluded",
            |p| p.write("supdoc.config.toml", "include = [\"docs/guide.md\"]\n"),
            &["check", "docs/old.md"],
        ),
        case(
            "scan-explicit-path-ignored",
            |p| p.write(".gitignore", "docs/old.md\n"),
            &["check", "docs/old.md"],
        ),
        case(
            "scan-explicit-path-unscanned",
            |p| p.write("notes.txt", "x\n"),
            &["check", "notes.txt"],
        ),
        case(
            "scan-explicit-path-symlinked",
            |p| {
                std::os::unix::fs::symlink("src", p.path().join("alias"))
                    .expect("the symlink should be creatable");
            },
            &["check", "alias/lib.rs"],
        ),
        restricted_case(
            "scan-unreadable-dir",
            |p| {
                p.write("docs/private/a.md", "# A\n");
                chmod(p, "docs/private", 0o000);
            },
            &["check"],
        ),
        case(
            "scan-invalid-source-path",
            |p| p.write("docs/a\\b.md", "# A\n"),
            &["check"],
        ),
        restricted_case("scan-root-unreadable", |p| chmod(p, ".", 0o300), &["check"]),
        case(
            "ignore-too-large",
            |p| p.write(".gitignore", &"#".repeat(300_000)),
            &["check"],
        ),
        restricted_case(
            "scan-ignore-file-unreadable",
            |p| {
                p.write(".gitignore", "gen/\n");
                chmod(p, ".gitignore", 0o000);
            },
            &["check"],
        ),
        case(
            "scan-ignore-file-symlink",
            |p| {
                std::os::unix::fs::symlink("../outside-rules", p.path().join("docs/.gitignore"))
                    .expect("the symlink should be creatable");
            },
            &["check"],
        ),
        case("cli-usage", |_| {}, &["check", "--no-such-flag"]),
        case("cli-path-missing", |_| {}, &["check", "docs/nope.md"]),
        case("cli-path-missing", |_| {}, &["resolve", "nope.rs"]),
        case("cli-update-version-argument", |_| {}, &["update", "0.3.1"]),
        case(
            "cli-resolve-fragment",
            |_| {},
            &["resolve", "src/lib.rs#add"],
        ),
        restricted_case(
            "cli-path-unreadable",
            |p| {
                p.write("docs/private/a.md", "# A\n");
                chmod(p, "docs/private", 0o000);
            },
            &["check", "docs/private/a.md"],
        ),
        case("cli-path-outside-project", |_| {}, &["check", "/"]),
        case("cli-path-invalid", |_| {}, &["resolve", "src"]),
        case("cli-target-invalid", |_| {}, &["list", "src/lib.rs#a..b"]),
        case("cli-init-exists", |_| {}, &["init"]),
        case("cli-trust-path", |_| {}, &["trust", "nowhere"]),
        case("cli-trust-broad-root", |_| {}, &["trust", "/"]),
        case(
            "cli-trust-path",
            |p| {
                std::fs::create_dir(p.path().join("a\nb"))
                    .expect("the directory with a newline in its name should be creatable");
            },
            &["trust", "a\nb"],
        ),
        HelpCase {
            kind: "cli-trust-no-state",
            setup: |_| {},
            args: &["trust"],
            via: Via::EmptyHome,
            needs_permissions: false,
        },
        HelpCase {
            kind: "cli-trust-store",
            setup: |p| p.write("state-file", "not a directory"),
            args: &["trust"],
            via: Via::StateHomeIsFile,
            needs_permissions: false,
        },
        case(
            "cli-trust-no-config",
            |p| {
                std::fs::remove_file(p.path().join("supdoc.config.toml"))
                    .expect("the config should be removable");
            },
            &["trust"],
        ),
        restricted_case(
            "cli-init-write",
            |p| {
                std::fs::remove_file(p.path().join("supdoc.config.toml"))
                    .expect("the config should be removable");
                chmod(p, ".", 0o555);
            },
            &["init"],
        ),
        case(
            "cli-rename-unknown-namespace",
            |_| {},
            &["namespace", "rename", "nope", "other"],
        ),
        case(
            "cli-rename-unknown-namespace",
            |_| {},
            &["namespace", "rename", "cor", "other"],
        ),
        case(
            "cli-rename-unknown-namespace",
            |p| p.write("supdoc.config.toml", ""),
            &["namespace", "rename", "core", "other"],
        ),
        case(
            "cli-rename-target-exists",
            |p| config_with(p, "base = \"docs\"\n"),
            &["namespace", "rename", "core", "base"],
        ),
        case(
            "cli-rename-inherited",
            |p| {
                p.write(
                    "base/supdoc.config.toml",
                    "[namespaces]\nshared = \"src\"\n",
                );
                p.write(
                    "supdoc.config.toml",
                    "extends = \"base/supdoc.config.toml\"\n",
                );
            },
            &["namespace", "rename", "shared", "common"],
        ),
        case(
            "cli-rename-config-outside-root",
            |p| {
                p.write("app/src/lib.rs", "pub fn app() {}\n");
                std::os::unix::fs::symlink(
                    "../supdoc.config.toml",
                    p.path().join("app/supdoc.config.toml"),
                )
                .expect("the symlink should be creatable");
            },
            &[
                "-c",
                "app/supdoc.config.toml",
                "namespace",
                "rename",
                "core",
                "base",
            ],
        ),
        restricted_case(
            "cli-rename-unreadable-source",
            |p| chmod(p, "docs/guide.md", 0o000),
            &["namespace", "rename", "core", "base"],
        ),
        restricted_case(
            "cli-rename-write",
            |p| chmod(p, "docs", 0o555),
            &["namespace", "rename", "core", "base", "--yes"],
        ),
        case(
            "cli-skill-exists",
            |p| p.write(".agents/skills/supdoc/SKILL.md", "different"),
            &["skill", "install"],
        ),
        case("cli-upgrade-not-cargo", |_| {}, &["upgrade", "99.0.0"]),
        HelpCase {
            kind: "cli-upgrade-cargo-failed",
            setup: |p| {
                p.write(
                    "bin/cargo",
                    "#!/bin/sh\necho 'error: linker failed' >&2\nexit 101\n",
                );
            },
            args: &["upgrade", "99.0.0", "--yes"],
            via: Via::CargoInstalled,
            needs_permissions: false,
        },
        HelpCase {
            kind: "cli-upgrade-no-cargo",
            setup: |_| {},
            args: &["upgrade", "99.0.0", "--yes"],
            via: Via::CargoInstalled,
            needs_permissions: false,
        },
        HelpCase {
            kind: "cli-upgrade-latest-unknown",
            setup: |p| p.write("bin/cargo", "#!/bin/sh\necho 'no crates found'\n"),
            args: &["upgrade"],
            via: Via::CargoInstalled,
            needs_permissions: false,
        },
        HelpCase {
            kind: "cli-skill-no-home",
            setup: |_| {},
            args: &["skill", "install", "--global"],
            via: Via::EmptyHome,
            needs_permissions: false,
        },
        restricted_case(
            "cli-skill-write",
            |p| chmod(p, ".", 0o555),
            &["skill", "install", "--yes"],
        ),
        case(
            "cli-skill-outside-root",
            |p| {
                std::os::unix::fs::symlink("..", p.path().join(".agents"))
                    .expect("the fixture symlink should be creatable");
            },
            &["skill", "install"],
        ),
        HelpCase {
            kind: "cli-skill-relative-claude-dir",
            setup: |_| {},
            args: &["skill", "install", "--global"],
            via: Via::RelativeClaudeDir,
            needs_permissions: false,
        },
        HelpCase {
            kind: "cli-init-nested",
            setup: |_| {},
            args: &["init"],
            via: Via::Subdirectory,
            needs_permissions: false,
        },
        HelpCase {
            kind: "cli-no-cwd",
            setup: |_| {},
            args: &["check"],
            via: Via::RemovedCwd,
            needs_permissions: false,
        },
        restricted_case(
            "fix-write-failed",
            |p| {
                fixable(p);
                chmod(p, "docs", 0o555);
            },
            &["fix", "--yes"],
        ),
        restricted_case(
            "fix-lock-write-failed",
            |p| {
                p.write(
                    "src/lib.rs",
                    "pub fn add() {}\npub fn mul() {}\npub fn subs() {}\n",
                );
                chmod(p, ".", 0o555);
            },
            &["fix", "--yes"],
        ),
        unwritable("cli-output-failed", &["check"]),
        unwritable("cli-schema-write", &["schema"]),
        unwritable("cli-completions-write", &["completions", "bash"]),
    ];

    /// `supdoc args`, run by `sh` in the project root after `prelude`, with an empty environment but
    /// for `PATH` and the isolated git config that [`Project::command`] sets.
    fn shell(project: &Project, prelude: &str, args: &[&str]) -> Command {
        let empty = project.no_git_config();
        let mut command = Command::new("sh");
        let _command = command
            .arg("-c")
            .arg(format!("{prelude} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_supdoc"))
            .args(args)
            .current_dir(project.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", empty)
            .env("XDG_CONFIG_HOME", empty)
            .env("GIT_CONFIG_SYSTEM", empty.join("gitconfig"))
            .stdin(Stdio::null());
        command
    }

    /// Runs `args` with a copy of the binary installed in a cargo home inside `project`, whose
    /// record says cargo installed it from crates.io, and `PATH` set to the project's `bin`
    /// directory, where the setup may have put a fake `cargo`.
    fn run_in_cargo_home(
        project: &Project,
        args: &[&str],
    ) -> std::io::Result<std::process::Output> {
        use std::os::unix::fs::PermissionsExt as _;
        let cargo_home = project.path().join("cargo-home");
        let installed = cargo_home.join("bin/supdoc");
        std::fs::create_dir_all(cargo_home.join("bin"))
            .expect("the cargo home should be creatable");
        std::fs::create_dir_all(project.path().join("bin"))
            .expect("the PATH directory should be creatable");
        let _bytes = std::fs::copy(env!("CARGO_BIN_EXE_supdoc"), &installed)
            .expect("the binary should copy");
        std::fs::write(
            cargo_home.join(".crates2.json"),
            format!(
                r#"{{"installs":{{"supdoc {} (registry+https://github.com/rust-lang/crates.io-index)":{{"bins":["supdoc"]}}}}}}"#,
                env!("CARGO_PKG_VERSION")
            ),
        )
        .expect("the install record should be writable");
        let fake = project.path().join("bin/cargo");
        if fake.exists() {
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
                .expect("the fake cargo should be executable");
        }
        let mut command = std::process::Command::new(&installed);
        let _builder = command
            .args(args)
            .current_dir(project.path())
            .env_clear()
            .env("HOME", project.path().join("home"))
            .env("PATH", project.path().join("bin"))
            .env("CARGO_HOME", &cargo_home)
            .stdin(Stdio::null());
        retry_busy(|| command.output())
    }

    /// Runs `spawn`, retrying up to 5 times, 20 ms apart, while it fails with `ETXTBSY`. On Linux a
    /// binary just copied can refuse to start while another test's fork still holds the write
    /// descriptor; any other result is returned at once.
    fn retry_busy<T>(mut spawn: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
        let mut attempt = 1;
        loop {
            match spawn() {
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 5 =>
                {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => return result,
            }
        }
    }

    /// Runs `case` with `--json` and returns the arguments shown, every `(kind, help)` it printed and
    /// its stderr.
    fn run_help_case(case: &HelpCase) -> (String, Vec<(String, Option<String>)>, String) {
        let project = Project::clean();
        (case.setup)(&project);
        let mut args = case.args.to_vec();
        args.push("--json");
        let home_dir = project.path().join("home");
        let home = home_dir.to_str().expect("the temp path should be UTF-8");

        let output = match case.via {
            Via::Plain => project.command(&args, &[]).stdin(Stdio::null()).output(),
            Via::EmptyHome => project
                .command(&args, &[("HOME", "")])
                .stdin(Stdio::null())
                .output(),
            Via::RelativeClaudeDir => project
                .command(&args, &[("HOME", home), ("CLAUDE_CONFIG_DIR", "claude")])
                .stdin(Stdio::null())
                .output(),
            Via::FullStdout => {
                std::fs::create_dir_all(&home_dir).expect("the sink directory should be created");
                let sink = std::fs::File::create(home_dir.join("stdout.txt"))
                    .expect("the sink should be created");
                // A write past the limit fails with EFBIG once SIGXFSZ is ignored. Std treats a
                // closed stdout (EBADF) as a successful write, so that can't stand in for it.
                shell(&project, "trap '' XFSZ; ulimit -f 0", &args)
                    .stdout(sink)
                    .output()
            }
            Via::StateHomeIsFile => {
                let file = project.path().join("state-file");
                let file = file.to_str().expect("the temp path should be UTF-8");
                project
                    .command(&args, &[("XDG_STATE_HOME", file)])
                    .stdin(Stdio::null())
                    .output()
            }
            Via::Subdirectory => shell(&project, "mkdir sub && cd sub", &args).output(),
            Via::OutsideTools => {
                use std::os::unix::fs::PermissionsExt as _;
                let outside =
                    tempfile::TempDir::new().expect("a temporary directory should be creatable");
                let tools = std::fs::read_dir(project.path().join("tools"))
                    .expect("the setup should write a tools directory");
                for entry in tools {
                    let from = entry.expect("a tools entry should be readable").path();
                    let to = outside
                        .path()
                        .join(from.file_name().expect("a tool has a name"));
                    let _bytes = std::fs::copy(&from, &to).expect("the stub should copy");
                    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755))
                        .expect("the stub should be made executable");
                }
                let path = outside
                    .path()
                    .to_str()
                    .expect("the temp path should be UTF-8");
                project
                    .command(&args, &[("PATH", path)])
                    .stdin(Stdio::null())
                    .output()
            }
            Via::ProjectPath => {
                let bin = project.path().join("bin");
                let path = bin.to_str().expect("the temp path should be UTF-8");
                project
                    .command(&args, &[("PATH", path)])
                    .stdin(Stdio::null())
                    .output()
            }
            Via::RemovedCwd => {
                shell(&project, "mkdir gone && cd gone && rmdir ../gone", &args).output()
            }
            Via::CargoInstalled => run_in_cargo_home(&project, &args),
        }
        .expect("the supdoc binary should start");
        unlock(project.path());

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let mut helps = Vec::new();
        for line in stdout.lines().chain(stderr.lines()) {
            if let Ok(document) = serde_json::from_str::<serde_json::Value>(line) {
                collect_helps(&document, &mut helps);
            }
        }
        (args.join(" "), helps, stderr)
    }

    /// Pushes onto `problems` what is wrong with the helps `case` printed: a missing help on its own
    /// code, a changed pinned help, or a help that names no command or value.
    fn help_problems(
        case: &HelpCase,
        args: &str,
        helps: &[(String, Option<String>)],
        problems: &mut Vec<String>,
    ) {
        for (kind, help) in helps {
            let helpless = HELPLESS.iter().any(|entry| entry.kind == kind);
            if kind == case.kind && help.as_deref().is_none_or(str::is_empty) && !helpless {
                problems.push(format!(
                    "`{kind}` (from `supdoc {args}`) printed no help; give it a `.with_help` or \
                     list it in HELPLESS with a reason"
                ));
            }
            if let Some((_, pinned)) = PINNED_HELPS.iter().find(|(pin, _)| pin == kind)
                && help.as_deref().is_some_and(|help| help != *pinned)
            {
                problems.push(format!(
                    "the help of `{kind}` (from `supdoc {args}`) changed; expected {pinned:?}, \
                     got {help:?}"
                ));
            }
            if let Some(problem) = help.as_deref().and_then(help_problem) {
                problems.push(format!(
                    "the help of `{kind}` (from `supdoc {args}`) {problem}: {help:?}"
                ));
            }
        }
    }

    #[test]
    fn lock_over_max_lock_size_is_refused_with_exit_78() {
        let project = Project::clean();

        let output = project
            .command(&["--max-lock-size", "16B", "check"], &[])
            .stdin(Stdio::null())
            .output()
            .expect("the supdoc binary should start");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(78), "stderr: {stderr}");
        assert!(
            stderr.contains("error[lock-too-large]"),
            "the run should report lock-too-large, got: {stderr}"
        );
    }

    #[test]
    fn every_help_names_a_command_or_value() {
        let mut problems = Vec::new();
        let mut covered = BTreeSet::new();

        let restricted = permissions_restrict_this_user();
        for case in HELP_CASES {
            if case.needs_permissions && !restricted {
                // Written to the process's stderr directly, which `cargo test` shows even for a
                // passing test.
                let _ignored = writeln!(
                    std::io::stderr(),
                    "SKIPPED a `{}` help case: mode 000 does not restrict this user (running as \
                     root?)",
                    case.kind
                );
                let _new = covered.insert(case.kind.to_owned());
                continue;
            }
            let (args, helps, stderr) = run_help_case(case);
            if helps.iter().any(|(kind, _)| kind == case.kind) {
                let _new = covered.insert(case.kind.to_owned());
            } else {
                problems.push(format!(
                    "`supdoc {args}` should print `{}`; saw {helps:?}; stderr: {}",
                    case.kind,
                    stderr.trim()
                ));
            }
            help_problems(case, &args, &helps, &mut problems);
        }

        for unit in UNIT_CASES {
            let (kind, help) = (unit.build)();
            if kind != unit.kind {
                problems.push(format!(
                    "the unit case for `{}` built `{kind}` instead",
                    unit.kind
                ));
            }
            if help.as_deref().is_none_or(str::is_empty)
                && !HELPLESS.iter().any(|entry| entry.kind == kind)
            {
                problems.push(format!(
                    "`{kind}` (built directly) has no help; give it a `.with_help` or list it \
                     in HELPLESS with a reason"
                ));
            }
            if let Some(problem) = help.as_deref().and_then(help_problem) {
                problems.push(format!(
                    "the help of `{kind}` (built directly) {problem}: {help:?}"
                ));
            }
            if (kind == "read-internal" || kind == "read-cartridge-load")
                && !help.as_deref().is_some_and(|h| h.contains(ISSUES_URL))
            {
                problems.push(format!(
                    "the help of `{kind}` is an exit-70 help and should name {ISSUES_URL}: {help:?}"
                ));
            }
            if kind == "read-cartridge-load"
                && !help
                    .as_deref()
                    .is_some_and(|h| h.contains("cargo install --locked supdoc"))
            {
                problems.push(format!(
                    "the help of `{kind}` should name `cargo install --locked supdoc`: {help:?}"
                ));
            }
            let _new = covered.insert(unit.kind.to_owned());
        }

        for helpless in HELPLESS {
            assert!(
                covered.contains(helpless.kind),
                "`{}` is listed as helpless ({}) but no case prints it",
                helpless.kind,
                helpless.why
            );
        }

        for unreachable in UNREACHABLE {
            assert!(
                covered.insert(unreachable.kind.to_owned()),
                "`{}` is listed as unreachable ({}) but a case reaches it; drop it from \
                 UNREACHABLE",
                unreachable.kind,
                unreachable.why
            );
        }

        let inventory = source_codes();
        let uncovered: Vec<_> = inventory.difference(&covered).collect();
        let unknown: Vec<_> = covered.difference(&inventory).collect();
        if !uncovered.is_empty() {
            problems.push(format!(
                "these codes in crates/supdoc/src have no case in HELP_CASES, UNIT_CASES or \
                 UNREACHABLE: {uncovered:?}"
            ));
        }
        if !unknown.is_empty() {
            problems.push(format!(
                "these cases name codes crates/supdoc/src no longer has: {unknown:?}"
            ));
        }

        problems.sort();
        problems.dedup();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
