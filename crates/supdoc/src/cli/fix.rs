//! `supdoc fix`: rewrites broken links whose repair is certain (05 C8).
//!
//! The run is `config → lockfile → scan → plan_fixes`, then either the preview, or `apply_fixes →
//! move lock entries → write the lockfile if an entry moved → scan the fixed files again →
//! reconcile`, so each fixed link reports the state it has now. A preview runs every step a real
//! run does up to the first write (K8).

use std::collections::BTreeSet;
use std::io::{self, Write};

use schemars::JsonSchema;
use serde::Serialize;

use crate::check::fix::{
    FixPlan, FixResult, PlannedFix, apply_fixes, move_lock_entries, plan_fixes,
};
use crate::check::model::Report as CheckModel;
use crate::check::reconcile::reconcile;
use crate::config::Config;
use crate::finding::{Finding, Fix};
use crate::link::{LinkKey, SourcePath};
use crate::lock::error::LockError;
use crate::lock::model::Lockfile;
use crate::output::json::Envelope;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, PathStyle, Report, TextContext};
use crate::read::cartridge::Languages;
use crate::scan::model::{Scan, Scope, TargetState};
use crate::state::LinkState;

use super::args::{FixArgs, WriteMode};
use super::project::{load_config, lock_path, read_lock, scope};
use super::{CliError, Context, Outcome};

/// The code of the finding for a source file `apply_fixes` could not write.
const CODE_WRITE_FAILED: &str = "fix/write-failed";

/// The code of the fatal error for sources rewritten while the lockfile could not be written.
const CODE_LOCK_WRITE_FAILED: &str = "fix/lock-write-failed";

/// The code of the finding for a source file that changed between the scan and the write.
const CODE_FILE_CHANGED: &str = "fix/file-changed";

/// The note that closes a preview, after the count of fixes.
const PREVIEW_NOTE: &str = "Nothing written. Run the same command with --yes to apply.";

/// Runs `supdoc fix`: previews every certain fix, or with `--yes` applies them and reports the
/// state each fixed link has now.
///
/// A file that changed since the scan is skipped and the run exits 1. A file that can't be
/// written is skipped and the run fails with exit 74 once the report is written. Either way the
/// other files are still fixed.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), `cli/path-missing`,
/// `cli/path-outside-project` or `cli/path-invalid` for a bad `[PATH]` (2), the lockfile's own
/// error (78 when invalid, 74 when it can't be read or written), the scan's own error (74),
/// `fix/write-failed` when a source file could not be written (74), and `cli/fix-output` when the
/// report cannot be written (74).
pub fn run(args: &FixArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    Planned::scan(&args.paths, context, output)?.finish(args.confirm.mode(), output)
}

/// The JSON Schema of `supdoc fix`'s `--json` output: the whole envelope, with `data` typed as
/// [`FixReport`].
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, FixReport>))
}

/// A scanned project and the fixes it offers: everything a run does before its first write.
#[derive(Debug)]
struct Planned {
    /// The loaded config, whose root every source path is relative to.
    config: Config,
    /// The lockfile as read, empty when there is none.
    lock: Lockfile,
    /// The fixes to preview or apply.
    plan: FixPlan,
    /// Broken links in scope, fixable or not, for the note when nothing can be fixed.
    broken: usize,
}

impl Planned {
    /// Loads the config and the lockfile, scans `paths` and plans every certain fix.
    ///
    /// # Errors
    ///
    /// Returns the config's, the lockfile's or the scan's own error, or a `[PATH]` usage error.
    fn scan(
        paths: &[std::path::PathBuf],
        context: &Context,
        output: &mut Output,
    ) -> Result<Self, CliError> {
        let config = load_config(context, output, "fix")?;
        let scope = scope(paths, &context.cwd, config.root())?;
        let lock = read_lock(&config)?;
        let scan = scan(&config, &scope)?;
        let broken = scan
            .links
            .iter()
            .filter(|(key, link)| {
                scope.contains(&key.source) && matches!(link.target, TargetState::Broken { .. })
            })
            .count();
        Ok(Self {
            plan: plan_fixes(&scan),
            config,
            lock,
            broken,
        })
    }

    /// Previews the plan, or applies it and reports what each fixed link is now.
    ///
    /// # Errors
    ///
    /// Returns `fix/write-failed` (74) after the report when a source file could not be written,
    /// the lockfile's or the re-scan's own error, and `cli/fix-output` when the report cannot be
    /// written.
    fn finish(self, mode: WriteMode, output: &mut Output) -> Result<Outcome, CliError> {
        match mode {
            WriteMode::Preview => self.preview(output),
            WriteMode::Apply => self.apply(output),
        }
    }

