//! `supdoc update`: records the code each selected link points at in `supdoc.lock` (05).
//!
//! The run is `config::load → Lockfile::read → scan → reconcile → plan_update`, then a preview,
//! or under `--yes` one atomic write of the planned lockfile. A preview runs every step a real run
//! does, up to that write (05 C2, 06a K8).

use std::collections::BTreeMap;
use std::io::{self, Write};

use schemars::JsonSchema;
use serde::Serialize;

use crate::check::model::{LinkReport, Report as CheckModel};
use crate::check::plan::{LockChange, UpdatePlan, plan_update};
use crate::check::reconcile::reconcile;
use crate::finding::Finding;
use crate::link::LinkKey;
use crate::output::json::Envelope;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, Report, Summary, TextContext};
use crate::read::cartridge::Languages;
use crate::state::LinkState;

use super::args::{UpdateArgs, WriteMode};
use super::check::LinkLocationJson;
use super::project::{load_config, lock_path, read_lock, scope};
use super::{CliError, Context, Outcome};

/// The closing note of a preview that has changes (06a K7).
const PREVIEW_NOTE: &str = "Nothing written. Run the same command with --yes to apply.";

/// The closing note when the plan is empty (06a Q6).
const NOTHING_NOTE: &str = "Nothing to update.";

