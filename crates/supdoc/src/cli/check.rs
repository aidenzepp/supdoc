//! `supdoc check`: compares every link in scope with `supdoc.lock` and reports which fail (05).
//!
//! Read-only: `check` never writes a file (05 C2). The run is `config::load → Lockfile::read →
//! scan → reconcile → check_outcome → render`, with effects only at the two ends.
//!
//! This module also owns the wire form of one link, [`LinkJson`], which `list` shares.

use std::io::{self, Write};
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Serialize;

use crate::check::model::{LinkReport, Report as CheckModel};
use crate::check::reconcile::{self, check_outcome, reconcile};
use crate::config::Config;
use crate::finding::{Finding, Location};
use crate::link::{LinkKey, SourcePath};
use crate::lock::error::LockError;
use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
use crate::output::json::FindingJson;
use crate::output::summary::render_summary;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, Report, Summary, TextContext, Verbosity};
use crate::path::AbsolutePath;
use crate::read::cartridge::Languages;
use crate::scan::model::Scope;
use crate::state::LinkState;

use super::args::CheckArgs;
use super::{CliError, Context, Outcome};

/// The exit code for a usage error, such as a missing `[PATH]` (06 X7).
const USAGE_EXIT_CODE: u8 = 2;

/// The exit code for an I/O failure (06 X7).
const IO_EXIT_CODE: u8 = 74;

/// The exit code for a config or lockfile problem (06 X7).
const CONFIG_EXIT_CODE: u8 = 78;

/// Runs `supdoc check`: prints every link that needs attention, the findings not tied to one
/// link, and the summary line, then says whether the check passed (05 C3).
///
/// A missing `supdoc.lock` is an empty one, so every link is `new`.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), `cli/path-missing`,
/// `cli/path-outside-project` or `cli/path-invalid` for a bad `[PATH]` (2), the lockfile's own
/// error (78 when invalid, 74 when unreadable), `scan/root-unreadable` (74), and
/// `cli/check-output` when the report cannot be written (74).
pub fn run(args: &CheckArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let config = load_config(context)?;
    let scope = scope(&args.paths, &context.cwd, config.root())?;
    let lock = read_lock(config.root())?;
    let scan =
        crate::scan::assemble::run(&config, &Languages::builtin(), &scope).map_err(|error| {
            CliError::new(error.code(), error.to_string(), error.help(), IO_EXIT_CODE)
        })?;

    let model = reconcile(&scan, &lock, &scope);
    let outcome = match check_outcome(&model, args.deny_warnings) {
        reconcile::Outcome::Clean => Outcome::Clean,
        reconcile::Outcome::Failing => Outcome::Failing,
    };
    let verbosity = Verbosity::from_flags(context.global.quiet, context.global.verbose);
    let report = CheckReport::new(model, verbosity);
    output
        .report(&report, outcome == Outcome::Clean)
        .map_err(|error| output_failed(&error))?;
    Ok(outcome)
}

/// The config `--config` names, or the one found by searching upward from the current directory.
///
/// # Errors
///
/// Returns the config's own error, with its code, help and exit code.
fn load_config(context: &Context) -> Result<Config, CliError> {
    let loaded = match &context.global.config {
        Some(file) => Config::load_file(&absolute(&context.cwd, file)),
        None => Config::load(&context.cwd),
    };
    loaded.map_err(|error| {
        let help = error.help().unwrap_or_else(|| {
            "Correct the config as the message says, then run `supdoc check` again".to_owned()
        });
        CliError::new(error.code(), error.to_string(), help, error.exit_code())
    })
}

/// `path` as typed, made absolute against the current directory.
fn absolute(cwd: &AbsolutePath, path: &std::path::Path) -> AbsolutePath {
    // Joining onto an absolute directory always gives an absolute path.
    AbsolutePath::new(&cwd.as_path().join(path)).unwrap_or_else(|| cwd.clone())
}