    /// Reports every planned fix and writes nothing.
    ///
    /// # Errors
    ///
    /// Returns `cli/fix-output` when the report cannot be written.
    fn preview(self, output: &mut Output) -> Result<Outcome, CliError> {
        let fixes: Vec<FixRecord> = self
            .plan
            .fixes
            .iter()
            .map(|planned| {
                let moved = planned.moved.clone().filter(|(old, new)| {
                    self.lock.get(old).is_some() && self.lock.get(new).is_none()
                });
                FixRecord::new(planned, moved, None)
            })
            .collect();
        let report = FixReport {
            applied: false,
            fixes,
            findings: Vec::new(),
        };
        output
            .report(&report, true)
            .map_err(|error| output_failed(&error))?;
        let note = if report.fixes.is_empty() {
            nothing_to_fix(self.broken)
        } else {
            let fix_count = count(report.fixes.len(), "fix", "fixes");
            let file_count = count(report.files(), "file", "files");
            format!("{fix_count} in {file_count}. {PREVIEW_NOTE}")
        };
        output.note(&note).map_err(|error| output_failed(&error))?;
        Ok(Outcome::Clean)
    }

    /// Writes every fix whose file still matches the scan, carries the lock entries of rewritten
    /// links, and reports the state each fixed link has after a fresh scan.
    ///
    /// # Errors
    ///
    /// Returns the re-scan's own error, `cli/fix-output` when the report cannot be written, and
    /// after the report `fix/lock-write-failed` (74) when the lockfile could not be written
    /// (the fixed files are named) or `fix/write-failed` (74) when a source file could not be.
    fn apply(self, output: &mut Output) -> Result<Outcome, CliError> {
        let root = self.config.root();
        let FixResult { applied, skipped } = apply_fixes(&self.plan, root);

        let mut lock = self.lock.clone();
        let mut moved = move_lock_entries(&mut lock, &applied);
        let mut lock_failure = None;
        if lock != self.lock
            && let Err(error) = lock.write(&lock_path(root))
        {
            // The write is atomic, so the lockfile on disk is still the one that was read.
            lock = self.lock.clone();
            moved.clear();
            lock_failure = Some(error);
        }

        let done: Vec<&PlannedFix> = self
            .plan
            .fixes
            .iter()
            .filter(|planned| applied.iter().any(|applied| applied.fix == planned.fix))
            .collect();
        let states = if done.is_empty() {
            CheckModel::default()
        } else {
            let fixed: BTreeSet<SourcePath> = done
                .iter()
                .map(|planned| planned.fix.file.clone())
                .collect();
            let scope = Scope::Paths(fixed.into_iter().collect());
            reconcile(&scan(&self.config, &scope)?, &lock, &scope)
        };
        let fixes: Vec<FixRecord> = done
            .iter()
            .zip(new_ranges(&done))
            .map(|(planned, range)| {
                let moved = planned.moved.clone().filter(|pair| moved.contains(pair));
                FixRecord::new(planned, moved, state_at(&states, &planned.fix.file, range))
            })
            .collect();

        let write_failed = skipped
            .iter()
            .filter(|finding| finding.code == CODE_WRITE_FAILED)
            .count();
        let file_changed = skipped
            .iter()
            .any(|finding| finding.code == CODE_FILE_CHANGED);
        let report = FixReport {
            applied: true,
            fixes,
            findings: skipped,
        };
        let ok = write_failed == 0 && !file_changed && lock_failure.is_none();
        output
            .report(&report, ok)
            .map_err(|error| output_failed(&error))?;
        let note = if !report.fixes.is_empty() {
            let link_count = count(report.fixes.len(), "link", "links");
            let file_count = count(report.files(), "file", "files");
            Some(format!("Fixed {link_count} in {file_count}."))
        } else if self.plan.fixes.is_empty() {
            Some(nothing_to_fix(self.broken))
        } else {
            None
        };
        if let Some(note) = note {
            output.note(&note).map_err(|error| output_failed(&error))?;
        }

        if let Some(error) = lock_failure {
            let changed: BTreeSet<&SourcePath> =
                done.iter().map(|planned| &planned.fix.file).collect();
            Err(lock_unwritten(&error, &changed))
        } else if write_failed > 0 {
            Err(sources_unwritten(write_failed))
        } else if file_changed {
            Ok(Outcome::Failing)
        } else {
            Ok(Outcome::Clean)
        }
    }
}

/// Scans `scope` with the built-in languages.
///
/// # Errors
///
/// Returns the scan's own error, such as an unreadable root (74).
fn scan(config: &Config, scope: &Scope) -> Result<Scan, CliError> {
    crate::scan::assemble::run(config, &Languages::builtin(), scope).map_err(|error| {
        CliError::new(
            error.code(),
            error.to_string(),
            error.help(),
            error.exit_code(),
        )
    })
}

/// Where each fix's replacement sits in its file once every fix before it in that file is
/// applied. `done` is in plan order: by file, then by position, never overlapping.
fn new_ranges(done: &[&PlannedFix]) -> Vec<Option<std::ops::Range<usize>>> {
    let mut ranges = Vec::with_capacity(done.len());
    let mut file: Option<&SourcePath> = None;
    let (mut grown, mut shrunk) = (0_usize, 0_usize);
    for planned in done {
        let Fix {
            file: this,
            range,
            replacement,
        } = &planned.fix;
        if file != Some(this) {
            file = Some(this);
            (grown, shrunk) = (0, 0);
        }
        let start = (range.start + grown).checked_sub(shrunk);
        ranges.push(start.map(|start| start..start + replacement.len()));
        grown += replacement.len();
        shrunk += range.len();
    }
    ranges
}

