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

/// 07 G3: every help names a fix. Unix only, since some codes are reached through file
/// permissions, `sh` and a file-size limit.
#[cfg(unix)]
mod helps {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use crate::support::Project;

    /// How a [`HelpCase`] runs `supdoc`, beyond its arguments.
    #[derive(Clone, Copy)]
    enum Via {
        /// In the project root with an empty environment.
        Plain,
        /// With `CLAUDE_CONFIG_DIR` set to a relative path and `HOME` to a directory in the project.
        RelativeClaudeDir,
        /// With standard output sent to a file under a zero file-size limit, so every write to it
        /// fails, while standard error stays a pipe.
        FullStdout,
        /// From a current directory that a shell removed after entering it.
        RemovedCwd,
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
            kind: "cli-rename-file-changed",
            why: "a file must change between planning and writing one rename",
        },
        Unreachable {
            kind: "cli-schema-serialize",
            why: "a built-in JSON Schema must fail to serialize, which is a supdoc bug",
        },
    ];

    /// A code whose message a test can build through the library, with the code and help it got.
    struct UnitCase {
        /// The code the constructor should give.
        kind: &'static str,
        /// Builds the message and returns its code and help.
        build: fn() -> (String, Option<String>),
    }

    /// Codes no binary run can print reliably, built through the library instead.
    const UNIT_CASES: &[UnitCase] = &[
        UnitCase {
            kind: "cli-not-built",
            build: || {
                let error = supdoc::cli::CliError::not_built("check");
                (error.code().to_owned(), Some(error.help().to_owned()))
            },
        },
        UnitCase {
            kind: "read-internal",
            build: || {
                let path =
                    supdoc::path::AbsolutePath::new(std::path::Path::new("/project/src/lib.rs"))
                        .expect("the fixture path should be absolute");
                let error = supdoc::read::error::ReadError::Internal { path };
                (error.code().to_owned(), Some(error.help()))
            },
        },
        UnitCase {
            kind: "fix-file-changed",
            build: file_changed_during_fix,
        },
    ];

    /// The finding for a doc that changed between planning and applying a fix.
    fn file_changed_during_fix() -> (String, Option<String>) {
        let project = Project::clean();
        project.write("docs/api.md", "# API\n\nSee [ad](../src/lib.rs#ad).\n");
        let real = std::fs::canonicalize(project.path()).expect("the project root should resolve");
        let root =
            supdoc::path::AbsolutePath::new(&real).expect("a canonical path should be absolute");
        let config = supdoc::config::Config::load(&root).expect("the fixture config should load");
        let languages = supdoc::read::cartridge::Languages::builtin();
        let scan =
            supdoc::scan::assemble::run(&config, &languages, &supdoc::scan::model::Scope::All)
                .expect("the fixture project should scan");
        let plan = supdoc::check::fix::plan_fixes(&scan);
        project.write(
            "docs/api.md",
            "# API\n\nEdited after the scan.\n\nSee [ad](../src/lib.rs#ad).\n",
        );
        let result = supdoc::check::fix::apply_fixes(&plan, &root);
        let finding = result
            .skipped
            .first()
            .expect("a doc edited after the scan should be skipped");
        (finding.code.to_owned(), finding.help.clone())
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
        help.split_whitespace().count() < 3
            || POINTERS.iter().any(|pointer| lowered.contains(pointer))
    }

    /// What is wrong with `help` under 07 G3, if anything: it must quote a runnable command or a
    /// concrete value (`supdoc update docs/guide.md --yes`, `did you mean log_in?`) and must not send
    /// the reader elsewhere.
    fn help_problem(help: &str) -> Option<&'static str> {
        if points_elsewhere(help) {
            Some("should say what to do, not send the reader elsewhere")
        } else if !quotes_a_command_or_value(help) {
            Some("should quote a runnable command or concrete value in backticks")
        } else {
            None
        }
    }

    /// Every code in supdoc's production source: each `"area-name"` string literal outside
    /// `tests.rs`, `fixtures.rs`, comments and the `mod tests` that ends a file.
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
                    let production = text
                        .lines()
                        .take_while(|line| !line.trim_start().starts_with("mod tests"))
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

    /// Whether `text` has the shape of a code: `area-name` in kebab-case, whose first word is one
    /// of supdoc's code areas, such as `link-failed` or `cli-usage`.
    fn is_code(text: &str) -> bool {
        const AREAS: &[&str] = &[
            "check", "cli", "config", "fix", "link", "lock", "read", "resolve", "scan",
        ];
        let Some((area, name)) = text.split_once('-') else {
            return false;
        };
        AREAS.contains(&area)
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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

    /// A doc whose one link `supdoc fix` can repair: `ad` is a misspelling of `add`.
    fn fixable(project: &Project) {
        project.write("docs/api.md", "# API\n\nSee [ad](../src/lib.rs#ad).\n");
    }

    /// Every message with a `help` that a user can be made to see, one way to reach each code.
    const HELP_CASES: &[HelpCase] = &[
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
            "read-unsupported-language",
            |p| {
                p.write("src/data.xyz", "x");
                p.write("docs/guide.md", "[x](../src/data.xyz#add)\n");
            },
            &["check"],
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
        case("read-io", |p| chmod(p, "src/lib.rs", 0o000), &["check"]),
        case("read-io", |p| chmod(p, "docs/old.md", 0o000), &["check"]),
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
            "config-not-found",
            |_| {},
            &["check", "--config", "nope.toml"],
        ),
        case(
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
        case("lock-read", |p| chmod(p, "supdoc.lock", 0o000), &["check"]),
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
        case(
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
            "scan-unreadable-dir",
            |p| {
                p.write("docs/private/a.md", "# A\n");
                chmod(p, "docs/private", 0o000);
            },
            &["check"],
        ),
        case("scan-root-unreadable", |p| chmod(p, ".", 0o300), &["check"]),
        case("cli-usage", |_| {}, &["check", "--no-such-flag"]),
        case("cli-path-missing", |_| {}, &["check", "docs/nope.md"]),
        case("cli-path-missing", |_| {}, &["resolve", "nope.rs"]),
        case("cli-path-outside-project", |_| {}, &["check", "/"]),
        case("cli-path-invalid", |_| {}, &["resolve", "src"]),
        case("cli-target-invalid", |_| {}, &["list", "src/lib.rs#a..b"]),
        case("cli-init-exists", |_| {}, &["init"]),
        case(
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
            "cli-rename-unreadable-source",
            |p| chmod(p, "docs/guide.md", 0o000),
            &["namespace", "rename", "core", "base"],
        ),
        case(
            "cli-rename-write",
            |p| chmod(p, "docs", 0o555),
            &["namespace", "rename", "core", "base", "--yes"],
        ),
        case(
            "cli-skill-exists",
            |p| p.write(".agents/skills/supdoc/SKILL.md", "different"),
            &["skill", "install"],
        ),
        case(
            "cli-skill-no-home",
            |_| {},
            &["skill", "install", "--global"],
        ),
        case(
            "cli-skill-write",
            |p| chmod(p, ".", 0o555),
            &["skill", "install", "--yes"],
        ),
        HelpCase {
            kind: "cli-skill-relative-claude-dir",
            setup: |_| {},
            args: &["skill", "install", "--global"],
            via: Via::RelativeClaudeDir,
        },
        HelpCase {
            kind: "cli-no-cwd",
            setup: |_| {},
            args: &["check"],
            via: Via::RemovedCwd,
        },
        case(
            "fix-write-failed",
            |p| {
                fixable(p);
                chmod(p, "docs", 0o555);
            },
            &["fix", "--yes"],
        ),
        case(
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
        unwritable("cli-check-output", &["check"]),
        unwritable("cli-update-output", &["update"]),
        unwritable("cli-fix-output", &["fix"]),
        unwritable("cli-list-output", &["list"]),
        unwritable("cli-resolve-output", &["resolve", "src/lib.rs"]),
        unwritable("cli-config-output", &["config", "show"]),
        unwritable("cli-namespace-output", &["namespace", "list"]),
        unwritable(
            "cli-rename-output",
            &["namespace", "rename", "core", "base"],
        ),
        unwritable("cli-skill-output", &["skill", "install"]),
        unwritable("cli-schema-write", &["schema"]),
        unwritable("cli-completions-write", &["completions", "bash"]),
    ];

    /// `supdoc args`, run by `sh` in the project root after `prelude`, with an empty environment but
    /// for `PATH`.
    fn shell(project: &Project, prelude: &str, args: &[&str]) -> Command {
        let mut command = Command::new("sh");
        let _command = command
            .arg("-c")
            .arg(format!("{prelude} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_supdoc"))
            .args(args)
            .current_dir(project.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null());
        command
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
            Via::RemovedCwd => {
                shell(&project, "mkdir gone && cd gone && rmdir ../gone", &args).output()
            }
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

    #[test]
    fn every_help_names_a_command_or_value() {
        let mut problems = Vec::new();
        let mut covered = BTreeSet::new();

        for case in HELP_CASES {
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
            for (kind, help) in &helps {
                if let Some(problem) = help.as_deref().and_then(help_problem) {
                    problems.push(format!(
                        "the help of `{kind}` (from `supdoc {args}`) {problem} (07 G3): {help:?}"
                    ));
                }
            }
        }

        for unit in UNIT_CASES {
            let (kind, help) = (unit.build)();
            if kind != unit.kind {
                problems.push(format!(
                    "the unit case for `{}` built `{kind}` instead",
                    unit.kind
                ));
            }
            if let Some(problem) = help.as_deref().and_then(help_problem) {
                problems.push(format!(
                    "the help of `{kind}` (built directly) {problem} (07 G3): {help:?}"
                ));
            }
            let _new = covered.insert(unit.kind.to_owned());
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