/// The part of the project `[PATHS]` names: everything when none are given, or when one of them
/// is the project root itself (04 S10).
///
/// # Errors
///
/// Returns `cli/path-missing` for a path that doesn't exist, `cli/path-outside-project` for one
/// that isn't inside the project root, and `cli/path-invalid` for one no source path can name.
/// All exit 2.
fn scope(paths: &[PathBuf], cwd: &AbsolutePath, root: &AbsolutePath) -> Result<Scope, CliError> {
    let mut sources = Vec::with_capacity(paths.len());
    for typed in paths {
        let path = absolute(cwd, typed);
        if path.as_path().symlink_metadata().is_err() {
            return Err(path_missing(typed));
        }
        let Some(inside) = path.strip_prefix(root) else {
            return Err(path_outside(typed, root));
        };
        if inside.as_os_str().is_empty() {
            return Ok(Scope::All);
        }
        let source =
            SourcePath::from_relative(inside).map_err(|error| path_invalid(typed, &error))?;
        sources.push(source);
    }
    Ok(if sources.is_empty() {
        Scope::All
    } else {
        Scope::Paths(sources)
    })
}

/// The lockfile in the project root, or an empty one when there is none yet (06 "What each
/// command touches").
///
/// # Errors
///
/// Returns the lockfile's own error: 74 when it can't be read, 78 when its text is invalid.
fn read_lock(root: &AbsolutePath) -> Result<Lockfile, CliError> {
    let path = AbsolutePath::new(&root.as_path().join(LOCK_FILE_NAME)).unwrap_or_else(|| {
        unreachable!("the lockfile name joined onto an absolute root is absolute")
    });
    match Lockfile::read(&path) {
        Ok(lock) => Ok(lock.unwrap_or_default()),
        Err(error) => {
            let exit_code = match error {
                LockError::Parse { .. } => CONFIG_EXIT_CODE,
                LockError::Read { .. } | LockError::Write { .. } => IO_EXIT_CODE,
            };
            Err(CliError::new(
                error.code(),
                error.to_string(),
                error.help(),
                exit_code,
            ))
        }
    }
}

/// The error for a `[PATH]` that doesn't exist (06 error table).
fn path_missing(typed: &std::path::Path) -> CliError {
    CliError::new(
        "cli/path-missing",
        format!("{} does not exist", typed.display()),
        "Check the path. Paths are relative to the current directory",
        USAGE_EXIT_CODE,
    )
}

/// The error for a `[PATH]` outside the project, whose links `check` can't name.
fn path_outside(typed: &std::path::Path, root: &AbsolutePath) -> CliError {
    CliError::new(
        "cli/path-outside-project",
        format!(
            "{} is outside the project at {}",
            typed.display(),
            root.display()
        ),
        format!(
            "Pass a path inside {}, or run supdoc from the project that contains it",
            root.display()
        ),
        USAGE_EXIT_CODE,
    )
}

/// The error for a `[PATH]` inside the project that no link source could have.
fn path_invalid(typed: &std::path::Path, error: &impl std::fmt::Display) -> CliError {
    CliError::new(
        "cli/path-invalid",
        format!("{} can't be checked: {error}", typed.display()),
        "Pass a file or directory whose path supdoc.lock could record, or leave out [PATHS] to check the whole project",
        USAGE_EXIT_CODE,
    )
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/check-output",
        format!("cannot write the check report: {error}"),
        "Check that standard output and standard error are open and writable",
        IO_EXIT_CODE,
    )
}

/// What `check` prints: the counts, the links to show, and the findings not tied to one link
/// (07 G8).
///
/// JSON puts `summary` and `links` under `data`; the envelope carries the loose findings. Passed
/// links are always counted but listed only under `--verbose` (07 G6).
#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckReport {
    /// How many links are in each state, passed ones included.
    summary: Summary,
    /// The links shown, each with the finding that explains its state.
    links: Vec<LinkJson>,
    /// The same links with their full findings, for text and GitHub output.
    #[serde(skip)]
    shown: Vec<LinkReport>,
    /// Findings not tied to one link.
    #[serde(skip)]
    loose: Vec<Finding>,
}