/// The state of the link in `file` whose occurrence covers `range`, the bytes a fix wrote.
fn state_at(
    states: &CheckModel,
    file: &SourcePath,
    range: Option<std::ops::Range<usize>>,
) -> Option<LinkState> {
    let range = range?;
    states
        .links
        .iter()
        .find(|link| {
            link.key.source == *file
                && link.locations.iter().any(|location| {
                    location.file == *file
                        && location.range.start <= range.start
                        && range.end <= location.range.end
                })
        })
        .map(|link| link.state)
}

/// `1 fix` or `2 fixes`.
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The note for a run with nothing to fix, naming the broken links left as they are (Q8).
fn nothing_to_fix(broken: usize) -> String {
    match broken {
        0 => "Nothing to fix.".to_owned(),
        1 => "Nothing to fix. 1 broken link has no certain fix; run supdoc check to see it."
            .to_owned(),
        n => format!(
            "Nothing to fix. {n} broken links have no certain fix; run supdoc check to see them."
        ),
    }
}

/// The error that ends a run in which `files` source files could not be written. The report
/// already carries a `fix/write-failed` finding for each one.
fn sources_unwritten(files: usize) -> CliError {
    let message = if files == 1 {
        "1 file could not be written; the other fixes were applied".to_owned()
    } else {
        format!("{files} files could not be written; the other fixes were applied")
    };
    CliError::new(
        CODE_WRITE_FAILED,
        message,
        "Make the files writable and run `supdoc fix --yes` again",
        CliError::IO_EXIT_CODE,
    )
}

/// The error for source files that were rewritten while the lockfile could not be written.
///
/// The lockfile is untouched, so it still holds each fixed link's history under its old key.
/// Rerunning `fix` finds nothing left to fix, so the help offers the two ways forward that work.
fn lock_unwritten(error: &LockError, files: &BTreeSet<&SourcePath>) -> CliError {
    let names: Vec<&str> = files.iter().map(|file| file.as_str()).collect();
    CliError::new(
        CODE_LOCK_WRITE_FAILED,
        format!(
            "fixed {}, but supdoc.lock could not be written: {error}",
            names.join(", ")
        ),
        "supdoc.lock is unchanged and still holds the old links' history. \
         Make it writable, then either restore those files and run `supdoc fix --yes` again \
         to carry the history over, or run `supdoc update --yes` on them to lock the fixed \
         links afresh without it",
        CliError::IO_EXIT_CODE,
    )
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/fix-output",
        format!("cannot write the fix report: {error}"),
        "Check that standard output and standard error are open and writable, or redirect both \
         to a file with `> supdoc.out 2>&1`",
        CliError::IO_EXIT_CODE,
    )
}

/// `supdoc fix`: the planned or applied edits, and the files left alone.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct FixReport {
    /// `false` for a preview, `true` once the fixes were written (K9).
    applied: bool,
    /// One per planned fix in a preview; one per written fix after `--yes`. Sorted by file, then
    /// position.
    fixes: Vec<FixRecord>,
    /// Files `--yes` left alone: `fix/file-changed` and `fix/write-failed`.
    #[serde(skip)]
    findings: Vec<Finding>,
}

impl FixReport {
    /// How many files the fixes are in.
    fn files(&self) -> usize {
        self.fixes
            .iter()
            .map(|record| &record.source)
            .collect::<BTreeSet<_>>()
            .len()
    }
}

impl Report for FixReport {
    fn command(&self) -> &'static str {
        "fix"
    }

    fn findings(&self) -> &[Finding] {
        &self.findings
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        for record in &self.fixes {
            record.render(context, out)?;
        }
        for finding in &self.findings {
            render_finding(context, finding, out)?;
        }
        Ok(())
    }
}

/// One edit: where it is, the bytes it replaces and writes, and the finding it repairs.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct FixRecord {
    /// The source file, relative to the project root.
    file: String,
    /// 1-based line of the link.
    line: u32,
    /// 1-based column of the link, in characters.
    column: u32,
    /// Start byte offset of the replaced bytes.
    start: usize,
    /// End byte offset of the replaced bytes.
    end: usize,
    /// The bytes replaced, as scanned.
    before: String,
    /// The bytes written instead.
    after: String,
    /// The code of the finding repaired, such as `resolve/not-found` or `scan/case-mismatch`.
    cause: String,
    /// The lock entry this fix carries to the link's new key.
    #[serde(skip_serializing_if = "Option::is_none")]
    moved: Option<MovedJson>,
    /// The link's state after the fix, from a fresh scan. Only after `--yes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<LinkState>,
    /// The source file, for text paths relative to the current directory.
    #[serde(skip)]
    source: SourcePath,
    /// The link as scanned, for text.
    #[serde(skip)]
    old_link: String,
    /// The link with the fix applied, for text.
    #[serde(skip)]
    new_link: String,
}