/// Runs `supdoc update`: prints every lockfile change the selected states call for, and writes
/// them under `--yes` (05 C2, C9).
///
/// Broken links are never recorded (05 C7). Each one's cause is reported as a finding pointing at
/// `supdoc fix`, and the run still exits 0 (06a U5, Q7). A missing `supdoc.lock` is an empty one,
/// and `--yes` creates it when there is something to record (05 C10).
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), `cli/path-missing`,
/// `cli/path-outside-project` or `cli/path-invalid` for a bad `[PATH]` (2), the lockfile's own
/// error (78 when invalid, 74 when unreadable or unwritable), `scan/root-unreadable` (74), and
/// `cli/update-output` when the report cannot be written (74).
pub fn run(args: &UpdateArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let config = load_config(context, output, "update")?;
    let scope = scope(&args.paths, &context.cwd, config.root())?;
    let lock = read_lock(config.root())?;
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
    let plan = plan_update(&model, &scan, &lock, &args.state);
    let applied = args.confirm.mode() == WriteMode::Apply;
    if applied && let Some(next) = &plan.next {
        next.write(&lock_path(config.root())).map_err(|error| {
            CliError::new(
                error.code(),
                error.to_string(),
                error.help(),
                error.exit_code(),
            )
        })?;
    }

    let report = UpdateReport::new(&model, &plan, &args.state, applied);
    output
        .report(&report, true)
        .map_err(|error| output_failed(&error))?;
    output
        .note(&report.closing_note(output.unicode()))
        .map_err(|error| output_failed(&error))?;
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc update`'s `--json` output: the whole envelope, with `data` typed as
/// [`UpdateReport`].
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, UpdateReport>))
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/update-output",
        format!("cannot write the update report: {error}"),
        "Check that standard output and standard error are open and writable",
        CliError::IO_EXIT_CODE,
    )
}

/// `supdoc update`: the planned or applied lockfile changes.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct UpdateReport {
    /// `false` for a preview: nothing was written.
    applied: bool,
    /// The `--state` values in effect.
    states: Vec<LinkState>,
    /// One per lockfile change, in plan order.
    changes: Vec<ChangeJson>,
    /// How many links in scope were in each state before the update.
    summary: Summary,
    /// The broken links update can't record, each as its cause finding.
    #[serde(skip)]
    findings: Vec<Finding>,
    /// The same changes for text output, which shows where each link is written.
    #[serde(skip)]
    rows: Vec<ChangeRow>,
}

impl UpdateReport {
    /// The report of `plan`, made from `model`, the reconciled links it was planned from.
    fn new(model: &CheckModel, plan: &UpdatePlan, states: &[LinkState], applied: bool) -> Self {
        let links: BTreeMap<&LinkKey, &LinkReport> =
            model.links.iter().map(|link| (&link.key, link)).collect();
        let rows: Vec<ChangeRow> = plan
            .changes
            .iter()
            .map(|change| ChangeRow::new(change, &links))
            .collect();
        Self {
            applied,
            states: states.to_vec(),
            changes: rows.iter().map(ChangeJson::from).collect(),
            summary: Summary::tally(model.links.iter().map(|link| link.state)),
            findings: model.links.iter().filter_map(broken_finding).collect(),
            rows,
        }
    }

    /// What the run says on stderr when it is done (06a K7, Q6).
    fn closing_note(&self, unicode: bool) -> String {
        if self.rows.is_empty() {
            NOTHING_NOTE.to_owned()
        } else if self.applied {
            self.applied_note(unicode)
        } else {
            PREVIEW_NOTE.to_owned()
        }
    }

    /// `supdoc.lock: 1 accepted · 1 recorded · 1 removed`, leaving out actions that didn't occur.
    /// Without `unicode` the counts are separated by `, ` so the note is plain ASCII (07 G12).
    fn applied_note(&self, unicode: bool) -> String {
        let counts = [Action::Accept, Action::Record, Action::Remove]
            .into_iter()
            .filter_map(|action| {
                let count = self.rows.iter().filter(|row| row.action == action).count();
                (count > 0).then(|| format!("{count} {}", action.past_tense()))
            })
            .collect::<Vec<_>>()
            .join(if unicode { " · " } else { ", " });
        format!("{}: {counts}", crate::lock::model::LOCK_FILE_NAME)
    }
}

impl Report for UpdateReport {
    fn command(&self) -> &'static str {
        "update"
    }

    fn summary(&self) -> Option<&Summary> {
        Some(&self.summary)
    }

    fn findings(&self) -> &[Finding] {
        &self.findings
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        render_rows(context, &self.rows, out)?;
        for finding in &self.findings {
            render_finding(context, finding, out)?;
        }
        Ok(())
    }
}

/// What a lockfile change does to its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    /// Replace a `failed` link's hash with the code's current one.
    Accept,
    /// Add an entry for a link that had none.
    Record,
    /// Drop an entry whose link is gone.
    Remove,
}

impl Action {
    /// The word a preview line starts with.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Record => "record",
            Self::Remove => "remove",
        }
    }

    /// The word the applied note counts with.
    const fn past_tense(self) -> &'static str {
        match self {
            Self::Accept => "accepted",
            Self::Record => "recorded",
            Self::Remove => "removed",
        }
    }
}

/// One lockfile change as JSON shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ChangeJson {
    /// What happens to the entry.
    action: Action,
    /// The link's state before the update.
    state: LinkState,
    /// Which entry: `source`, `target` and, for a symbol link, `symbol`.
    #[serde(flatten)]
    key: LinkKey,
    /// Every place the link appears in its source. Empty for `remove`.
    locations: Vec<LinkLocationJson>,
    /// The entry a moved link carries its history from (05 C6).
    #[serde(skip_serializing_if = "Option::is_none")]
    moved_from: Option<LinkKey>,
}

impl From<&ChangeRow> for ChangeJson {
    fn from(row: &ChangeRow) -> Self {
        Self {
            action: row.action,
            state: row.state,
            key: row.key.clone(),
            locations: row.locations.clone(),
            moved_from: row.moved_from.clone(),
        }
    }
}

/// One lockfile change with what text output needs to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangeRow {
    /// What happens to the entry.
    action: Action,
    /// The link's state before the update.
    state: LinkState,
    /// Which entry.
    key: LinkKey,
    /// Every place the link appears in its source. Empty for `remove`.
    locations: Vec<LinkLocationJson>,
    /// The entry a moved link carries its history from.
    moved_from: Option<LinkKey>,
}

impl ChangeRow {
    /// The row for `change`, looking its link up in `links`.
    ///
    /// The old key of a moved link is not a link in scope, so its removal is shown as an
    /// `obsolete` entry with no locations, like any other entry whose link is gone.
    fn new(change: &LockChange, links: &BTreeMap<&LinkKey, &LinkReport>) -> Self {
        let (action, key) = match change {
            LockChange::Accept(key) => (Action::Accept, key),
            LockChange::Record(key) => (Action::Record, key),
            LockChange::Remove(key) => (Action::Remove, key),
        };
        let link = links.get(key).filter(|_| action != Action::Remove);
        Self {
            action,
            state: link.map_or(LinkState::Obsolete, |link| link.state),
            key: key.clone(),
            locations: link
                .map(|link| link.locations.iter().map(LinkLocationJson::from).collect())
                .unwrap_or_default(),
            moved_from: link.and_then(|link| link.moved_from.clone()),
        }
    }
}

/// Writes one aligned line per change, as `accept  docs/guide.md:3  →  src/lib.rs#add  failed →
/// passed` (07 "Previews").
///
/// # Errors
///
/// Fails if writing to `out` fails.
fn render_rows(context: &TextContext, rows: &[ChangeRow], out: &mut dyn Write) -> io::Result<()> {
    let arrow = if context.unicode { "→" } else { "->" };
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            let mut written_at = context.paths.text(&row.key.source);
            if let Some(first) = row.locations.first() {
                written_at = format!("{written_at}:{}", first.line);
            }
            let mut target = row.key.target.to_string();
            if let Some(symbol) = &row.key.symbol {
                target = format!("{target}#{symbol}");
            }
            let mut result = match row.action {
                Action::Accept => format!("{} {arrow} {}", row.state, LinkState::Passed),
                Action::Record | Action::Remove => row.state.to_string(),
            };
            if let Some(old) = &row.moved_from {
                result = format!(
                    "{result}  (history from {})",
                    context.paths.text(&old.source)
                );
            }
            [
                row.action.as_str().to_owned(),
                visible(&written_at),
                visible(&target),
                visible(&result),
            ]
        })
        .collect();
    let width = |cell: fn(&[String; 4]) -> &String| {
        cells
            .iter()
            .map(|row| cell(row).chars().count())
            .max()
            .unwrap_or(0)
    };
    let location_width = width(|[_, written_at, _, _]| written_at);
    let target_width = width(|[_, _, target, _]| target);
    for [action, written_at, target, result] in &cells {
        writeln!(
            out,
            "{action}  {written_at:<location_width$}  {arrow}  {target:<target_width$}  {result}"
        )?;
    }
    Ok(())
}

