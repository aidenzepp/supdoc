//! `supdoc fix`: rewrites broken links whose repair is certain, and refreshes stale
//! `from(...)` code blocks.
//!
//! The run is `config → lockfile → scan → plan_fixes`, then either the preview, or `apply_fixes →
//! move lock entries → write the lockfile if an entry moved → scan the fixed files again →
//! reconcile`, so each fixed link reports the state it has now. A preview runs every step a real
//! run does up to the first write.

use std::collections::BTreeSet;
use std::io::{self, Write};

use schemars::JsonSchema;
use serde::Serialize;

use crate::check::fix::{
    AppliedFix, FixPlan, FixResult, PlannedFix, Skipped, SkippedKind, apply_fixes,
    move_lock_entries, plan_fixes,
};
use crate::check::model::Report as CheckModel;
use crate::check::moves::Move;
use crate::check::reconcile::reconcile_in;
use crate::code_blocks::evaluate::evaluate;
use crate::code_blocks::tools::ToolEnv;
use crate::config::Config;
use crate::exit::ExitStatus;
use crate::finding::{Finding, Fix};
use crate::link::{LinkKey, SourcePath};
use crate::lock::error::LockError;
use crate::lock::model::Lockfile;
use crate::output::json::Envelope;
use crate::output::paths::relative_to;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, PathStyle, RecordArray, Report, TextContext, preview_note};
use crate::path::AbsolutePath;
use crate::read::cartridge::Languages;
use crate::read::error::CODE_TIMEOUT;
use crate::scan::model::{Scan, Scope, TargetState};
use crate::scan::walk::GlobalExcludes;
use crate::state::LinkState;

use super::args::{FixArgs, WriteMode};
use super::project::{command_line, load_config, lock_path, read_lock, scope};
use super::{CliError, Context, Outcome};

/// The code of the fatal error for source files `apply_fixes` could not write.
const CODE_WRITE_FAILED: &str = "fix-write-failed";

/// The code of the fatal error for sources rewritten while the lockfile could not be written.
const CODE_LOCK_WRITE_FAILED: &str = "fix-lock-write-failed";

/// The code of a `from(...)` block whose body differs from its source, which `fix` refreshes.
const CODE_STALE: &str = "code-block-stale";

/// The code of a block its formatter would change, which `fix` rewrites when it carries a fix.
const CODE_UNFORMATTED: &str = "code-block-unformatted";

/// Runs `supdoc fix`: previews every certain fix, or with `--yes` applies them and reports the
/// state each fixed link has now.
///
/// A file that changed since the scan is skipped and the run exits 1. A file that can't be
/// written is skipped and the run fails with exit 74 once the report is written. Either way the
/// other files are still fixed.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), `cli-path-missing`,
/// `cli-path-outside-project` or `cli-path-invalid` for a bad `[PATH]` (2), `cli-path-unreadable` (74) for one that can't be looked up, the lockfile's own
/// error (78 when invalid, 74 when it can't be read or written), the scan's own error (74),
/// `fix-write-failed` when a source file could not be written (74), and `cli-output-failed` when the
/// report cannot be written (74).
pub(crate) fn run(
    args: &FixArgs,
    tools: &ToolEnv,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let planned = Planned::scan(&args.paths, tools, context, output)?;
    planned.finish(
        args.confirm.mode(),
        args.confirm.dry_run,
        context,
        &args.paths,
        output,
    )
}

/// The JSON Schema of `supdoc fix`'s `--json` output: the whole envelope, with `data` typed as
/// [`FixReport`].
#[must_use]
pub(crate) fn schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, FixReport>)
}