impl FixRecord {
    /// The record of `planned`, with the lock entry that moved (or, in a preview, would move) and
    /// the state its link has after the fix when known.
    fn new(
        planned: &PlannedFix,
        moved: Option<(LinkKey, LinkKey)>,
        state: Option<LinkState>,
    ) -> Self {
        let Fix {
            file,
            range,
            replacement,
        } = &planned.fix;
        let finding = &planned.finding;
        // A planned fix lies inside its finding's location and snippet, so these slices exist.
        let (line, column, link_range) = finding
            .location
            .as_ref()
            .map_or((0, 0, range.clone()), |location| {
                (location.line, location.column, location.range.clone())
            });
        let scanned = |wanted: &std::ops::Range<usize>| -> Option<String> {
            let snippet = finding.snippet.as_ref()?;
            let origin = link_range.start.checked_sub(snippet.highlight.start)?;
            let from = wanted.start.checked_sub(origin)?;
            let to = wanted.end.checked_sub(origin)?;
            snippet.text.get(from..to).map(str::to_owned)
        };
        let before = scanned(range).unwrap_or_default();
        let old_link = scanned(&link_range).unwrap_or_else(|| before.clone());
        let new_link = range
            .start
            .checked_sub(link_range.start)
            .zip(range.end.checked_sub(link_range.start))
            .and_then(|(from, to)| {
                Some(format!(
                    "{}{replacement}{}",
                    old_link.get(..from)?,
                    old_link.get(to..)?
                ))
            })
            .unwrap_or_else(|| replacement.clone());
        Self {
            file: PathStyle::json(file),
            line,
            column,
            start: range.start,
            end: range.end,
            before,
            after: replacement.clone(),
            cause: finding.code.to_owned(),
            moved: moved.map(|(from, to)| MovedJson { from, to }),
            state,
            source: file.clone(),
            old_link,
            new_link,
        }
    }

    /// `docs/api.md:40:3`, the link before and after, and after `--yes` its state now.
    ///
    /// # Errors
    ///
    /// Fails if writing to `out` fails.
    fn render(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let path = visible(&context.paths.text(&self.source));
        writeln!(out, "{path}:{}:{}", self.line, self.column)?;
        writeln!(out, "  - {}", visible(&self.old_link))?;
        writeln!(out, "  + {}", visible(&self.new_link))?;
        if let Some(state) = self.state {
            writeln!(out, "  = now {state}")?;
            if state == LinkState::Failed {
                writeln!(
                    out,
                    "  = help: reread it, then run supdoc update {path} --yes"
                )?;
            }
        }
        writeln!(out)
    }
}