impl CheckReport {
    /// The report for `model`, listing passed links only when `verbosity` asks for them.
    #[must_use]
    pub fn new(model: CheckModel, verbosity: Verbosity) -> Self {
        let summary = Summary::tally(model.links.iter().map(|link| link.state));
        let shown: Vec<LinkReport> = model
            .links
            .into_iter()
            .filter(|link| link.state != LinkState::Passed || verbosity.lists_passed())
            .collect();
        Self {
            summary,
            links: shown.iter().map(LinkJson::from).collect(),
            shown,
            loose: model.findings,
        }
    }
}

impl Report for CheckReport {
    fn command(&self) -> &'static str {
        "check"
    }

    fn summary(&self) -> Option<&Summary> {
        Some(&self.summary)
    }

    fn findings(&self) -> &[Finding] {
        &self.loose
    }

    fn annotated_findings(&self) -> Vec<&Finding> {
        self.shown
            .iter()
            .filter_map(|link| link.finding.as_ref())
            .chain(&self.loose)
            .collect()
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        for link in &self.shown {
            match &link.finding {
                Some(finding) => render_finding(context, finding, out)?,
                None => render_passed(context, &link.key, out)?,
            }
        }
        for finding in &self.loose {
            render_finding(context, finding, out)?;
        }
        render_summary(context, &self.summary, out)
    }
}

/// `passed  docs/guide.md -> src/lib.rs#add`, the line `--verbose` shows for a passed link.
///
/// # Errors
///
/// Fails if writing to `out` fails.
fn render_passed(context: &TextContext, key: &LinkKey, out: &mut dyn Write) -> io::Result<()> {
    let arrow = if context.unicode { "→" } else { "->" };
    let mut target = key.target.to_string();
    if let Some(symbol) = &key.symbol {
        target = format!("{target}#{symbol}");
    }
    writeln!(
        out,
        "passed  {} {arrow} {}\n",
        visible(&context.paths.text(&key.source)),
        visible(&target),
    )
}

/// One link as `check --json` and `list --json` show it (07 "JSON output").
///
/// `source`, `target` and `symbol` sit beside the state. The explaining finding is nested
/// without a location, since `locations` already says where the link is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct LinkJson {
    /// The link's state.
    pub state: LinkState,
    /// Which link: `source`, `target` and, for a symbol link, `symbol`.
    #[serde(flatten)]
    pub key: LinkKey,
    /// Every place it appears in its source. Empty only for `obsolete`.
    pub locations: Vec<LinkLocationJson>,
    /// The finding that explains the state. Left out for `passed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<FindingJson>,
}

impl From<&LinkReport> for LinkJson {
    fn from(link: &LinkReport) -> Self {
        Self {
            state: link.state,
            key: link.key.clone(),
            locations: link.locations.iter().map(LinkLocationJson::from).collect(),
            finding: link.finding.as_ref().map(FindingJson::nested),
        }
    }
}

/// Where a link appears in its source. The file is the link's `source`, so it isn't repeated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct LinkLocationJson {
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
    /// Start byte offset.
    pub start: usize,
    /// End byte offset.
    pub end: usize,
}

impl From<&Location> for LinkLocationJson {
    fn from(location: &Location) -> Self {
        Self {
            line: location.line,
            column: location.column,
            start: location.range.start,
            end: location.range.end,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::{Cli, Command};
    use crate::output::{OutputSettings, PathStyle};
    use crate::scan::model::TargetState;

    /// A file that documents `add` in `src/lib.rs`.
    const GUIDE: &str = "# Guide\n\nThe [add](../src/lib.rs#add) function sums.\n";

    /// The code `GUIDE` points at.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

    /// A buffer `Output` writes into and the test reads back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the capture lock should be free");
            String::from_utf8(bytes.clone()).expect("captured output should be UTF-8")
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("the capture lock should be free")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The value at `pointer` (RFC 6901) in `json`, or a failure naming what is missing.
    fn at(json: &serde_json::Value, pointer: &str) -> serde_json::Value {
        json.pointer(pointer)
            .unwrap_or_else(|| panic!("`{pointer}` should be in the JSON output: {json}"))
            .clone()
    }

    /// What one `supdoc check` run returned and printed.
    struct Run {
        result: Result<Outcome, CliError>,
        stdout: String,
        stderr: String,
    }

    impl Run {
        /// The exit code the process would end with.
        fn exit_code(&self) -> u8 {
            match &self.result {
                Ok(outcome) => outcome.exit_code(),
                Err(error) => error.exit_code(),
            }
        }

        /// stdout parsed as the one JSON document `--json` prints.
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.stdout)
                .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", self.stdout))
        }
    }