/// A scanned project and the fixes it offers: everything a run does before its first write.
#[derive(Debug)]
struct Planned {
    /// The loaded config, whose root every source path is relative to.
    config: Config,
    /// The global excludes file the walk honors, for the re-scan.
    excludes: GlobalExcludes,
    /// The lockfile as read, empty when there is none.
    lock: Lockfile,
    /// The fixes to preview or apply.
    plan: FixPlan,
    /// How help commands name files: relative to the current directory.
    paths: PathStyle,
    /// Broken links in scope, fixable or not, for the note when nothing can be fixed.
    broken: usize,
    /// Stale `from(...)` blocks, fixable or not, and blocks with a formatter fix, for the note
    /// when nothing can be fixed.
    stale: usize,
    /// The command that lists the broken links, as typed here: it keeps this run's `[PATH]` and `-c`.
    check_command: String,
    /// The command that runs this fix again without `--yes`, keeping this run's `[PATH]` and `-c`.
    fix_command: String,
    /// One `read-timeout` per file that outlasted `parse-timeout`. Its links were never checked
    /// for fixes, so the run fails rather than saying there is nothing to fix.
    timeouts: Vec<Finding>,
}

impl Planned {
    /// Loads the config and the lockfile, scans `paths`, judges the code blocks as `check` does,
    /// running each formatter found on `tools`' `PATH`, and plans every certain fix: broken
    /// links, stale `from(...)` blocks and formatter output alike.
    ///
    /// # Errors
    ///
    /// Returns the config's, the lockfile's or the scan's own error, a `[PATH]` usage error, or
    /// `read-cartridge-load` (70) when a built-in cartridge doesn't load.
    fn scan(
        paths: &[std::path::PathBuf],
        tools: &ToolEnv,
        context: &Context,
        output: &mut Output,
    ) -> Result<Self, CliError> {
        let config = load_config(context, output)?;
        let scope = scope(paths, &config, context)?;
        let lock = read_lock(&config, &context.cwd)?;
        let mut scan = scan(&config, &scope, &context.global_excludes)?;
        let in_scope = |doc: &str| {
            doc.parse::<SourcePath>()
                .is_ok_and(|doc| scope.contains(&doc) && !scan.is_unread(&doc))
        };
        let languages = Languages::builtin().map_err(CliError::from)?;
        let mut evaluation = evaluate(&config, languages, &scan.code_blocks, &lock, in_scope)
            .map_err(|error| CliError::from(&error))?;
        evaluation.look_up_tools(tools, config.root().as_path());
        scan.findings.extend(evaluation.findings);
        let stale = scan
            .findings
            .iter()
            .filter(|finding| is_block_fix(finding))
            .count();
        let broken = scan
            .links
            .iter()
            .filter(|(key, link)| {
                scope.contains(&key.source) && matches!(link.target, TargetState::Broken { .. })
            })
            .count();
        // Every link into a timed-out target carries the same finding; one per file is enough.
        let mut files = BTreeSet::new();
        let timeouts: Vec<Finding> = timed_out(&scan)
            .filter(|finding| files.insert(finding.message.clone()))
            .cloned()
            .collect();
        let typed: Vec<String> = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let check_command = command_line(context, "check", &typed, &[]);
        let fix_command = command_line(context, "fix", &typed, &[]);
        let paths = PathStyle::new(context.cwd.clone(), config.root().clone(), None);
        Ok(Self {
            plan: plan_fixes(&scan),
            paths,
            config,
            excludes: context.global_excludes.clone(),
            lock,
            broken,
            stale,
            check_command,
            fix_command,
            timeouts,
        })
    }

    /// Previews the plan, or applies it and reports what each fixed link is now.
    ///
    /// # Errors
    ///
    /// Returns `fix-write-failed` (74) after the report when a source file could not be written,
    /// the lockfile's or the re-scan's own error, and `cli-output-failed` when the report cannot be
    /// written.
    fn finish(
        self,
        mode: WriteMode,
        args_dry_run: bool,
        context: &Context,
        paths: &[std::path::PathBuf],
        output: &mut Output,
    ) -> Result<Outcome, CliError> {
        let paths: Vec<String> = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        match mode {
            WriteMode::Preview => self.preview(
                &preview_note(
                    args_dry_run,
                    &command_line(context, "fix", &paths, &["--yes"]),
                ),
                output,
            ),
            WriteMode::Apply => self.apply(context, &paths, output),
        }
    }