/// The lock entry a fix carries from a link's old key to its new one (05 C6).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct MovedJson {
    /// The key before the fix.
    from: LinkKey,
    /// The key after it.
    to: LinkKey,
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
    use crate::lock::model::LOCK_FILE_NAME;
    use crate::output::{OutputSettings, Verbosity};
    use crate::path::AbsolutePath;

    /// A guide whose link misspells `add` as `ad`, which has exactly one near match.
    const GUIDE: &str = "# Guide\n\nThe [add](../src/lib.rs#ad) function sums.\n";

    /// The guide once `fix --yes` has repaired it.
    const FIXED_GUIDE: &str = "# Guide\n\nThe [add](../src/lib.rs#add) function sums.\n";

    /// The code the guide points at.
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

    /// What one `supdoc` run returned and printed.
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

    /// One parsed command line with the output and context it runs with.
    struct Session {
        cli: Cli,
        context: Context,
        output: Output,
        stdout: Capture,
        stderr: Capture,
    }

    impl Session {
        /// The arguments of the `fix` command this session parsed.
        fn fix_args(&self) -> &FixArgs {
            let Command::Fix(args) = &self.cli.command else {
                panic!("the session should hold a fix command");
            };
            args
        }

        /// What the run printed, with `result`.
        fn finished(self, result: Result<Outcome, CliError>) -> Run {
            Run {
                result,
                stdout: self.stdout.text(),
                stderr: self.stderr.text(),
            }
        }
    }

    /// A project in `<tempdir>/project`, so links can also point outside its root.
    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        /// A project with a config, the guide and the code it documents.
        fn new() -> Self {
            let project = Self::empty();
            project.write("docs/guide.md", GUIDE);
            project.write("src/lib.rs", LIB);
            project
        }

        /// A project with only an empty config, which makes its directory the root.
        fn empty() -> Self {
            let project = Self {
                dir: tempfile::tempdir().expect("a fresh temp directory should be created"),
            };
            project.write("supdoc.config.toml", "");
            project
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.path().join("project").join(relative)
        }

        fn root(&self) -> AbsolutePath {
            AbsolutePath::new(&self.path("")).expect("a temp directory should be absolute")
        }

        /// Writes `text` at `relative`, which may climb out of the root with `..`.
        fn write(&self, relative: &str, text: &str) {
            let path = self.path(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be created");
            }
            fs::write(&path, text).expect("the fixture file should be written");
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.path(relative)).expect("the fixture file should be readable")
        }

        fn scan(&self) -> Scan {
            let config = Config::load(&self.root()).expect("the fixture config should load");
            crate::scan::assemble::run(&config, &Languages::builtin(), &Scope::All)
                .expect("the fixture project should scan")
        }

        /// The current hash of every present link.
        fn hashes(&self) -> Lockfile {
            self.scan()
                .links
                .into_iter()
                .filter_map(|(key, link)| match link.target {
                    TargetState::Present { hash, .. } => Some((key, hash)),
                    TargetState::Broken { .. } => None,
                })
                .collect()
        }

        /// The key of the one link the project has.
        fn only_key(&self) -> LinkKey {
            let scan = self.scan();
            let mut keys = scan.links.into_keys();
            let key = keys.next().expect("the fixture should have a link");
            assert_eq!(keys.next(), None, "the fixture should have one link");
            key
        }

        fn write_lock(&self, lock: &Lockfile) {
            let path = AbsolutePath::new(&self.path(LOCK_FILE_NAME))
                .expect("the lockfile path should be absolute");
            lock.write(&path)
                .expect("the fixture lockfile should be written");
        }

        fn lock(&self) -> Lockfile {
            let path = AbsolutePath::new(&self.path(LOCK_FILE_NAME))
                .expect("the lockfile path should be absolute");
            Lockfile::read(&path)
                .expect("the lockfile should be valid")
                .expect("the lockfile should exist")
        }

        /// Records the hash `GUIDE`'s link will have once fixed under its broken key, as if the
        /// symbol was renamed after `update --yes`.
        fn record_before_rename(&self) {
            self.write("docs/guide.md", FIXED_GUIDE);
            let hashes = self.hashes();
            self.write("docs/guide.md", GUIDE);
            let old = self.only_key();
            let hash = hashes
                .iter()
                .map(|(_, hash)| hash)
                .next()
                .expect("the fixed guide's link should be present");
            self.write_lock(&[(old, hash)].into_iter().collect());
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

        /// Parses `supdoc <args>` to run from the project root.
        fn session(&self, args: &[&str]) -> Session {
            let command_line: Vec<&str> = std::iter::once("supdoc")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&command_line).unwrap_or_else(|error| {
                panic!("`{}` should parse: {error}", command_line.join(" "))
            });
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
            let output = Output::new(settings, Box::new(stdout.clone()), Box::new(stderr.clone()));
            let context = Context {
                global: cli.global.clone(),
                cwd,
                home: None,
                claude_config_dir: None,
            };
            Session {
                cli,
                context,
                output,
                stdout,
                stderr,
            }
        }

        /// Runs `supdoc <args>` from the project root, through the real dispatch.
        fn run(&self, args: &[&str]) -> Run {
            let mut session = self.session(args);
            let result =
                crate::cli::run(&session.cli.command, &session.context, &mut session.output);
            session.finished(result)
        }
    }

    #[test]
    fn fix_preview_json_lists_each_fix_with_byte_range_and_applied_false() {
        let project = Project::new();
        project.record_before_rename();
        let before = project.files();

        let run = project.run(&["--json", "fix"]);

        assert_eq!(run.exit_code(), 0, "a preview exits 0: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/ok"), true);
        assert_eq!(at(&json, "/data/applied"), false);
        let start = GUIDE.find("#ad)").expect("the guide should hold the link") + 1;
        assert_eq!(
            at(&json, "/data/fixes"),
            serde_json::json!([{
                "file": "docs/guide.md",
                "line": 3,
                "column": 5,
                "start": start,
                "end": start + 2,
                "before": "ad",
                "after": "add",
                "cause": "resolve/not-found",
                "moved": {
                    "from": {"source": "docs/guide.md", "target": "src/lib.rs", "symbol": "ad"},
                    "to": {"source": "docs/guide.md", "target": "src/lib.rs", "symbol": "add"},
                },
            }]),
        );
        assert_eq!(project.files(), before, "a preview writes nothing");
    }

    #[test]
    fn fix_json_without_a_lockfile_reports_no_move() {
        let project = Project::new();

        for args in [&["--json", "fix"][..], &["--json", "fix", "--yes"][..]] {
            let run = project.run(args);

            assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
            assert_eq!(
                run.json().pointer("/data/fixes/0/moved"),
                None,
                "`supdoc {}` has no lock entry to move, so it should report none",
                args.join(" ")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn fix_yes_with_unwritable_lockfile_names_the_fixed_files_and_keeps_the_lock() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = Project::new();
        project.record_before_rename();
        let lock_before =
            fs::read(project.path(LOCK_FILE_NAME)).expect("the lockfile should exist");
        let root = project.path("");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555))
            .expect("the fixture root should become read-only");

        let run = project.run(&["--json", "fix", "--yes"]);

        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))
            .expect("the fixture root should become writable again");
        assert_eq!(run.exit_code(), 74, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/data/fixes/0/file"), "docs/guide.md");
        assert_eq!(
            json.pointer("/data/fixes/0/moved"),
            None,
            "no entry moved, so the report must not claim one did"
        );
        let error = run
            .result
            .as_ref()
            .expect_err("a lockfile failure is fatal");
        assert_eq!(error.code(), "fix/lock-write-failed");
        assert!(
            error.message().contains("docs/guide.md"),
            "the error should name the fixed file: {}",
            error.message()
        );
        assert!(
            error.help().contains("supdoc update --yes") && error.help().contains("history"),
            "the help should say how to recover the history: {}",
            error.help()
        );
        assert_eq!(project.read("docs/guide.md"), FIXED_GUIDE);
        assert_eq!(
            fs::read(project.path(LOCK_FILE_NAME)).expect("the lockfile should still exist"),
            lock_before,
            "the lockfile is left as it was"
        );
    }

    #[test]
    fn dry_run_beats_yes() {
        let project = Project::new();
        let before = project.files();

        let run = project.run(&["--json", "fix", "--yes", "--dry-run"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!(at(&run.json(), "/data/applied"), false);
        assert_eq!(project.files(), before, "--dry-run writes nothing");
    }

    #[test]
    fn fix_yes_json_reports_new_state_of_each_fixed_link() {
        let project = Project::new();
        project.record_before_rename();
        let old = project.only_key();

        let run = project.run(&["--json", "fix", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/ok"), true);
        assert_eq!(at(&json, "/data/applied"), true);
        assert_eq!(at(&json, "/data/fixes/0/state"), "passed");
        assert_eq!(at(&json, "/data/fixes/0/moved/from/symbol"), "ad");
        assert_eq!(at(&json, "/data/fixes/0/moved/to/symbol"), "add");
        assert_eq!(project.read("docs/guide.md"), FIXED_GUIDE);
        let lock = project.lock();
        assert_eq!(lock.get(&old), None, "the old key's entry should move");
        assert!(
            lock.get(&project.only_key()).is_some(),
            "the new key keeps the entry"
        );
    }

    #[test]
    fn fixed_link_without_a_lock_entry_reports_new() {
        let project = Project::new();

        let run = project.run(&["--json", "fix", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(at(&run.json(), "/data/fixes/0/state"), "new");
    }

    #[test]
    fn fixed_link_with_changed_body_reports_failed_and_says_to_reread() {
        let project = Project::new();
        project.record_before_rename();
        project.write(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        );

        let run = project.run(&["fix", "--yes"]);

        assert_eq!(
            run.exit_code(),
            0,
            "fixing a link succeeds even if it then fails check"
        );
        assert_eq!(
            run.stdout,
            "docs/guide.md:3:5\n  \
             - [add](../src/lib.rs#ad)\n  \
             + [add](../src/lib.rs#add)\n  \
             = now failed\n  \
             = help: reread it, then run supdoc update docs/guide.md --yes\n\n",
        );
        assert_eq!(run.stderr, "Fixed 1 link in 1 file.\n");
    }

    #[test]
    fn fix_preview_text_shows_the_link_before_and_after_then_the_note_on_stderr() {
        let project = Project::new();

        let run = project.run(&["fix"]);

        assert_eq!(
            run.stdout,
            "docs/guide.md:3:5\n  - [add](../src/lib.rs#ad)\n  + [add](../src/lib.rs#add)\n\n",
        );
        assert_eq!(
            run.stderr,
            "1 fix in 1 file. Nothing written. Run the same command with --yes to apply.\n",
        );
    }

    #[test]
    fn nothing_to_fix_prints_no_data_and_counts_broken_links_left() {
        let project = Project::new();
        project.write("docs/guide.md", "See [add](../src/lib.rs#zzzzzz).\n");

        let run = project.run(&["fix", "--yes"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!(run.stdout, "");
        assert_eq!(
            run.stderr,
            "Nothing to fix. 1 broken link has no certain fix; run supdoc check to see it.\n",
        );
    }

    #[test]
    fn fix_yes_with_changed_file_exits_1_and_reports_file_changed() {
        let project = Project::new();
        project.write("docs/other.md", "The [add](../src/lib.rs#ad) again.\n");
        let mut session = project.session(&["--json", "fix", "--yes"]);
        let (paths, mode) = {
            let args = session.fix_args();
            (args.paths.clone(), args.confirm.mode())
        };
        let planned = Planned::scan(&paths, &session.context, &mut session.output)
            .expect("the fixture project should plan");
        let edited = format!("Edited after the scan.\n{GUIDE}");
        project.write("docs/guide.md", &edited);

        let result = planned.finish(mode, &mut session.output);
        let run = session.finished(result);

        assert_eq!(run.exit_code(), 1, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/ok"), false);
        assert_eq!(at(&json, "/findings/0/kind"), "fix/file-changed");
        assert_eq!(at(&json, "/data/fixes/0/file"), "docs/other.md");
        assert_eq!(
            project.read("docs/guide.md"),
            edited,
            "a changed file is left alone"
        );
        assert_eq!(
            project.read("docs/other.md"),
            "The [add](../src/lib.rs#add) again.\n",
            "the other file is still fixed",
        );
    }

    #[cfg(unix)]
    #[test]
    fn fix_yes_with_unwritable_file_exits_74_and_fixes_the_rest() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = Project::new();
        project.write("notes/other.md", "The [add](../src/lib.rs#ad) again.\n");
        let locked = project.path("docs");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555))
            .expect("the fixture directory should become read-only");

        let run = project.run(&["--json", "fix", "--yes"]);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
            .expect("the fixture directory should become writable again");
        assert_eq!(run.exit_code(), 74, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/ok"), false);
        assert_eq!(at(&json, "/findings/0/kind"), "fix/write-failed");
        assert_eq!(at(&json, "/data/fixes/0/file"), "notes/other.md");
        assert_eq!(
            project.read("docs/guide.md"),
            GUIDE,
            "the unwritable file is untouched"
        );
        assert_eq!(
            project.read("notes/other.md"),
            "The [add](../src/lib.rs#add) again.\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fix_yes_with_a_changed_file_and_an_unwritable_file_exits_74() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = Project::new();
        project.write("locked/other.md", "The [add](../src/lib.rs#ad) again.\n");
        project.write("notes/third.md", "The [add](../src/lib.rs#ad) once more.\n");
        let mut session = project.session(&["--json", "fix", "--yes"]);
        let (paths, mode) = {
            let args = session.fix_args();
            (args.paths.clone(), args.confirm.mode())
        };
        let planned = Planned::scan(&paths, &session.context, &mut session.output)
            .expect("the fixture project should plan");
        let edited = format!("Edited after the scan.\n{GUIDE}");
        project.write("docs/guide.md", &edited);
        let locked = project.path("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555))
            .expect("the fixture directory should become read-only");

        let result = planned.finish(mode, &mut session.output);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
            .expect("the fixture directory should become writable again");
        let run = session.finished(result);
        assert_eq!(
            run.exit_code(),
            74,
            "a write failure outranks a changed file (F5); stderr: {}",
            run.stderr
        );
        let json = run.json();
        assert_eq!(at(&json, "/ok"), false);
        let kinds: BTreeSet<String> = at(&json, "/findings")
            .as_array()
            .expect("the findings should be an array")
            .iter()
            .map(|finding| at(finding, "/kind").to_string())
            .collect();
        assert_eq!(
            kinds,
            ["\"fix/file-changed\"", "\"fix/write-failed\""]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            "both failures should be reported"
        );
        assert_eq!(project.read("docs/guide.md"), edited);
        assert_eq!(
            project.read("locked/other.md"),
            "The [add](../src/lib.rs#ad) again.\n"
        );
        assert_eq!(
            project.read("notes/third.md"),
            "The [add](../src/lib.rs#add) once more.\n",
            "the third file is still fixed"
        );
    }

    #[test]
    fn fix_without_lockfile_does_not_create_one() {
        let project = Project::new();

        let run = project.run(&["fix", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(project.read("docs/guide.md"), FIXED_GUIDE);
        assert!(
            !project.path(LOCK_FILE_NAME).exists(),
            "no entry moved, so no lockfile is written"
        );
    }

    #[test]
    fn fix_leaves_an_unmoved_lockfile_byte_for_byte() {
        let project = Project::new();
        project.write("docs/kept.md", "Keep [add](../src/lib.rs#add).\n");
        project.write_lock(&project.hashes());
        let before = fs::read(project.path(LOCK_FILE_NAME)).expect("the lockfile should exist");

        let run = project.run(&["fix", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            fs::read(project.path(LOCK_FILE_NAME)).expect("the lockfile should still exist"),
            before,
        );
    }

    #[test]
    fn paths_narrow_the_fix_to_links_written_there() {
        let project = Project::new();
        project.write("notes/other.md", "The [add](../src/lib.rs#ad) again.\n");

        let run = project.run(&["fix", "--yes", "notes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(project.read("docs/guide.md"), GUIDE, "outside [PATHS]");
        assert_eq!(
            project.read("notes/other.md"),
            "The [add](../src/lib.rs#add) again.\n"
        );
    }

    #[test]
    fn missing_path_is_a_usage_error() {
        let project = Project::new();

        let run = project.run(&["fix", "nowhere.md"]);

        assert_eq!(run.exit_code(), 2);
        assert_eq!(run.stdout, "");
    }

    #[test]
    fn fix_jsonl_is_one_fix_record_per_fix_then_summary_with_applied() {
        let project = Project::new();
        project.write("notes/other.md", "The [add](../src/lib.rs#ad) again.\n");

        let run = project.run(&["--jsonl", "fix", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let lines: Vec<serde_json::Value> = run
            .stdout
            .lines()
            .map(|line| serde_json::from_str(line).expect("each JSONL line should be JSON"))
            .collect();
        let types: Vec<serde_json::Value> = lines.iter().map(|line| at(line, "/type")).collect();
        assert_eq!(types, ["fix", "fix", "summary"]);
        let summary = lines.last().expect("the stream should end with a summary");
        assert_eq!(at(summary, "/applied"), true);
        assert_eq!(at(summary, "/command"), "fix");
        assert_eq!(at(summary, "/ok"), true);
    }

    /// Every way `json` breaks the schema `schema()` publishes for `supdoc fix`.
    fn schema_violations(json: &serde_json::Value) -> Vec<String> {
        let schema = schema().expect("fix is built, so it has a schema");
        let schema = serde_json::to_value(schema).expect("the fix schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        validator
            .iter_errors(json)
            .map(|error| error.to_string())
            .collect()
    }

    #[test]
    fn fix_json_validates_against_its_schema() {
        let project = Project::new();
        let preview = project.run(&["--json", "fix"]).json();
        project.record_before_rename();
        let applied = project.run(&["--json", "fix", "--yes"]).json();
        assert_eq!(at(&applied, "/data/fixes/0/state"), "passed");

        for json in [preview, applied] {
            let errors = schema_violations(&json);
            assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
        }
    }

    #[test]
    fn fix_schema_rejects_a_report_without_applied() {
        let project = Project::new();
        let mut json = project.run(&["--json", "fix"]).json();
        let _applied = json
            .pointer_mut("/data")
            .and_then(serde_json::Value::as_object_mut)
            .expect("the envelope should carry data")
            .remove("applied")
            .expect("the report should carry applied");

        assert!(
            !schema_violations(&json).is_empty(),
            "a report without applied should fail the schema: {json}"
        );
    }

    #[test]
    fn every_fixable_finding_the_scan_produces_is_planned() {
        let project = Project::empty();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nauth = \"src\"\nweb = \"apps/web\"\n",
        );
        project.write("src/lib.rs", LIB);
        project.write(
            "docs/guide.md",
            "Symbol [a](../src/lib.rs#ad).\n\nCase [b](../SRC/lib.rs#add).\n\nNamespace [c](atuh:lib.rs#add).\n",
        );
        let scan = project.scan();
        let fixable: BTreeSet<(String, String)> = scan
            .links
            .values()
            .filter_map(|link| match &link.target {
                TargetState::Broken { finding } => Some(finding),
                TargetState::Present { .. } => None,
            })
            .chain(&scan.findings)
            .filter_map(|finding| {
                let fix = finding.fix.as_ref()?;
                Some((finding.code.to_owned(), fix.replacement.clone()))
            })
            .collect();
        assert_eq!(
            fixable.len(),
            3,
            "the fixture should offer three fixes: {fixable:?}"
        );

        let planned: BTreeSet<(String, String)> = plan_fixes(&scan)
            .fixes
            .iter()
            .map(|planned| {
                (
                    planned.finding.code.to_owned(),
                    planned.fix.replacement.clone(),
                )
            })
            .collect();

        assert_eq!(planned, fixable);
    }

    /// A link to `real` written as `written` in `docs/a.md`, recorded under its miscased key,
    /// is fixed by `fix --yes` and then passes `check`.
    fn assert_case_fix_then_check_passes(written: &str, real: &str) {
        let project = Project::new();
        project.write(real, LIB);
        let doc = |path: &str| format!("See [add]({path}#add).\n");
        let correct = written.to_lowercase();
        project.write("docs/a.md", &doc(&correct));
        project.write("docs/guide.md", "Nothing linked here.\n");
        let hash = project
            .hashes()
            .iter()
            .map(|(_, hash)| hash)
            .next()
            .unwrap_or_else(|| panic!("`{correct}` should resolve"));
        project.write("docs/a.md", &doc(written));
        project.write_lock(&[(project.only_key(), hash)].into_iter().collect());

        let fixed = project.run(&["--json", "fix", "--yes"]);
        assert_eq!(fixed.exit_code(), 0, "fix of `{written}`: {}", fixed.stderr);
        assert_eq!(
            at(&fixed.json(), "/data/fixes/0/cause"),
            "scan/case-mismatch"
        );
        assert_eq!(
            at(&fixed.json(), "/data/fixes/0/state"),
            "passed",
            "`{written}`"
        );
        assert_eq!(project.read("docs/a.md"), doc(&correct));

        let checked = project.run(&["--json", "check"]);
        assert_eq!(
            checked.exit_code(),
            0,
            "check after fixing `{written}`: {}",
            checked.stdout
        );
        assert_eq!(at(&checked.json(), "/data/summary/passed"), 1);
    }

    #[test]
    fn case_fix_of_a_same_directory_link_then_passes_check() {
        assert_case_fix_then_check_passes("LIB.rs", "docs/lib.rs");
    }

    #[test]
    fn case_fix_of_a_subdirectory_link_then_passes_check() {
        assert_case_fix_then_check_passes("Api/LIB.rs", "docs/api/lib.rs");
    }

    #[test]
    fn case_fix_of_a_link_climbing_to_the_root_then_passes_check() {
        assert_case_fix_then_check_passes("../SRC/lib.rs", "src/lib.rs");
    }
}