    /// A project in a fresh temporary directory, with the guide and the code it documents.
    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        fn new() -> Self {
            let project = Self {
                dir: tempfile::tempdir().expect("a fresh temp directory should be created"),
            };
            project.write("docs/guide.md", GUIDE);
            project.write("src/lib.rs", LIB);
            project
        }

        fn root(&self) -> AbsolutePath {
            AbsolutePath::new(self.dir.path()).expect("a temp directory should be absolute")
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be created");
            }
            fs::write(&path, text).expect("the fixture file should be written");
        }

        /// Records every present link's current hash in `supdoc.lock`, as `update --yes` would.
        fn record_all(&self) {
            let root = self.root();
            let config = Config::load(&root).expect("the fixture config should load");
            let scan = crate::scan::assemble::run(&config, &Languages::builtin(), &Scope::All)
                .expect("the fixture project should scan");
            let mut lock = Lockfile::default();
            for (key, link) in scan.links {
                if let TargetState::Present { hash, .. } = link.target {
                    let _previous = lock.insert(key, hash);
                }
            }
            assert!(!lock.is_empty(), "the fixture should have a link to record");
            let path = AbsolutePath::new(&self.dir.path().join(LOCK_FILE_NAME))
                .expect("the lockfile path should be absolute");
            lock.write(&path)
                .expect("the fixture lockfile should be written");
        }

        /// Every file under the root and its bytes.
        fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
            fn collect(dir: &Path, found: &mut BTreeMap<PathBuf, Vec<u8>>) {
                for entry in fs::read_dir(dir).expect("a fixture directory should be readable") {
                    let path = entry.expect("a directory entry should be readable").path();
                    if path.is_dir() {
                        collect(&path, found);
                    } else {
                        let bytes = fs::read(&path).expect("a fixture file should be readable");
                        let _previous = found.insert(path, bytes);
                    }
                }
            }
            let mut found = BTreeMap::new();
            collect(self.dir.path(), &mut found);
            found
        }

        /// Runs `supdoc <args>` from the project root. `args` must start with `check`.
        fn check(&self, args: &[&str]) -> Run {
            let command_line: Vec<&str> = std::iter::once("supdoc")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&command_line).unwrap_or_else(|error| {
                panic!("`{}` should parse: {error}", command_line.join(" "))
            });
            let Command::Check(check_args) = &cli.command else {
                panic!("`{}` should be a check command", command_line.join(" "));
            };
            let cwd = self.root();
            let settings = OutputSettings {
                format: cli.global.format(),
                verbosity: Verbosity::from_flags(cli.global.quiet, cli.global.verbose),
                color: false,
                unicode: false,
                width: 80,
                paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
            };
            let (stdout, stderr) = (Capture::default(), Capture::default());
            let mut output =
                Output::new(settings, Box::new(stdout.clone()), Box::new(stderr.clone()));
            let context = Context {
                global: cli.global.clone(),
                cwd,
            };
            let result = run(check_args, &context, &mut output);
            Run {
                result,
                stdout: stdout.text(),
                stderr: stderr.text(),
            }
        }
    }

    #[test]
    fn recorded_unchanged_link_passes_with_exit_0() {
        let project = Project::new();
        project.record_all();

        let run = project.check(&["check"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "Links  1 passed\n");
    }

    #[test]
    fn unrecorded_link_is_new_and_fails_with_exit_1() {
        let project = Project::new();

        let run = project.check(&["check"]);

        assert_eq!(run.exit_code(), 1, "stderr: {}", run.stderr);
        assert!(
            run.stdout.contains("error[link/new]"),
            "stdout: {}",
            run.stdout
        );
        assert!(
            run.stdout.ends_with("Links  1 new\n"),
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn changed_target_is_failed_and_fails_the_check() {
        let project = Project::new();
        project.record_all();
        project.write(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        );

        let run = project.check(&["check"]);

        assert_eq!(run.exit_code(), 1, "stderr: {}", run.stderr);
        assert!(
            run.stdout.contains("error[link/failed]"),
            "stdout: {}",
            run.stdout
        );
        assert!(
            run.stdout.contains("docs/guide.md:3:5"),
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn missing_target_is_broken_and_fails_the_check() {
        let project = Project::new();
        project.record_all();
        project.write(
            "src/lib.rs",
            "pub fn sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );

        let run = project.check(&["--json", "check"]);

        assert_eq!(run.exit_code(), 1, "stderr: {}", run.stderr);
        assert_eq!(
            at(&run.json(), "/data/summary/broken"),
            1,
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn obsolete_entry_only_warns_unless_warnings_are_denied() {
        let project = Project::new();
        project.record_all();
        project.write("docs/guide.md", "# Guide\n\nNo links any more.\n");

        let lenient = project.check(&["check"]);
        let strict = project.check(&["check", "--deny-warnings"]);

        assert_eq!(lenient.exit_code(), 0, "stdout: {}", lenient.stdout);
        assert!(
            lenient.stdout.contains("warning[link/obsolete]"),
            "stdout: {}",
            lenient.stdout
        );
        assert_eq!(strict.exit_code(), 1, "stdout: {}", strict.stdout);
    }

    #[test]
    fn check_never_writes_any_file() {
        for recorded in [false, true] {
            let project = Project::new();
            if recorded {
                project.record_all();
                // Make one link failed and add an obsolete entry's worth of drift.
                project.write("src/lib.rs", "pub fn add() {}\n");
            }
            let before = project.files();

            let runs = [
                project.check(&["check"]),
                project.check(&["--json", "check", "--deny-warnings"]),
                project.check(&["check", "docs/guide.md"]),
            ];

            assert!(
                runs.iter().all(|run| run.result.is_ok()),
                "every run should finish (recorded: {recorded})"
            );
            assert_eq!(
                project.files(),
                before,
                "check changed the project (recorded: {recorded})"
            );
        }
    }

    #[test]
    fn exit_codes_match_table() {
        let clean = Project::new();
        clean.record_all();
        let failing = Project::new();
        let bad_config = Project::new();
        bad_config.write("supdoc.config.toml", "include = 3\n");
        let conflicted_lock = Project::new();
        conflicted_lock.write(LOCK_FILE_NAME, "<<<<<<< HEAD\n=======\n>>>>>>> theirs\n");
        let unreadable_lock = Project::new();
        fs::create_dir(unreadable_lock.dir.path().join(LOCK_FILE_NAME))
            .expect("a directory in place of the lockfile should be created");

        let cases = [
            ("clean", clean.check(&["check"]), 0),
            ("failing links", failing.check(&["check"]), 1),
            ("missing path", failing.check(&["check", "docs/nope.md"]), 2),
            ("invalid config", bad_config.check(&["check"]), 78),
            ("lockfile conflict", conflicted_lock.check(&["check"]), 78),
            ("unreadable lockfile", unreadable_lock.check(&["check"]), 74),
        ];

        for (name, run, expected) in cases {
            assert_eq!(
                run.exit_code(),
                expected,
                "{name}: {:?}",
                run.result.as_ref().err().map(CliError::code)
            );
        }
    }

    #[test]
    fn missing_path_is_a_usage_error_naming_the_path() {
        let project = Project::new();

        let run = project.check(&["check", "docs/nope.md"]);

        let error = run.result.expect_err("a missing path should be an error");
        assert_eq!(error.code(), "cli/path-missing");
        assert!(
            error.message().contains("docs/nope.md"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn path_outside_the_project_is_a_usage_error() {
        let project = Project::new();
        let outside = project
            .dir
            .path()
            .parent()
            .expect("a temp directory should have a parent")
            .display()
            .to_string();

        let run = project.check(&["check", &outside]);

        let error = run
            .result
            .expect_err("a path outside the project should be an error");
        assert_eq!(error.code(), "cli/path-outside-project");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn paths_narrow_the_check_and_leave_other_entries_unjudged() {
        let project = Project::new();
        project.write("docs/other.md", "See [add](../src/lib.rs#add).\n");
        project.record_all();
        project.write("docs/other.md", "No links here.\n");

        let narrowed = project.check(&["--json", "check", "docs/guide.md"]);
        let whole = project.check(&["--json", "check"]);

        assert_eq!(narrowed.exit_code(), 0, "stdout: {}", narrowed.stdout);
        assert_eq!(
            at(&narrowed.json(), "/data/summary"),
            serde_json::json!({ "passed": 1, "failed": 0, "broken": 0, "new": 0, "obsolete": 0 }),
        );
        assert_eq!(at(&whole.json(), "/data/summary/obsolete"), 1);
    }

    #[test]
    fn json_nests_link_findings_and_keeps_loose_findings_top_level() {
        let project = Project::new();
        project.write("supdoc.config.toml", "exclude = [\"private\"]\n");
        project.write("private/notes.md", "[add](../src/lib.rs#add)\n");

        let run = project.check(&["--json", "check", "private/notes.md"]);
        let unscoped = project.check(&["--json", "check"]);

        let json = run.json();
        assert_eq!(at(&json, "/command"), "check");
        assert_eq!(
            at(&json, "/ok"),
            true,
            "only an excluded path was named: {json}"
        );
        assert_eq!(
            at(&json, "/data")
                .as_object()
                .map(|data| data.keys().cloned().collect::<Vec<_>>()),
            Some(vec!["links".to_owned(), "summary".to_owned()]),
            "data holds only summary and links: {json}"
        );
        let loose = at(&json, "/findings");
        assert_eq!(
            loose.as_array().map(Vec::len),
            Some(1),
            "the excluded path should be warned about: {json}"
        );

        let json = unscoped.json();
        assert_eq!(at(&json, "/ok"), false);
        let link = at(&json, "/data/links/0");
        assert_eq!(at(&link, "/state"), "new");
        assert_eq!(at(&link, "/source"), "docs/guide.md");
        assert_eq!(at(&link, "/target"), "src/lib.rs");
        assert_eq!(at(&link, "/symbol"), "add");
        let written = "[add](../src/lib.rs#add)";
        let start = GUIDE.find(written).expect("the guide should hold the link");
        assert_eq!(
            at(&link, "/locations"),
            serde_json::json!([{
                "line": 3, "column": 5, "start": start, "end": start + written.len(),
            }]),
            "a link location carries no `file`: the link's `source` names it",
        );
        assert_eq!(at(&link, "/finding/kind"), "link/new");
        assert!(
            at(&link, "/finding").get("location").is_none(),
            "a nested finding leaves its location to the link: {link}"
        );
    }

    #[test]
    fn passed_links_are_listed_only_when_verbose() {
        let project = Project::new();
        project.record_all();

        let normal = project.check(&["--json", "check"]);
        let verbose = project.check(&["--json", "-v", "check"]);
        let verbose_text = project.check(&["-v", "check"]);

        assert_eq!(at(&normal.json(), "/data/links"), serde_json::json!([]));
        assert_eq!(at(&normal.json(), "/data/summary/passed"), 1);
        assert_eq!(at(&verbose.json(), "/data/links/0/state"), "passed");
        assert!(
            verbose_text
                .stdout
                .contains("passed  docs/guide.md -> src/lib.rs#add"),
            "stdout: {}",
            verbose_text.stdout
        );
    }

    #[test]
    fn quiet_clean_run_prints_nothing() {
        let project = Project::new();
        project.record_all();

        let run = project.check(&["-q", "check"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!((run.stdout.as_str(), run.stderr.as_str()), ("", ""));
    }

    #[test]
    fn check_json_validates_against_its_schema() {
        let project = Project::new();
        let run = project.check(&["--json", "check"]);
        let schema = serde_json::to_value(schemars::schema_for!(CheckReport))
            .expect("the check schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");

        let json = run.json();
        let data = at(&json, "/data");

        let errors: Vec<String> = validator
            .iter_errors(&data)
            .map(|e| e.to_string())
            .collect();

        assert!(errors.is_empty(), "schema violations: {errors:?} in {data}");
    }
}