    /// Reports every planned fix and writes nothing.
    ///
    /// # Errors
    ///
    /// Returns `cli-output-failed` when the report cannot be written.
    fn preview(self, apply_note: &str, output: &mut Output) -> Result<Outcome, CliError> {
        // The real rule on a copy of the lock, so the preview names exactly the moves `--yes` makes.
        let would = move_lock_entries(
            &mut self.lock.clone(),
            self.plan
                .fixes()
                .iter()
                .filter_map(|planned| planned.moved.as_ref()),
        );
        let fixes: Vec<FixRecord> = self
            .plan
            .fixes()
            .iter()
            .map(|planned| {
                let moved = planned
                    .moved
                    .clone()
                    .filter(|planned| would.contains(planned));
                FixRecord::new(planned, moved, None, None)
            })
            .collect();
        let report = FixReport {
            applied: false,
            fixes,
            findings: self.timeouts,
        };
        let timed_out = !report.findings.is_empty();
        output
            .report(&report, !timed_out)
            .map_err(|error| CliError::output_failed("the fix report", &error))?;
        let planned = if report.fixes.is_empty() {
            None
        } else {
            let fix_count = count(report.fixes.len(), "fix", "fixes");
            let file_count = count(report.files(), "file", "files");
            Some(format!("{fix_count} in {file_count}. {apply_note}"))
        };
        let note = match (timed_out, planned) {
            (true, Some(planned)) => format!(
                "{} {planned}",
                timeout_note(report.findings.len(), &self.fix_command)
            ),
            (true, None) => timeout_note(report.findings.len(), &self.fix_command),
            (false, Some(planned)) => planned,
            (false, None) => nothing_to_fix(self.broken, self.stale, &self.check_command),
        };
        output
            .note(&note)
            .map_err(|error| CliError::output_failed("the fix report", &error))?;
        Ok(if timed_out {
            Outcome::Failing
        } else {
            Outcome::Clean
        })
    }

    /// Writes every fix whose file still matches the scan, carries the lock entries of rewritten
    /// links, and reports the state each fixed link has after a fresh scan.
    ///
    /// # Errors
    ///
    /// Returns the re-scan's own error, `cli-output-failed` when the report cannot be written, and
    /// after the report `fix-lock-write-failed` (74) when the lockfile could not be written
    /// (the fixed files are named) or `fix-write-failed` (74) when a source file could not be.
    fn apply(
        self,
        context: &Context,
        paths: &[String],
        output: &mut Output,
    ) -> Result<Outcome, CliError> {
        let root = self.config.root();
        let rerun = command_line(context, "fix", paths, &["--yes"]);
        let written = self.write(root, &rerun);
        let done: Vec<&PlannedFix> = self
            .plan
            .fixes()
            .iter()
            .filter(|planned| {
                written
                    .applied
                    .iter()
                    .any(|applied| applied.fix == planned.fix)
            })
            .collect();
        let fixed_files = shown_files(&done, root, &context.cwd);
        let states = self.rescan(&done, &written.lock)?;
        let fixes = self.records(context, &done, &written.moved, &states);

        let write_failed = count_skipped(&written.skipped, SkippedKind::WriteFailed);
        let file_changed = count_skipped(&written.skipped, SkippedKind::FileChanged) > 0;
        let timeout_count = self.timeouts.len();
        let timed_out = timeout_count > 0;
        let report = FixReport {
            applied: true,
            fixes,
            findings: self
                .timeouts
                .into_iter()
                .chain(written.skipped.into_iter().map(|skipped| skipped.finding))
                .collect(),
        };
        let ok = write_failed == 0 && !file_changed && written.lock_failure.is_none() && !timed_out;
        output
            .report(&report, ok)
            .map_err(|error| CliError::output_failed("the fix report", &error))?;
        let note = if !report.fixes.is_empty() {
            // A refreshed block has nothing to record, so only rewritten links name `update`.
            let blocks = done
                .iter()
                .filter(|planned| is_block_fix(&planned.finding))
                .count();
            let links = done.len() - blocks;
            let file_count = count(report.files(), "file", "files");
            let block_count = count(blocks, "code block", "code blocks");
            Some(if links == 0 {
                format!("Fixed {block_count} in {file_count}.")
            } else {
                let link_count = count(links, "link", "links");
                let what = if blocks == 0 {
                    link_count
                } else {
                    format!("{link_count} and {block_count}")
                };
                let update = command_line(context, "update", &fixed_files, &["--yes"]);
                format!("Fixed {what} in {file_count}. Run `{update}` to record them.")
            })
        } else if timed_out {
            Some(timeout_note(timeout_count, &rerun))
        } else if self.plan.fixes().is_empty() {
            Some(nothing_to_fix(self.broken, self.stale, &self.check_command))
        } else {
            None
        };
        if let Some(note) = note {
            output
                .note(&note)
                .map_err(|error| CliError::output_failed("the fix report", &error))?;
        }

        if let Some(error) = written.lock_failure {
            let changed: BTreeSet<&SourcePath> =
                done.iter().map(|planned| &planned.fix.file).collect();
            Err(lock_unwritten(
                &error,
                root,
                &changed,
                context,
                paths,
                &fixed_files,
            ))
        } else if write_failed > 0 {
            Err(sources_unwritten(
                write_failed,
                !report.fixes.is_empty(),
                &rerun,
            ))
        } else if file_changed || timed_out {
            Ok(Outcome::Failing)
        } else {
            Ok(Outcome::Clean)
        }
    }

