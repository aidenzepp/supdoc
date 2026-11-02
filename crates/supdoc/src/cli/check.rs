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
use crate::check::reconcile::{self, check_outcome, reconcile_in};
use crate::finding::{Finding, Location};
use crate::link::destination::Untracked;
use crate::link::{LinkKey, SourcePath};
use crate::output::json::{Envelope, FindingJson};
use crate::output::summary::render_summary;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, PathStyle, Report, Summary, TextContext, Verbosity};
use crate::read::cartridge::Languages;
use crate::scan::assemble::UntrackedLinks;
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
/// Returns the config's own error (78, or 74 when unreadable), `cli-path-missing`,
/// `cli-path-outside-project` or `cli-path-invalid` for a bad `[PATH]` (2), `cli-path-unreadable` (74) for one that can't be looked up, the lockfile's own
/// error (78 when invalid, 74 when unreadable), `scan-root-unreadable` (74), and
/// `cli-check-output` when the report cannot be written (74).
pub fn run(args: &CheckArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let config = load_config(context, output)?;
    let scope = scope(&args.paths, &config, context)?;
    let lock = read_lock(&config, &context.cwd)?;
    let (scan, untracked) =
        crate::scan::assemble::run_with_untracked(&config, &Languages::builtin(), &scope)
            .map_err(CliError::from)?;

    let paths = PathStyle::new(context.cwd.clone(), config.root().clone(), None);
    let model = reconcile_in(&scan, &lock, &scope, &paths);
    let outcome = match check_outcome(&model, args.deny_warnings) {
        reconcile::Outcome::Clean => Outcome::Clean,
        reconcile::Outcome::Failing => Outcome::Failing,
    };
    let verbosity = output.verbosity();
    let report = CheckReport::new(model, verbosity);
    let report = if verbosity.lists_passed() {
        report.with_untracked(&untracked)
    } else {
        report
    };
    output
        .report(&report, outcome == Outcome::Clean)
        .map_err(|error| CliError::output_failed("cli-check-output", "the check report", &error))?;
    Ok(outcome)
}

/// The JSON Schema of `supdoc check`'s `--json` output: the whole envelope, with `data` typed as
/// [`CheckReport`].
#[must_use]
pub fn schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, CheckReport>)
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
    /// How many links were left untracked for each reason, printed as text only.
    #[serde(skip)]
    untracked: Vec<(Untracked, usize)>,
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
            untracked: Vec::new(),
        }
    }

    /// The report, also printing one line per reason that links were left untracked for.
    #[must_use]
    pub fn with_untracked(self, untracked: &UntrackedLinks) -> Self {
        Self {
            untracked: untracked.counts().collect(),
            ..self
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
        self.in_document_order()
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Link(link) => link.finding.as_ref(),
                Entry::Loose(finding) => Some(finding),
            })
            .collect()
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        for entry in self.in_document_order() {
            match entry {
                Entry::Link(link) => match &link.finding {
                    Some(finding) => render_finding(context, finding, out)?,
                    None => render_passed(context, &link.key, out)?,
                },
                Entry::Loose(finding) => render_finding(context, finding, out)?,
            }
        }
        render_untracked(&self.untracked, out)?;
        render_summary(context, &self.summary, out)
    }
}

/// One printed entry of a check: a link or a finding that belongs to no link.
enum Entry<'a> {
    /// A link, with its finding or its `passed` line.
    Link(&'a LinkReport),
    /// A finding not tied to one link.
    Loose(&'a Finding),
}

impl Entry<'_> {
    /// Where the entry sits, for ordering by file then position (S8). Entries about no one place
    /// sort first.
    fn position(&self) -> Option<(&SourcePath, usize, usize)> {
        let location = match self {
            Self::Link(link) => link.locations.first().or_else(|| {
                link.finding
                    .as_ref()
                    .and_then(|finding| finding.location.as_ref())
            }),
            Self::Loose(finding) => finding.location.as_ref(),
        };
        location.map(|location| (&location.file, location.range.start, location.range.end))
    }
}

