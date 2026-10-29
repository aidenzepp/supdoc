//! `supdoc check`: compares every link in scope with `supdoc.lock` and reports which fail (05).
//!
//! Read-only: `check` never writes a file (05 C2). The run is `config::load → Lockfile::read →
//! scan → reconcile → check_outcome → render`, with effects only at the two ends.
//!
//! This module also owns the wire form of one link, [`LinkJson`], which `list` shares.

use std::io::{self, Write};

use schemars::JsonSchema;
use serde::Serialize;

use crate::check::model::{LinkReport, Report as CheckModel};
use crate::check::reconcile::{self, check_outcome, reconcile};
use crate::finding::{Finding, Location};
use crate::link::LinkKey;
use crate::output::json::{Envelope, FindingJson};
use crate::output::summary::render_summary;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, Report, Summary, TextContext, Verbosity};
use crate::read::cartridge::Languages;
use crate::state::LinkState;

use super::args::CheckArgs;
use super::project::{load_config, read_lock, scope};
use super::{CliError, Context, Outcome};

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
    let config = load_config(context, output, "check")?;
    let scope = scope(&args.paths, &context.cwd, config.root())?;
    let lock = read_lock(&config)?;
    let scan =
        crate::scan::assemble::run(&config, &Languages::builtin(), &scope).map_err(|error| {
            CliError::new(
                error.code(),
                error.to_string(),
                error.help(),
                error.exit_code(),
            )
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

/// The JSON Schema of `supdoc check`'s `--json` output: the whole envelope, with `data` typed as
/// [`CheckReport`].
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, CheckReport>))
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/check-output",
        format!("cannot write the check report: {error}"),
        "Check that standard output and standard error are open and writable",
        CliError::IO_EXIT_CODE,
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
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::{Cli, Command};
    use crate::config::Config;
    use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
    use crate::output::{OutputSettings, PathStyle};
    use crate::path::AbsolutePath;
    use crate::scan::model::{Scope, TargetState};

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
            Self::check_in(self.root(), args)
        }

        /// Runs `supdoc <args>` from the project directory `relative`, which must exist.
        fn check_from(&self, relative: &str, args: &[&str]) -> Run {
            let cwd = AbsolutePath::new(&self.dir.path().join(relative))
                .expect("a directory under the root should be absolute");
            Self::check_in(cwd, args)
        }

        /// Runs `supdoc <args>` from `cwd`, with output paths provisionally rooted there as
        /// `main` roots them before any config loads.
        fn check_in(cwd: AbsolutePath, args: &[&str]) -> Run {
            let command_line: Vec<&str> = std::iter::once("supdoc")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&command_line).unwrap_or_else(|error| {
                panic!("`{}` should parse: {error}", command_line.join(" "))
            });
            let Command::Check(check_args) = &cli.command else {
                panic!("`{}` should be a check command", command_line.join(" "));
            };
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
                home: None,
                claude_config_dir: None,
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
    fn run_from_a_subdirectory_reports_paths_against_the_project_root() {
        let project = Project::new();
        project.write("supdoc.config.toml", "");

        let json = project.check_from("docs", &["--json", "check"]).json();
        let text = project.check_from("docs", &["check"]);

        assert_eq!(
            at(&json, "/root"),
            project.root().as_path().to_string_lossy().as_ref(),
            "the envelope root is the project root, not the current directory: {json}"
        );
        assert_eq!(
            at(&json, "/data/links/0/source"),
            "docs/guide.md",
            "JSON paths stay relative to the project root: {json}"
        );
        assert!(
            text.stdout.contains("guide.md:3:5") && !text.stdout.contains("docs/guide.md"),
            "text paths are relative to the current directory: {}",
            text.stdout
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

    /// Every way `json` breaks the schema `schema()` publishes for `supdoc check`.
    fn schema_violations(json: &serde_json::Value) -> Vec<String> {
        let schema = schema().expect("check is built, so it has a schema");
        let schema = serde_json::to_value(schema).expect("the check schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        validator
            .iter_errors(json)
            .map(|error| error.to_string())
            .collect()
    }

    #[test]
    fn check_json_validates_against_its_schema() {
        let project = Project::new();
        let json = project.check(&["--json", "check"]).json();

        let errors = schema_violations(&json);

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
    }

    #[test]
    fn check_schema_rejects_an_envelope_without_its_schema_version() {
        let project = Project::new();
        let mut json = project.check(&["--json", "check"]).json();
        let _version = json
            .as_object_mut()
            .expect("the envelope should be a JSON object")
            .remove("schema_version")
            .expect("the envelope should carry schema_version");

        let errors = schema_violations(&json);

        assert!(
            !errors.is_empty(),
            "an envelope without schema_version should fail the schema: {json}"
        );
    }
}