    /// Phase one: rewrites the source files, then moves the lock entries of rewritten links and
    /// writes the lockfile if any moved.
    fn write(&self, root: &AbsolutePath, rerun: &str) -> Written {
        let FixResult { applied, skipped } = apply_fixes(&self.plan, root, rerun);
        let mut lock = self.lock.clone();
        let mut moved = move_lock_entries(
            &mut lock,
            applied.iter().filter_map(|applied| applied.moved.as_ref()),
        );
        let mut lock_failure = None;
        if lock != self.lock
            && let Err(error) = lock.write(&lock_path(root))
        {
            // The write is atomic, so the lockfile on disk is still the one that was read.
            lock = self.lock.clone();
            moved.clear();
            lock_failure = Some(error);
        }
        Written {
            applied,
            skipped,
            lock,
            moved,
            lock_failure,
        }
    }

    /// Phase two: scans the fixed files again and reconciles them with `lock`.
    ///
    /// # Errors
    ///
    /// Returns the re-scan's own error.
    fn rescan(&self, done: &[&PlannedFix], lock: &Lockfile) -> Result<CheckModel, CliError> {
        if done.is_empty() {
            return Ok(CheckModel::default());
        }
        let fixed: BTreeSet<SourcePath> = done
            .iter()
            .map(|planned| planned.fix.file.clone())
            .collect();
        let scope = Scope::Paths(fixed.into_iter().collect());
        Ok(reconcile_in(
            &scan(&self.config, &scope, &self.excludes)?,
            lock,
            &scope,
            &self.paths,
        ))
    }

    /// Phase three: one record per applied fix, with the state its link has now.
    fn records(
        &self,
        context: &Context,
        done: &[&PlannedFix],
        moved: &[Move],
        states: &CheckModel,
    ) -> Vec<FixRecord> {
        done.iter()
            .zip(new_ranges(done))
            .map(|(planned, range)| {
                let moved = planned
                    .moved
                    .clone()
                    .filter(|planned| moved.contains(planned));
                let state = state_at(states, &planned.fix.file, range);
                // `supdoc update` takes the path from the current directory, like the report.
                let update = (state == Some(LinkState::Failed)).then(|| {
                    let path = self.paths.text(&planned.fix.file);
                    command_line(context, "update", &[path.as_str()], &["--yes"])
                });
                FixRecord::new(planned, moved, state, update)
            })
            .collect()
    }
}

/// What phase one of an apply left behind.
struct Written {
    /// The edits on disk.
    applied: Vec<AppliedFix>,
    /// The files left alone.
    skipped: Vec<Skipped>,
    /// The lockfile as it is on disk after the run.
    lock: Lockfile,
    /// The lock entries that moved to a rewritten link's new key.
    moved: Vec<Move>,
    /// Why the lockfile could not be written, if it could not.
    lock_failure: Option<LockError>,
}