impl CheckReport {
    /// The links shown and the loose findings as one list ordered by file then position (S8),
    /// whichever link or target each comes from. The `links` array and the lockfile keep `LinkKey`
    /// order; only what is printed follows the document.
    fn in_document_order(&self) -> Vec<Entry<'_>> {
        let mut entries: Vec<Entry<'_>> = self
            .shown
            .iter()
            .map(Entry::Link)
            .chain(self.loose.iter().map(Entry::Loose))
            .collect();
        entries.sort_by(|left, right| left.position().cmp(&right.position()));
        entries
    }
}

/// `passed  docs/guide.md -> src/lib.rs#add`, the line `--verbose` shows for a passed link.
///
/// # Errors
///
/// Fails if writing to `out` fails.
fn render_passed(context: &TextContext, key: &LinkKey, out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "passed  {} {} {}\n",
        visible(&context.paths.text(&key.source)),
        context.arrow(),
        visible(&key.target_text()),
    )
}

/// `untracked  directories: 2`, one line per reason links were left untracked for, then a blank
/// line. Prints nothing when no link was left untracked.
///
/// # Errors
///
/// Fails if writing to `out` fails.
fn render_untracked(untracked: &[(Untracked, usize)], out: &mut dyn Write) -> io::Result<()> {
    for (reason, count) in untracked {
        writeln!(out, "untracked  {}: {count}", reason.describe())?;
    }
    if untracked.is_empty() {
        Ok(())
    } else {
        writeln!(out)
    }
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

    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::cli::args::Command;
    use crate::cli::testing::{Project, Run, Session, at};

    use crate::lock::model::LOCK_FILE_NAME;

    use crate::path::AbsolutePath;

    /// A file that documents `add` in `src/lib.rs`.
    const GUIDE: &str = "# Guide\n\nThe [add](../src/lib.rs#add) function sums.\n";

    /// The code `GUIDE` points at.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

    /// A project with the guide and the code it documents.
    fn project() -> Project {
        let project = Project::empty();
        project.write("docs/guide.md", GUIDE);
        project.write("src/lib.rs", LIB);
        project
    }

    impl Project {
        /// Runs `supdoc <args>` from the project root. `args` must start with `check`.
        fn check(&self, args: &[&str]) -> Run {
            check_in(self.root(), args)
        }

        /// Runs `supdoc <args>` from the project directory `relative`, which must exist.
        fn check_from(&self, relative: &str, args: &[&str]) -> Run {
            check_in(self.root_of(relative), args)
        }
    }

    /// Runs `supdoc <args>` from `cwd`. `args` must start with `check`.
    fn check_in(cwd: AbsolutePath, args: &[&str]) -> Run {
        let mut session = Session::new(cwd, args);
        let Command::Check(check_args) = &session.cli.command else {
            panic!("`supdoc {}` should be a check command", args.join(" "));
        };
        let result = run(check_args, &session.context, &mut session.output);
        session.finished(result)
    }

    #[test]
    fn recorded_unchanged_link_passes_with_exit_0() {
        let project = project();
        project.record_all();

        let run = project.check(&["check"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "Links  1 passed\n");
    }

    #[test]
    fn unrecorded_link_is_new_and_fails_with_exit_1() {
        let project = project();

        let run = project.check(&["check"]);

        assert_eq!(run.exit_code(), 1, "stderr: {}", run.stderr);
        assert!(
            run.stdout.contains("error[link-new]"),
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
    fn failed_help_names_the_update_path_from_where_check_ran_quoted() {
        let project = project();
        project.write("supdoc.config.toml", "");
        project.write("my docs/guide.md", GUIDE);
        project.record_all();
        project.write(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        );

        let from_root = project.check(&["check"]);
        let from_subdirectory = project.check_from("my docs", &["check"]);

        assert!(
            from_root
                .stdout
                .contains("`supdoc update 'my docs/guide.md' --yes`"),
            "stdout: {}",
            from_root.stdout
        );
        assert!(
            from_subdirectory
                .stdout
                .contains("`supdoc update guide.md --yes`"),
            "stdout: {}",
            from_subdirectory.stdout
        );
    }

    #[test]
    fn changed_target_is_failed_and_fails_the_check() {
        let project = project();
        project.record_all();
        project.write(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        );

        let run = project.check(&["check"]);

        assert_eq!(run.exit_code(), 1, "stderr: {}", run.stderr);
        assert!(
            run.stdout.contains("error[link-failed]"),
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
    fn findings_print_in_document_order_not_link_key_order() {
        let project = project();
        project.write("src/a.rs", "pub fn add() {}\n");
        project.write("src/b.rs", "pub fn sub() {}\n");
        project.write(
            "docs/g.md",
            "# Notes\n\n[b](../src/b.rs#sub)\n\n[a](../src/a.rs#add)\n",
        );
        project.record_all();
        project.write("src/a.rs", "pub fn add() { 1; }\n");
        project.write("src/b.rs", "pub fn sub() { 2; }\n");

        let run = project.check(&["check"]);

        let line_of = |location: &str| {
            run.stdout
                .find(location)
                .unwrap_or_else(|| panic!("{location} should be printed: {}", run.stdout))
        };
        assert!(
            line_of("docs/g.md:3:1") < line_of("docs/g.md:5:1"),
            "the finding at line 3 should print before line 5, though `src/a.rs` sorts first: {}",
            run.stdout,
        );
        let json = project.check(&["--json", "check"]).json();
        assert_eq!(
            at(&json, "/data/links/0/target"),
            "src/a.rs",
            "the JSON `links` array keeps LinkKey order"
        );
    }

    #[test]
    fn a_located_loose_finding_prints_before_a_later_link_finding_in_the_same_file() {
        let project = project();
        project.write("src/a.rs", "pub fn add() {}\n");
        project.write(
            "docs/g.md",
            "[outside](../../../outside.rs)\n\n[a](../src/a.rs#add)\n",
        );

        let run = project.check(&["check"]);

        let line_of = |location: &str| {
            run.stdout
                .find(location)
                .unwrap_or_else(|| panic!("{location} should be printed: {}", run.stdout))
        };
        assert!(
            line_of("docs/g.md:1:") < line_of("docs/g.md:3:"),
            "the loose `link-outside-root` at line 1 should print before the `link-new` at line 3: {}",
            run.stdout,
        );
    }

    #[test]
    fn missing_target_is_broken_and_fails_the_check() {
        let project = project();
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
        let project = project();
        project.record_all();
        project.write("docs/guide.md", "# Guide\n\nNo links any more.\n");

        let lenient = project.check(&["check"]);
        let strict = project.check(&["check", "--deny-warnings"]);

        assert_eq!(lenient.exit_code(), 0, "stdout: {}", lenient.stdout);
        assert!(
            lenient.stdout.contains("warning[link-obsolete]"),
            "stdout: {}",
            lenient.stdout
        );
        assert_eq!(strict.exit_code(), 1, "stdout: {}", strict.stdout);
    }

    #[cfg(unix)]
    #[test]
    fn entries_under_an_unreadable_directory_are_not_obsolete() {
        use std::os::unix::fs::PermissionsExt as _;

        /// Gives the directory its permissions back when dropped, even if the test fails.
        struct Restore<'dir>(&'dir Path);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _restored = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
            }
        }

        let project = project();
        project.write("docs/sub/c.md", "See [add](../../src/lib.rs#add).\n");
        project.record_all();
        project.write("docs/guide.md", "# Guide\n\nNo links any more.\n");
        let sub = project.dir.path().join("docs/sub");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o000))
            .expect("the fixture directory permissions should be settable");
        let restore = Restore(&sub);

        let run = project.check(&["--json", "check"]);
        drop(restore);

        let json = run.json();
        let obsolete: Vec<String> = at(&json, "/data/links")
            .as_array()
            .expect("`data.links` should be an array")
            .iter()
            .filter(|link| link.get("state").and_then(|state| state.as_str()) == Some("obsolete"))
            .filter_map(|link| link.get("source").and_then(|source| source.as_str()))
            .map(str::to_owned)
            .collect();
        assert_eq!(
            obsolete,
            ["docs/guide.md"],
            "only the entry whose link was deleted is obsolete; the unread directory keeps its \
             entry: {json}"
        );
        assert_eq!(run.exit_code(), 0, "stdout: {}", run.stdout);
    }

    #[test]
    fn check_never_writes_any_file() {
        for recorded in [false, true] {
            let project = project();
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
        let clean = project();
        clean.record_all();
        let failing = project();
        let bad_config = project();
        bad_config.write("supdoc.config.toml", "include = 3\n");
        let conflicted_lock = project();
        conflicted_lock.write(LOCK_FILE_NAME, "<<<<<<< HEAD\n=======\n>>>>>>> theirs\n");
        let unreadable_lock = project();
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
        let project = project();

        let run = project.check(&["check", "docs/nope.md"]);

        let error = run.result.expect_err("a missing path should be an error");
        assert_eq!(error.code(), "cli-path-missing");
        assert!(
            error.message().contains("docs/nope.md"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn path_outside_the_project_is_a_usage_error() {
        let project = project();
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
        assert_eq!(error.code(), "cli-path-outside-project");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn paths_narrow_the_check_and_leave_other_entries_unjudged() {
        let project = project();
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
        let project = project();
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
        assert_eq!(at(&link, "/finding/kind"), "link-new");
        assert!(
            at(&link, "/finding").get("location").is_none(),
            "a nested finding leaves its location to the link: {link}"
        );
    }

    #[test]
    fn run_from_a_subdirectory_reports_paths_against_the_project_root() {
        let project = project();
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
        let project = project();
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
    fn untracked_links_are_counted_by_reason_only_when_verbose() {
        let project = project();
        project.write(
            "docs/more.md",
            "[home](/), [api](api.html#section), [logo](logo.png) and [web](https://example.com).\n",
        );
        project.record_all();

        let normal = project.check(&["check"]);
        let verbose = project.check(&["-v", "check"]);

        assert_eq!(verbose.exit_code(), 0, "untracked links never fail a check");
        assert!(
            !normal.stdout.contains("untracked"),
            "stdout: {}",
            normal.stdout
        );
        assert!(
            verbose.stdout.contains(
                "untracked  URLs or code paths: 1\n\
                 untracked  directories: 1\n\
                 untracked  files no language reads: 2\n"
            ),
            "stdout: {}",
            verbose.stdout
        );
    }

    #[test]
    fn repeating_verbose_changes_nothing() {
        let project = project();
        project.record_all();

        let once = project.check(&["-v", "check"]);
        let twice = project.check(&["-vv", "check"]);

        assert_eq!(
            (once.stdout, once.stderr),
            (twice.stdout, twice.stderr),
            "`-vv` should print what `-v` prints"
        );
    }

    #[test]
    fn quiet_clean_run_prints_nothing() {
        let project = project();
        project.record_all();

        let run = project.check(&["-q", "check"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!((run.stdout.as_str(), run.stderr.as_str()), ("", ""));
    }

    /// Every way `json` breaks the schema `schema()` publishes for `supdoc check`.
    fn schema_violations(json: &serde_json::Value) -> Vec<String> {
        let schema = schema();
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
        let project = project();
        let json = project.check(&["--json", "check"]).json();

        let errors = schema_violations(&json);

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
    }

    #[test]
    fn check_schema_rejects_an_envelope_without_its_schema_version() {
        let project = project();
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