/// The cause of a broken link, as a top-level finding pointing at `supdoc fix`, or `None` for a
/// link in any other state (05 C7, 06a U5).
fn broken_finding(link: &LinkReport) -> Option<Finding> {
    if link.state != LinkState::Broken {
        return None;
    }
    let mut finding = link.finding.clone()?;
    if finding.location.is_none()
        && let Some(location) = link.locations.first()
    {
        finding = finding.with_location(location.clone());
    }
    let pointer = format!(
        "update can't record a broken link; repair it with `supdoc fix {} --yes`",
        link.key.source
    );
    let help = match finding.help.take() {
        Some(own) if own.ends_with(['.', '?', '!']) => format!("{own} {}", sentence(&pointer)),
        Some(own) => format!("{own}. {}", sentence(&pointer)),
        None => pointer,
    };
    Some(finding.with_help(help))
}

/// `text` with its first letter capitalized, to follow another sentence.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
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
    use crate::output::{OutputSettings, PathStyle, Verbosity};
    use crate::path::AbsolutePath;
    use crate::scan::model::{Scope, TargetState};

    /// A file that documents `add` in `src/lib.rs`.
    const GUIDE: &str = "# Guide\n\nThe [add](../src/lib.rs#add) function sums.\n";

    /// The code `GUIDE` points at.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

    /// `LIB` with `add`'s body changed, so a recorded link to it fails.
    const LIB_CHANGED: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n";

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

    /// What one `supdoc update` run returned and printed.
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

        /// stdout parsed as JSONL, one value per line.
        fn jsonl(&self) -> Vec<serde_json::Value> {
            self.stdout
                .lines()
                .map(|line| {
                    serde_json::from_str(line).unwrap_or_else(|error| {
                        panic!("each stdout line should be JSON ({error}): {line}")
                    })
                })
                .collect()
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

        fn lock_file(&self) -> PathBuf {
            self.dir.path().join(LOCK_FILE_NAME)
        }

        /// The lockfile as it is on disk, or `None` when there is none.
        fn lock(&self) -> Option<Lockfile> {
            let path =
                AbsolutePath::new(&self.lock_file()).expect("the lockfile path should be absolute");
            Lockfile::read(&path).expect("the lockfile should be readable and valid")
        }

        /// Records every present link's current hash in `supdoc.lock`, independently of `update`.
        fn record_all(&self) {
            let config = Config::load(&self.root()).expect("the fixture config should load");
            let scan = crate::scan::assemble::run(&config, &Languages::builtin(), &Scope::All)
                .expect("the fixture project should scan");
            let mut lock = Lockfile::default();
            for (key, link) in scan.links {
                if let TargetState::Present { hash, .. } = link.target {
                    let _previous = lock.insert(key, hash);
                }
            }
            assert!(!lock.is_empty(), "the fixture should have a link to record");
            let path =
                AbsolutePath::new(&self.lock_file()).expect("the lockfile path should be absolute");
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

        /// Runs `supdoc <args>` from the project root. `args` must hold an `update` command.
        fn update(&self, args: &[&str]) -> Run {
            self.update_drawing(args, false)
        }

        /// Like [`Self::update`], drawing with Unicode when `unicode` is set and ASCII otherwise.
        fn update_drawing(&self, args: &[&str], unicode: bool) -> Run {
            let cwd = self.root();
            let command_line: Vec<&str> = std::iter::once("supdoc")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&command_line).unwrap_or_else(|error| {
                panic!("`{}` should parse: {error}", command_line.join(" "))
            });
            let Command::Update(update_args) = &cli.command else {
                panic!("`{}` should be an update command", command_line.join(" "));
            };
            let settings = OutputSettings {
                format: cli.global.format(),
                verbosity: Verbosity::from_flags(cli.global.quiet, cli.global.verbose),
                color: false,
                unicode,
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
            let result = run(update_args, &context, &mut output);
            Run {
                result,
                stdout: stdout.text(),
                stderr: stderr.text(),
            }
        }
    }

    /// A project with one link in each state `update` changes: `failed`, `new` and `obsolete`.
    fn project_with_failed_new_and_obsolete() -> Project {
        let project = Project::new();
        project.write("docs/old.md", "[add](../src/lib.rs#add)\n");
        project.record_all();
        project.write("src/lib.rs", LIB_CHANGED);
        project.write("docs/old.md", "No links any more.\n");
        project.write("docs/new.md", "[the file](../src/lib.rs)\n");
        project
    }

    /// The `(action, state, source)` of each change in `json`'s `data.changes`.
    fn change_summaries(json: &serde_json::Value) -> Vec<(String, String, String)> {
        at(json, "/data/changes")
            .as_array()
            .expect("data.changes should be an array")
            .iter()
            .map(|change| {
                let field = |name: &str| at(change, &format!("/{name}")).to_string();
                (field("action"), field("state"), field("source"))
            })
            .collect()
    }

    #[test]
    fn update_preview_json_has_applied_false_and_every_planned_change() {
        let project = project_with_failed_new_and_obsolete();
        let before = project.files();

        let run = project.update(&["--json", "update"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/command"), "update");
        assert_eq!(at(&json, "/ok"), true);
        assert_eq!(at(&json, "/data/applied"), false);
        assert_eq!(
            at(&json, "/data/states"),
            serde_json::json!(["failed", "new", "obsolete"])
        );
        assert_eq!(
            change_summaries(&json),
            [
                ("\"accept\"", "\"failed\"", "\"docs/guide.md\""),
                ("\"record\"", "\"new\"", "\"docs/new.md\""),
                ("\"remove\"", "\"obsolete\"", "\"docs/old.md\""),
            ]
            .map(|(action, state, source)| (
                action.to_owned(),
                state.to_owned(),
                source.to_owned()
            )),
        );
        assert_eq!(project.files(), before, "a preview must not write any file");
    }

    #[test]
    fn update_yes_json_has_applied_true_and_lock_matches_changes() {
        let project = project_with_failed_new_and_obsolete();

        let run = project.update(&["--json", "update", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/data/applied"), true);
        assert_eq!(
            change_summaries(&json).len(),
            3,
            "every planned change is reported: {json}"
        );
        let after = project.check_all();
        assert_eq!(
            after,
            Summary {
                passed: 2,
                ..Summary::default()
            },
            "after --yes the failed and new links pass and the obsolete entry is gone",
        );
    }

    #[test]
    fn update_text_preview_lists_one_aligned_line_per_change_and_notes_nothing_written() {
        let project = project_with_failed_new_and_obsolete();

        let run = project.update(&["update"]);

        assert_eq!(
            run.stdout,
            "accept  docs/guide.md:3  ->  src/lib.rs#add  failed -> passed\n\
             record  docs/new.md:1    ->  src/lib.rs      new\n\
             remove  docs/old.md      ->  src/lib.rs#add  obsolete\n",
        );
        assert_eq!(run.stderr, format!("{PREVIEW_NOTE}\n"));
    }

    #[test]
    fn update_text_applied_repeats_the_lines_and_counts_them_in_the_note() {
        let project = project_with_failed_new_and_obsolete();
        let preview = project.update(&["update"]);

        let applied = project.update(&["update", "--yes"]);

        assert_eq!(applied.stdout, preview.stdout);
        assert_eq!(
            applied.stderr, "supdoc.lock: 1 accepted, 1 recorded, 1 removed\n",
            "the ASCII note has no `·`"
        );
    }

    #[test]
    fn update_text_applied_note_separates_counts_with_a_dot_under_unicode() {
        let project = project_with_failed_new_and_obsolete();

        let applied = project.update_drawing(&["update", "--yes"], true);

        assert_eq!(
            applied.stderr,
            "supdoc.lock: 1 accepted · 1 recorded · 1 removed\n"
        );
    }

    #[test]
    fn dry_run_beats_yes() {
        let project = project_with_failed_new_and_obsolete();
        let before = project.files();

        let run = project.update(&["--json", "update", "--yes", "--dry-run"]);

        assert_eq!(at(&run.json(), "/data/applied"), false);
        assert_eq!(project.files(), before);
    }

    #[test]
    fn update_jsonl_is_one_change_record_per_change_then_summary_with_applied() {
        let project = project_with_failed_new_and_obsolete();

        let lines = project.update(&["--jsonl", "update", "--yes"]).jsonl();

        let types: Vec<String> = lines
            .iter()
            .map(|line| at(line, "/type").to_string())
            .collect();
        assert_eq!(
            types,
            ["\"change\"", "\"change\"", "\"change\"", "\"summary\""]
        );
        let summary = lines.last().expect("there should be a summary line");
        assert_eq!(at(summary, "/applied"), true);
        assert_eq!(at(summary, "/command"), "update");
        assert_eq!(at(summary, "/ok"), true);
        let first = lines.first().expect("there should be a change record");
        assert_eq!(at(first, "/action"), "accept");
    }

    #[test]
    fn update_with_nothing_to_do_prints_no_data_and_leaves_lock_bytes() {
        let project = Project::new();
        project.record_all();
        let before = project.files();

        let text = project.update(&["update", "--yes"]);
        let json = project.update(&["--json", "update", "--yes"]);

        assert_eq!(text.exit_code(), 0, "stderr: {}", text.stderr);
        assert_eq!(text.stdout, "");
        assert_eq!(text.stderr, format!("{NOTHING_NOTE}\n"));
        assert_eq!(at(&json.json(), "/data/changes"), serde_json::json!([]));
        assert_eq!(
            project.files(),
            before,
            "an empty plan must not rewrite supdoc.lock"
        );
    }

    #[test]
    fn first_update_yes_creates_lockfile() {
        let project = Project::new();
        assert!(
            project.lock().is_none(),
            "the fixture starts without a lockfile"
        );

        let run = project.update(&["update", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let lock = project
            .lock()
            .expect("update --yes should create supdoc.lock");
        assert_eq!(lock.len(), 1, "the one link should be recorded");
        assert_eq!(project.check_all().passed, 1);
    }

    #[test]
    fn update_reports_broken_links_as_findings_and_exits_0() {
        let project = Project::new();
        project.write(
            "docs/broken.md",
            "See [gone](../src/lib.rs#subtract) here.\n",
        );

        let run = project.update(&["--json", "update", "--yes"]);
        let text = project.update(&["update"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/ok"), true);
        let findings = at(&json, "/findings");
        assert_eq!(
            findings.as_array().map(Vec::len),
            Some(1),
            "the broken link is the one finding: {json}"
        );
        assert_eq!(at(&findings, "/0/location/file"), "docs/broken.md");
        let help = at(&findings, "/0/help").to_string();
        assert!(
            help.contains("supdoc fix docs/broken.md --yes"),
            "the help should point at fix: {help}"
        );
        assert!(
            help.contains("did you mean"),
            "the finding's own hint should stay: {help}"
        );
        assert!(
            change_summaries(&json)
                .iter()
                .all(|(_, _, source)| source != "\"docs/broken.md\""),
            "a broken link is never recorded: {json}"
        );
        assert_eq!(project.lock().map(|lock| lock.len()), Some(1));
        assert!(
            text.stdout.contains("supdoc fix docs/broken.md --yes"),
            "text shows the finding too: {}",
            text.stdout
        );
    }

    #[test]
    fn state_passed_or_broken_is_accepted_and_changes_nothing() {
        let project = project_with_failed_new_and_obsolete();
        let before = project.files();

        for state in ["passed", "broken"] {
            let run = project.update(&["--json", "update", "--state", state, "--yes"]);

            assert_eq!(run.exit_code(), 0, "--state {state}: {}", run.stderr);
            assert_eq!(at(&run.json(), "/data/changes"), serde_json::json!([]));
            assert_eq!(at(&run.json(), "/data/states"), serde_json::json!([state]));
        }
        assert_eq!(project.files(), before);
    }

    #[test]
    fn update_paths_leave_other_entries_alone() {
        let project = Project::new();
        project.write("docs/other.md", "See [add](../src/lib.rs#add).\n");
        project.record_all();
        project.write("src/lib.rs", LIB_CHANGED);
        let lock_before = project.lock().expect("the fixture lockfile should exist");

        let run = project.update(&["--json", "update", "docs/guide.md", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            change_summaries(&run.json()),
            [(
                "\"accept\"".to_owned(),
                "\"failed\"".to_owned(),
                "\"docs/guide.md\"".to_owned()
            )],
        );
        let lock_after = project.lock().expect("the lockfile should still exist");
        let other = lock_before
            .iter()
            .find(|(key, _)| key.source.as_str() == "docs/other.md")
            .expect("the other doc's link should be recorded");
        assert_eq!(
            lock_after.get(other.0),
            Some(other.1),
            "the other doc's entry keeps its old hash"
        );
        assert_eq!(project.check_all().failed, 1, "docs/other.md still fails");
    }

    #[test]
    fn update_missing_path_is_usage_error_exit_2() {
        let project = Project::new();

        let run = project.update(&["update", "docs/nope.md", "--yes"]);

        let error = run.result.as_ref().expect_err("a missing path should fail");
        assert_eq!(error.code(), "cli/path-missing");
        assert_eq!(run.exit_code(), 2);
        assert_eq!(run.stdout, "");
        assert!(
            project.lock().is_none(),
            "nothing is written on a usage error"
        );
    }

    #[test]
    fn update_moved_link_change_names_its_old_key() {
        let project = Project::new();
        project.record_all();
        fs::remove_file(project.dir.path().join("docs/guide.md"))
            .expect("the old doc should be removed");
        project.write("docs/intro.md", GUIDE);

        let run = project.update(&["--json", "update", "--yes"]);
        let text = project.update(&["update"]);

        let json = run.json();
        let changes = at(&json, "/data/changes");
        assert_eq!(at(&changes, "/0/action"), "record");
        assert_eq!(at(&changes, "/0/source"), "docs/intro.md");
        assert_eq!(
            at(&changes, "/0/moved_from"),
            serde_json::json!({ "source": "docs/guide.md", "target": "src/lib.rs", "symbol": "add" }),
        );
        assert_eq!(at(&changes, "/1/action"), "remove");
        assert_eq!(at(&changes, "/1/source"), "docs/guide.md");
        assert_eq!(
            text.stdout, "",
            "after --yes the move is recorded: {}",
            text.stdout
        );
        let lock = project.lock().expect("the lockfile should exist");
        assert!(
            lock.iter()
                .all(|(key, _)| key.source.as_str() == "docs/intro.md"),
            "only the moved link's entry is left"
        );
    }

    #[test]
    fn update_moved_link_text_names_where_its_history_came_from() {
        let project = Project::new();
        project.record_all();
        fs::remove_file(project.dir.path().join("docs/guide.md"))
            .expect("the old doc should be removed");
        project.write("docs/intro.md", GUIDE);

        let run = project.update(&["update"]);

        assert!(
            run.stdout.contains("(history from docs/guide.md)"),
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn exit_codes_match_table() {
        let bad_config = Project::new();
        bad_config.write("supdoc.config.toml", "include = 3\n");
        let conflicted_lock = Project::new();
        conflicted_lock.write(LOCK_FILE_NAME, "<<<<<<< HEAD\n=======\n>>>>>>> theirs\n");
        let unreadable_lock = Project::new();
        fs::create_dir(unreadable_lock.lock_file())
            .expect("a directory in place of the lockfile should be created");

        let cases = [
            ("invalid config", bad_config.update(&["update"]), 78),
            ("lockfile conflict", conflicted_lock.update(&["update"]), 78),
            (
                "unreadable lockfile",
                unreadable_lock.update(&["update"]),
                74,
            ),
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

    /// Every way `json` breaks the schema `schema()` publishes for `supdoc update`.
    fn schema_violations(json: &serde_json::Value) -> Vec<String> {
        let schema = schema().expect("update is built, so it has a schema");
        let schema = serde_json::to_value(schema).expect("the update schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        validator
            .iter_errors(json)
            .map(|error| error.to_string())
            .collect()
    }

    #[test]
    fn update_json_validates_against_its_schema() {
        let project = project_with_failed_new_and_obsolete();
        project.write("docs/broken.md", "[gone](../src/lib.rs#subtract)\n");
        let json = project.update(&["--json", "update"]).json();

        let errors = schema_violations(&json);

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
    }

    impl Project {
        /// The state counts `supdoc check` would report for the whole project now.
        fn check_all(&self) -> Summary {
            let config = Config::load(&self.root()).expect("the fixture config should load");
            let scan = crate::scan::assemble::run(&config, &Languages::builtin(), &Scope::All)
                .expect("the fixture project should scan");
            let lock = self.lock().unwrap_or_default();
            let model = reconcile(&scan, &lock, &Scope::All);
            Summary::tally(model.links.iter().map(|link| link.state))
        }
    }
}