/// How many skips are of `kind`.
fn count_skipped(skipped: &[Skipped], kind: SkippedKind) -> usize {
    skipped
        .iter()
        .filter(|skipped| skipped.kind == kind)
        .count()
}

/// Scans `scope` with the built-in languages, under the global `excludes` file.
///
/// # Errors
///
/// Returns the scan's own error, such as an unreadable root (74).
fn scan(config: &Config, scope: &Scope, excludes: &GlobalExcludes) -> Result<Scan, CliError> {
    crate::scan::assemble::run(
        config,
        Languages::builtin().map_err(CliError::from)?,
        scope,
        excludes,
    )
    .map_err(CliError::from)
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
            scanned: _,
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

/// The distinct files `done` rewrote, named relative to `cwd` so a command naming them runs there.
fn shown_files(done: &[&PlannedFix], root: &AbsolutePath, cwd: &AbsolutePath) -> Vec<String> {
    let shown: BTreeSet<String> = done
        .iter()
        .map(|planned| {
            let file = root.as_path().join(planned.fix.file.as_str());
            AbsolutePath::new(&file).map_or_else(
                || planned.fix.file.as_str().to_owned(),
                |file| relative_to(cwd, &file),
            )
        })
        .collect();
    shown.into_iter().collect()
}

/// `1 fix` or `2 fixes`.
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The note for a run in which `files` files outlasted `parse-timeout`, so their links got no
/// fixes. `rerun` is the command that runs the fix again.
fn timeout_note(files: usize, rerun: &str) -> String {
    let files = count(files, "file", "files");
    format!(
        "Reading {files} took longer than `parse-timeout` allows per read, so links there got no \
         fixes; raise the limit and run `{rerun}` again."
    )
}

/// Every `read-timeout` in `scan`: a source whose links weren't read, then each link into a
/// target that wasn't. Every command that meets one exits 1.
pub(crate) fn timed_out(scan: &Scan) -> impl Iterator<Item = &Finding> {
    let targets = scan.links.values().filter_map(|link| match &link.target {
        TargetState::Broken { finding } => Some(finding),
        TargetState::Present { .. } => None,
    });
    scan.findings
        .iter()
        .chain(targets)
        .filter(|finding| finding.code == CODE_TIMEOUT)
}

/// Whether `finding` is a code block's own fix: a stale `from(...)` block, or formatter output.
/// Neither has anything for `update` to record.
fn is_block_fix(finding: &Finding) -> bool {
    finding.code == CODE_STALE || (finding.code == CODE_UNFORMATTED && finding.fix.is_some())
}

/// The note for a run with nothing to fix, naming the `broken` links and `stale` code blocks
/// left as they are. `check` is the command that lists them.
fn nothing_to_fix(broken: usize, stale: usize, check: &str) -> String {
    let left: Vec<String> = [
        (broken, "broken link", "broken links"),
        (stale, "stale code block", "stale code blocks"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, one, many)| count(n, one, many))
    .collect();
    if left.is_empty() {
        return "Nothing to fix.".to_owned();
    }
    let (verb, them) = if broken + stale == 1 {
        ("has", "it")
    } else {
        ("have", "them")
    };
    format!(
        "Nothing to fix. {} {verb} no certain fix; run `{check}` to see {them}.",
        left.join(" and ")
    )
}

/// The error that ends a run in which `files` source files could not be written. The report
/// already carries a `fix-write-failed` finding for each one.
///
/// The message says the other fixes were applied only when `others_applied`.
fn sources_unwritten(files: usize, others_applied: bool, rerun: &str) -> CliError {
    let others = if others_applied {
        "; the other fixes were applied"
    } else {
        ""
    };
    let message = if files == 1 {
        format!("1 file could not be written{others}")
    } else {
        format!("{files} files could not be written{others}")
    };
    CliError::new(
        CODE_WRITE_FAILED,
        message,
        format!("Make the files writable and run `{rerun}` again"),
        ExitStatus::Io,
    )
}

/// The error for source files that were rewritten while the lockfile could not be written.
///
/// The lockfile is untouched, so it still holds each fixed link's history under its old key.
/// Rerunning `fix` finds nothing left to fix, so the help offers the two ways forward that work.
/// The message names the lockfile relative to the project `root`, as it names the fixed files.
fn lock_unwritten(
    error: &LockError,
    root: &AbsolutePath,
    files: &BTreeSet<&SourcePath>,
    context: &Context,
    paths: &[String],
    fixed: &[String],
) -> CliError {
    let rerun = command_line(context, "fix", paths, &["--yes"]);
    let update = command_line(context, "update", fixed, &["--yes"]);
    let names: Vec<&str> = files.iter().map(|file| file.as_str()).collect();
    CliError::new(
        CODE_LOCK_WRITE_FAILED,
        format!(
            "fixed {}, but supdoc.lock could not be written: {}",
            names.join(", "),
            error.message_from(root)
        ),
        format!(
            "supdoc.lock is unchanged and still holds the old links' history. \
             Make it writable, then either restore those files and run `{rerun}` again \
             to carry the history over, or run `{update}` on them to lock the fixed \
             links afresh without it"
        ),
        ExitStatus::Io,
    )
}

/// `supdoc fix`: the planned or applied edits, and the files left alone.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct FixReport {
    /// `false` for a preview, `true` once the fixes were written.
    applied: bool,
    /// One per planned fix in a preview; one per written fix after `--yes`. Sorted by file, then
    /// position.
    fixes: Vec<FixRecord>,
    /// Files `--yes` left alone: `fix-file-changed` and `fix-write-failed`.
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
    fn record_arrays(&self) -> &'static [RecordArray] {
        const { &[RecordArray::new("fixes", "fix")] }
    }

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
    /// The code of the finding repaired, such as `resolve-not-found` or `scan-case-mismatch`.
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
    /// The command that records the fixed link, for text, when its state after the fix is `failed`.
    #[serde(skip)]
    update_command: Option<String>,
}

impl FixRecord {
    /// The record of `planned`, with the lock entry that moved (or, in a preview, would move) and
    /// the state its link has after the fix when known.
    fn new(
        planned: &PlannedFix,
        moved: Option<Move>,
        state: Option<LinkState>,
        update_command: Option<String>,
    ) -> Self {
        let Fix {
            file,
            range,
            replacement,
            scanned: _,
        } = &planned.fix;
        let finding = &planned.finding;
        let before = planned.fix.scanned.to_string();
        Self {
            file: PathStyle::json(file),
            line: planned.location.line,
            column: planned.location.column,
            start: range.start,
            end: range.end,
            before,
            after: replacement.clone(),
            cause: finding.code.to_owned(),
            moved: moved.map(|Move { from, to }| MovedJson { from, to }),
            state,
            source: file.clone(),
            old_link: planned.old_link.clone(),
            new_link: planned.new_link.clone(),
            update_command,
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
            if state == LinkState::Failed
                && let Some(command) = &self.update_command
            {
                writeln!(out, "  = help: reread it, then run {}", visible(command))?;
            }
        }
        writeln!(out)
    }
}

/// The lock entry a fix carries from a link's old key to its new one.
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

    use super::*;
    use crate::cli::args::Command;
    use crate::cli::testing::{Run, Session, at};
    use crate::limits::Limits;
    use crate::lock::model::LOCK_FILE_NAME;

    /// A guide whose link misspells `add` as `ad`, which has exactly one near match.
    const GUIDE: &str = "# Guide\n\nThe [add](../src/lib.rs#ad) function sums.\n";

    /// The guide once `fix --yes` has repaired it.
    const FIXED_GUIDE: &str = "# Guide\n\nThe [add](../src/lib.rs#add) function sums.\n";

    /// The code the guide points at.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

    /// The arguments of the `fix` command `session` parsed.
    fn fix_args(session: &Session) -> &FixArgs {
        let Command::Fix(args) = &session.cli.command else {
            panic!("the session should hold a fix command");
        };
        args
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
            crate::scan::assemble::run(
                &config,
                Languages::builtin().expect("the built-in cartridges should load"),
                &Scope::All,
                &GlobalExcludes::none(),
            )
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
            Lockfile::read(&path, Limits::MAX_LOCK_SIZE)
                .expect("the lockfile should be valid")
                .expect("the lockfile should exist")
        }

        /// Records the hash `GUIDE`'s link will have once fixed under its broken key, as if the
        /// symbol was renamed after `update --yes`.
        fn record_before_rename(&self) {
            self.record_before_rename_at("docs/guide.md");
        }

        /// [`Self::record_before_rename`] for the guide at `relative`, which must be the only one.
        fn record_before_rename_at(&self, relative: &str) {
            self.write(relative, FIXED_GUIDE);
            let hashes = self.hashes();
            self.write(relative, GUIDE);
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
            Session::new(self.root(), args)
        }

        /// Runs `supdoc <args>` from the project root, through the real dispatch.
        fn run(&self, args: &[&str]) -> Run {
            let mut session = self.session(args);
            let result = crate::cli::run(
                &session.cli.command,
                &crate::cli::skill::SkillDirs::from_vars(|_| None),
                &crate::code_blocks::tools::ToolEnv::default(),
                &session.context,
                &mut session.output,
            );
            session.finished(result)
        }
    }

    #[test]
    fn fix_yes_in_a_directory_with_a_space_ends_with_a_runnable_update_command() {
        let project = Project::new();
        project.write("my docs/guide.md", GUIDE);

        let fixed = project.run(&["fix", "--yes"]);

        let command = "supdoc update docs/guide.md 'my docs/guide.md' --yes";
        assert!(
            fixed
                .stderr
                .contains(&format!("Run `{command}` to record them.")),
            "the closing note should end with the quoted update command: {}",
            fixed.stderr
        );
        let updated = project.run(&["update", "docs/guide.md", "my docs/guide.md", "--yes"]);
        assert_eq!(updated.exit_code(), 0, "stderr: {}", updated.stderr);
    }

    #[test]
    fn fix_yes_help_for_a_failed_link_keeps_the_config_and_quotes_a_spaced_path() {
        let project = Project::empty();
        project.write("src/lib.rs", LIB);
        project.record_before_rename_at("my docs/guide.md");
        project.write(
            "src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        );

        let run = project.run(&["-c", "supdoc.config.toml", "fix", "--yes"]);

        assert!(
            run.stdout.contains(
                "= help: reread it, then run \
                 supdoc -c supdoc.config.toml update 'my docs/guide.md' --yes\n"
            ),
            "the help should keep -c and quote the path: {}",
            run.stdout
        );
    }

    #[test]
    fn fix_yes_with_a_config_flag_keeps_it_in_the_update_command() {
        let project = Project::new();

        let fixed = project.run(&["-c", "supdoc.config.toml", "fix", "--yes"]);

        assert!(
            fixed
                .stderr
                .contains("`supdoc -c supdoc.config.toml update docs/guide.md --yes`"),
            "the update command should keep -c: {}",
            fixed.stderr
        );
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
                "cause": "resolve-not-found",
                "moved": {
                    "from": {"source": "docs/guide.md", "target": "src/lib.rs", "symbol": "ad"},
                    "to": {"source": "docs/guide.md", "target": "src/lib.rs", "symbol": "add"},
                },
            }]),
        );
        assert_eq!(project.files(), before, "a preview writes nothing");
    }

    #[test]
    fn colliding_moves_preview_exactly_the_moves_apply_makes() {
        let project = Project::new();
        project.write(
            "docs/guide.md",
            "# Guide\n\n[one](../src/lib.rs#ad) and [two](../src/lib.rs#addd).\n",
        );
        let hash = |text: &str| crate::hash::Hash::digest(text.as_bytes());
        let key = |symbol: &str| LinkKey {
            source: "docs/guide.md"
                .parse()
                .expect("the fixture source should be valid"),
            target: "src/lib.rs"
                .parse()
                .expect("the fixture target should be valid"),
            symbol: Some(symbol.parse().expect("the fixture symbol should be valid")),
        };
        let before: Lockfile = [(key("ad"), hash("one")), (key("addd"), hash("two"))]
            .into_iter()
            .collect();
        project.write_lock(&before);

        let preview = project.run(&["--json", "fix"]);
        let previewed: Vec<serde_json::Value> = preview
            .json()
            .pointer("/data/fixes")
            .and_then(serde_json::Value::as_array)
            .expect("the preview should list fixes")
            .iter()
            .filter_map(|fix| fix.get("moved").cloned())
            .collect();
        let applied = project.run(&["--json", "fix", "--yes"]);
        let after = project.lock();

        let moved_in_lock = [key("ad"), key("addd")]
            .iter()
            .filter(|old| before.get(old).is_some() && after.get(old).is_none())
            .count();
        assert_eq!(applied.exit_code(), 0, "stderr: {}", applied.stderr);
        assert_eq!(
            previewed.len(),
            moved_in_lock,
            "the preview moves {previewed:?} but the lock changed by {moved_in_lock} entries"
        );
        assert_eq!(moved_in_lock, 1, "two moves onto one key can only make one");
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
        assert_eq!(error.code(), "fix-lock-write-failed");
        assert!(
            error.message().contains("docs/guide.md"),
            "the error should name the fixed file: {}",
            error.message()
        );
        assert!(
            error
                .message()
                .contains("could not be written: cannot write supdoc.lock: "),
            "the error should name the lockfile relative to the root, not absolutely: {}",
            error.message()
        );
        assert!(
            error.help().contains("supdoc update docs/guide.md --yes")
                && error.help().contains("history"),
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
        assert_eq!(
            run.stderr,
            "Fixed 1 link in 1 file. Run `supdoc update docs/guide.md --yes` to record them.\n"
        );
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
            "1 fix in 1 file. Nothing written. Run `supdoc fix --yes` to apply.\n",
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
            "Nothing to fix. 1 broken link has no certain fix; run `supdoc check` to see it.\n",
        );
    }

    #[test]
    fn fix_yes_with_changed_file_exits_1_and_reports_file_changed() {
        let project = Project::new();
        project.write("docs/other.md", "The [add](../src/lib.rs#ad) again.\n");
        let mut session = project.session(&["--json", "fix", "--yes"]);
        let (paths, mode) = {
            let args = fix_args(&session);
            (args.paths.clone(), args.confirm.mode())
        };
        let planned = Planned::scan(
            &paths,
            &ToolEnv::default(),
            &session.context,
            &mut session.output,
        )
        .expect("the fixture project should plan");
        let edited = format!("Edited after the scan.\n{GUIDE}");
        project.write("docs/guide.md", &edited);

        let result = planned.finish(mode, false, &session.context, &paths, &mut session.output);
        let run = session.finished(result);

        assert_eq!(run.exit_code(), 1, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/ok"), false);
        assert_eq!(at(&json, "/findings/0/kind"), "fix-file-changed");
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
        assert_eq!(at(&json, "/findings/0/kind"), "fix-write-failed");
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
            let args = fix_args(&session);
            (args.paths.clone(), args.confirm.mode())
        };
        let planned = Planned::scan(
            &paths,
            &ToolEnv::default(),
            &session.context,
            &mut session.output,
        )
        .expect("the fixture project should plan");
        let edited = format!("Edited after the scan.\n{GUIDE}");
        project.write("docs/guide.md", &edited);
        let locked = project.path("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555))
            .expect("the fixture directory should become read-only");

        let result = planned.finish(mode, false, &session.context, &paths, &mut session.output);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
            .expect("the fixture directory should become writable again");
        let run = session.finished(result);
        assert_eq!(
            run.exit_code(),
            74,
            "a write failure outranks a changed file; stderr: {}",
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
            ["\"fix-file-changed\"", "\"fix-write-failed\""]
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
        let schema = schema();
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
                let fix = finding.fix.as_deref()?;
                Some((finding.code.to_owned(), fix.replacement.clone()))
            })
            .collect();
        assert_eq!(
            fixable.len(),
            3,
            "the fixture should offer three fixes: {fixable:?}"
        );

        let planned: BTreeSet<(String, String)> = plan_fixes(&scan)
            .fixes()
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
            "scan-case-mismatch"
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
