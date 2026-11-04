//! The per-block pipeline that `check` runs: judges each code block and returns its findings
//! and the count of blocks in each state.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::num::NonZeroUsize;
use std::ops::Range;
use std::panic::resume_unwind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::config::code_blocks::{Choice, CodeBlocks, Dialect, Edition, Language};
use crate::finding::{Finding, Location, Severity};
use crate::hash::Hash;
use crate::lock::model::Lockfile;
use crate::output::paths::PathStyle;
use crate::read::cartridge::Languages;
use crate::read::error::CartridgeLoadError;

use super::block::{CodeBlock, Info};
use super::cache::{self, Cache};
use super::excerpt;
use super::format::{self, FixDoc};
use super::info::{
    KNOWN_ATTRIBUTES, SourceKind, is_known_attribute, parenthesized_spelling, reasoned,
    source_attributes,
};
use super::lint;
use super::runner::{self, Call, Ran};
use super::rust::{self, Scan};
use super::shell;
use super::source::{self, SourceText, Unresolved};
use super::states::{self, Excerpt, NotChecked, StepState, Steps, state_of, tag_of};
use super::toolchain::{self, Refusal, Toolchain};
use super::tools::{Found, Step, TOOLS, Tool, ToolEnv, find, formatter, linter};
use super::uses::{self, UseState, Written};

pub(crate) use super::states::{BlockState, CodeBlockCounts};

/// The code for an info-string attribute supdoc doesn't know.
const UNKNOWN_ATTRIBUTE: &str = "code-block-unknown-attribute";

/// The code for an `ignore`, `no_format` or `no_lint` written without a reason.
const IGNORE_REASON: &str = "code-block-ignore-reason";

/// The code for a selected tool that isn't on `PATH`.
const TOOL_MISSING: &str = "code-block-tool-missing";

/// The code for blocks a hardening rule refused to check.
const NOT_CHECKED: &str = "code-block-not-checked";

/// What judging the code blocks found.
#[derive(Debug, Default)]
pub(crate) struct Evaluation {
    /// The findings, in block order, then the `uses(...)` findings.
    pub(crate) findings: Vec<Finding>,
    /// How many blocks are in each state. `None` when `[features.code-blocks]` is off.
    pub(crate) counts: Option<CodeBlockCounts>,
    /// One record per block, in block order. Empty when `[features.code-blocks]` is off.
    pub(crate) records: Vec<BlockRecord>,
    /// How long one tool run may take, from `[limits] code-block-timeout`. `None` is no limit.
    timeout: Option<Duration>,
}

/// What `check` knows about one block.
#[derive(Debug, Clone)]
pub(crate) struct BlockRecord {
    /// Where the block's opening fence is.
    pub(crate) at: Location,
    /// The language tag the block is read as, if it has one.
    pub(crate) language: Option<String>,
    /// The block's state.
    pub(crate) state: BlockState,
    /// What its format and lint steps did.
    pub(crate) steps: Steps,
    /// Why it is in that state, when there is a reason.
    pub(crate) reason: Option<String>,
    /// Each `uses(...)` target the block writes, with its state.
    pub(crate) uses: Vec<(String, UseState)>,
    /// The block's 1-based position among the blocks of its doc.
    index: usize,
    /// The formatter the config selects for it, while the block is pending.
    tool: Option<&'static Tool>,
    /// The linter the config selects for it, while the block is pending and its lint step due.
    linter: Option<&'static Tool>,
    /// The shell dialect shfmt parses it as and shellcheck checks it as.
    dialect: Dialect,
    /// The edition its `edition20xx` attribute or the config's `edition` names, if either does.
    edition: Option<Edition>,
    /// What the format step needs, kept while the block is pending.
    pending: Option<Pending>,
}

/// A pending block as the format step reads and fixes it.
#[derive(Debug, Clone)]
struct Pending {
    /// The block.
    block: CodeBlock,
    /// The doc it was found in, which its fix is checked against.
    doc: Arc<FixDoc>,
    /// What a formatter reads for it: for a Rust block, its text with hidden lines' `# ` removed.
    input: String,
    /// A Rust block's hidden lines, which its fix must keep.
    hidden: Option<rust::Hidden>,
}

impl Pending {
    /// `block`, found in `doc`, ready for its steps: read through [`rust::unhide`] when it is a
    /// Rust block. `None` when its body isn't inside `doc`.
    fn of(block: &CodeBlock, doc: &Arc<FixDoc>, language: Option<Language>) -> Option<Self> {
        let input = format::input(block, doc.text())?;
        let (input, hidden) = if language == Some(Language::Rust) {
            let (checked, hidden) = rust::unhide(&input);
            (checked, Some(hidden))
        } else {
            (input, None)
        };
        Some(Self {
            input,
            hidden,
            block: block.clone(),
            doc: Arc::clone(doc),
        })
    }
}

/// The findings for `blocks`, in block order, reading each `from(...)` and `uses(...)` source
/// with `languages`, and one `code-block-stale` error for each `from(...)` block whose body
/// differs from its parts, followed by a `code-block-fix-unsafe` warning when
/// [`format::safe_fix`] refuses its fix. Then the `uses(...)` findings against `lock`'s `[code-blocks]`
/// section, judging a doc's recorded targets obsolete only when `in_scope` holds for it and none
/// of its `uses(...)` values fails to resolve. Their help commands name each doc as `paths`
/// shows it, relative to the current directory. With them, each block's [`BlockState`], counted.
/// Nothing when `config` leaves `[features.code-blocks]` off.
///
/// Effect: reads every source file the blocks name, contained in the project root, and each doc
/// holding a `from(...)` block.
///
/// # Errors
///
/// [`CartridgeLoadError`] when a source's built-in cartridge doesn't load: a supdoc bug that
/// fails the run (exit 70).
pub(crate) fn evaluate(
    config: &Config,
    languages: &Languages,
    blocks: &[CodeBlock],
    lock: &Lockfile,
    in_scope: impl Fn(&str) -> bool,
    paths: &PathStyle,
) -> Result<Evaluation, CartridgeLoadError> {
    let Some(code_blocks) = config.code_blocks() else {
        return Ok(Evaluation::default());
    };
    let mut findings = Vec::new();
    let mut doc: Option<(&str, Option<Arc<FixDoc>>)> = None;
    let parse_timeout = config.limits_in_effect().parse_timeout.value;
    let read = |file: &str| {
        read_doc(config, file).map(|text| Arc::new(FixDoc::new(Arc::from(text), parse_timeout)))
    };
    let mut written = Vec::new();
    let mut unresolved_uses = BTreeSet::new();
    let recorded = recorded_by_target(lock);
    let mut records = Vec::with_capacity(blocks.len());
    let mut index = 0;
    for (position, block) in blocks.iter().enumerate() {
        let same_doc = position
            .checked_sub(1)
            .and_then(|previous| blocks.get(previous))
            .is_some_and(|previous| previous.fence.file == block.fence.file);
        index = if same_doc { index + 1 } else { 1 };
        let mut outcome = None;
        let mut block_uses = Vec::new();
        if let Some(info) = block.info.as_ref() {
            findings.extend(unknown_attributes(info));
            findings.extend(missing_reasons(info));
            let sources = sources(config, languages, info)?;
            findings.extend(sources.missing);
            block_uses.extend(use_states(
                &sources.uses,
                sources.unresolved,
                &recorded,
                block.fence.file.as_str(),
            ));
            written.extend(sources.uses);
            unresolved_uses.extend(sources.use_missing.then_some(block.fence.file.as_str()));
            outcome = sources.from_missing.then_some(Excerpt::SourceMissing);
            if let Some(parts) = sources.parts {
                let file = block.fence.file.as_str();
                if doc.as_ref().is_none_or(|(read, _)| *read != file) {
                    doc = Some((file, read(file)));
                }
                let stale = doc
                    .as_ref()
                    .and_then(|(_, text)| text.as_ref())
                    .map(|text| {
                        let line_endings = config.hash_settings().line_endings;
                        let expected = excerpt::expected(&parts, line_endings);
                        excerpt::stale(block, text, &expected, line_endings)
                    })
                    .unwrap_or_default();
                outcome = Some(if stale.is_empty() {
                    Excerpt::Current
                } else {
                    Excerpt::Stale
                });
                findings.extend(stale);
            }
        }
        let state = state_of(block, outcome, |language| code_blocks.checks(language));
        let language = tag_of(block).map(str::to_owned);
        let steps = Steps::of(block, &state, code_blocks);
        let checked = language.as_deref().and_then(Language::from_tag);
        let (tool, linter) = selected_tools(code_blocks, &steps, checked);
        let pending = (state == BlockState::NotChecked(NotChecked::Pending))
            .then(|| {
                let file = block.fence.file.as_str();
                if doc.as_ref().is_none_or(|(read, _)| *read != file) {
                    doc = Some((file, read(file)));
                }
                doc.as_ref()?
                    .1
                    .as_ref()
                    .and_then(|text| Pending::of(block, text, checked))
            })
            .flatten();
        records.push(BlockRecord {
            at: block.fence.clone(),
            reason: state.reason(&steps, language.as_deref(), tool.map(|tool| tool.name)),
            tool,
            linter,
            dialect: code_blocks.dialect(),
            edition: block_edition(block).or_else(|| code_blocks.edition()),
            language,
            state,
            steps,
            uses: block_uses,
            index,
            pending,
        });
    }
    let judged = |doc: &str| in_scope(doc) && !unresolved_uses.contains(doc);
    let command = |doc: &str| paths.command_word(doc);
    findings.extend(uses::judge(&written, lock.recorded_uses(), judged, command));
    Ok(Evaluation {
        findings,
        counts: Some(tally(&records)),
        records,
        timeout: config.limits_in_effect().code_block_timeout.value.get(),
    })
}

impl Evaluation {
    /// Looks up the formatter and the linter of each pending block's language on `env`'s
    /// `PATH`, once per tool; a linter counts only once its language has a lint step. A tool
    /// not found adds one `code-block-tool-missing` warning naming its install command; one
    /// whose real path is inside `root` adds one `code-block-not-checked` warning. When neither
    /// of a block's tools was found outside `root`, the first refusal decides it:
    /// [`BlockState::ToolMissing`] or not checked.
    ///
    /// A found formatter runs, hardened, on each block of a language with a format step, which
    /// passes it or fails it with one `code-block-unformatted`, `code-block-parse`,
    /// `code-block-tool-failed` or `code-block-tool-timeout` error. An unformatted block's error
    /// carries the fix to the formatter's output, unless [`format::safe_fix`] finds it could end
    /// the block early, which adds a `code-block-fix-unsafe` warning instead. A found linter then
    /// runs on the block as written, whatever the format verdict, except on a block the
    /// formatter couldn't parse: each diagnostic is one `code-block-lint` error (see [`lint`]).
    /// A block's format findings come before its lint findings, and its state is the outcome
    /// over the steps that ran.
    ///
    /// A block whose run is in the result cache (see [`cache`]) isn't run again; a run that
    /// passed or plainly failed is stored there, a tool failure or timeout never is.
    ///
    /// The runs for distinct blocks go in parallel (see [`in_parallel`]), each under its own
    /// timeout; findings and states come out as a serial run gives them, in block order.
    ///
    /// Effect: reads file metadata along `PATH`, resolves symlinks, reads and writes the result
    /// cache, and runs each found tool once per block not in it, killing it at the timeout.
    pub(crate) fn look_up_tools(&mut self, env: &ToolEnv, root: &Path) {
        let mut cache: Option<Option<Cache>> = None;
        let mut looked_up: Vec<(&'static str, Found)> = Vec::new();
        let mut rust_setup: Option<RustSetup> = None;
        let mut toolchain_warned = false;
        let mut slots: Vec<Slot> = Vec::new();
        let mut jobs: Vec<Job> = Vec::new();
        for (position, record) in self.records.iter_mut().enumerate() {
            if record.state != BlockState::NotChecked(NotChecked::Pending) {
                continue;
            }
            let doc = root.join(record.at.file.as_str());
            let linter = record
                .linter
                .filter(|tool| lint::args(tool, &doc, record.index).is_some());
            let Some(tool) = record.tool.or(linter) else {
                continue;
            };
            let rust = (tool.language == Language::Rust)
                .then(|| &*rust_setup.get_or_insert_with(|| RustSetup::read(root)));
            if let Some(rust) = rust {
                let compiles = linter.is_some_and(|linter| linter.language == Language::Rust);
                let refused =
                    rust_refusal(record, rust, compiles, &mut toolchain_warned, &mut slots);
                if let Some((refused, named)) = refused {
                    record.state = BlockState::NotChecked(refused);
                    record.reason = named.or_else(|| {
                        record.state.reason(
                            &record.steps,
                            record.language.as_deref(),
                            Some(tool.name),
                        )
                    });
                    continue;
                }
            }
            let (runs, refused) = found_runs(
                [record.tool, linter],
                &mut looked_up,
                &mut slots,
                env,
                root,
                &record.at,
            );
            let [format, lint] = runs;
            if format.is_some() || lint.is_some() {
                let _opened = cache.get_or_insert_with(|| Cache::open(env, root));
                slots.push(Slot::Run(jobs.len()));
                jobs.push(Job {
                    record: position,
                    format,
                    lint,
                });
                continue;
            }
            if let Some((state, tool)) = refused {
                record.state = state;
                record.reason =
                    record
                        .state
                        .reason(&record.steps, record.language.as_deref(), Some(tool.name));
            }
        }
        let cache = cache.flatten();
        let session = Session {
            env,
            root,
            timeout: self.timeout,
            cache: cache.as_ref(),
        };
        let records = &self.records;
        let ran = in_parallel(&jobs, |job| {
            let record = records.get(job.record)?;
            let format = job.format.as_ref().and_then(|run| {
                let rust = rust_setup
                    .as_ref()
                    .filter(|_| run.tool.language == Language::Rust);
                run_formatter(run.tool, &run.program, record, &session, rust)
            });
            let parses = !matches!(format, Some((format::Verdict::ParseFailed { .. }, _)));
            let lint = job.lint.as_ref().filter(|_| parses).and_then(|run| {
                let rust = rust_setup
                    .as_ref()
                    .filter(|_| run.tool.language == Language::Rust);
                run_linter(run.tool, &run.program, record, &session, rust)
            });
            Some(StepsRan { format, lint })
        });
        self.settle(slots, &jobs, ran);
        if let Some(cache) = cache {
            cache.prune();
        }
        if self.counts.is_some() {
            self.counts = Some(tally(&self.records));
        }
    }
}

impl Evaluation {
    /// Adds `slots`' findings in order, each [`Slot::Run`] taking its job's result from `ran`, which
    /// holds one per job of `jobs` in order: a block's format findings, then its lint findings.
    /// Each run block's state is the outcome over the steps that ran, the first failure (format
    /// before lint) deciding it and naming its tool in the reason. A run with no result, or whose
    /// steps both didn't run, leaves its block pending.
    fn settle(&mut self, slots: Vec<Slot>, jobs: &[Job], ran: Vec<Option<StepsRan>>) {
        let mut ran = ran.into_iter();
        for slot in slots {
            let (job, run) = match slot {
                Slot::Found(finding) => {
                    self.findings.push(finding);
                    continue;
                }
                Slot::Run(job) => (jobs.get(job), ran.next().flatten()),
            };
            let (Some(job), Some(StepsRan { format, lint })) = (job, run) else {
                continue;
            };
            let Some(record) = self.records.get_mut(job.record) else {
                continue;
            };
            let mut outcomes = Vec::new();
            if let (Some((verdict, findings)), Some(run)) = (format, job.format.as_ref()) {
                self.findings.extend(findings);
                record.steps.format = StepState::Ran;
                outcomes.push((verdict.state(), run.tool));
            }
            match (lint, job.lint.as_ref()) {
                (Some(Ok((verdict, findings))), Some(run)) => {
                    self.findings.extend(findings);
                    record.steps.lint = StepState::Ran;
                    outcomes.push((lint_state(&verdict), run.tool));
                }
                (Some(Err(reason)), Some(_)) => {
                    record.steps.lint = StepState::Unrunnable { reason };
                }
                _ => {}
            }
            let decided = outcomes
                .iter()
                .find(|(state, _)| matches!(state, BlockState::Failed(_)))
                .or_else(|| outcomes.first());
            if let Some((state, tool)) = decided {
                record.state = state.clone();
                record.reason =
                    record
                        .state
                        .reason(&record.steps, record.language.as_deref(), Some(tool.name));
            } else if let StepState::Unrunnable { reason } = record.steps.lint {
                record.reason = Some(reason.to_owned());
            }
        }
    }
}

/// The state a lint verdict alone puts a block in.
fn lint_state(verdict: &lint::Verdict) -> BlockState {
    match verdict {
        lint::Verdict::Clean => BlockState::Passed,
        lint::Verdict::Diagnostics(_) => BlockState::Failed(states::Failure::Lint),
        lint::Verdict::Failed(failed) => failed.state(),
    }
}

/// The most tool runs `check` keeps going at once, whatever the machine's parallelism, so a doc
/// with thousands of blocks can't start thousands of processes.
const MAX_WORKERS: usize = 8;

/// One step of [`Evaluation::look_up_tools`]'s output, kept in block order while the runs go in
/// parallel.
enum Slot {
    /// A finding known before any run.
    Found(Finding),
    /// The findings of the [`Job`] at this index.
    Run(usize),
}

/// The steps run on one pending block: its formatter, then its linter unless the formatter
/// found that the block doesn't parse.
struct Job {
    /// The position of the block's record.
    record: usize,
    /// The formatter, when one runs.
    format: Option<ToolRun>,
    /// The linter, when one runs.
    lint: Option<ToolRun>,
}

/// A tool found on `PATH` to run on a block.
struct ToolRun {
    /// The tool.
    tool: &'static Tool,
    /// Where it was found on `PATH`.
    program: PathBuf,
}

/// What a [`Job`]'s steps gave: each step's verdict and findings, `None` for a step that didn't
/// run.
struct StepsRan {
    /// The format step's.
    format: Option<(format::Verdict, Vec<Finding>)>,
    /// The lint step's, or why it couldn't run.
    lint: Option<LintRan>,
}

/// What a lint step gave: its verdict and findings, or why supdoc couldn't run it safely.
type LintRan = Result<(lint::Verdict, Vec<Finding>), &'static str>;

/// Why clippy didn't run on a block when [`runner::neutral_dir`] found no private working
/// directory: its metadata would have nowhere private to go.
const NO_PRIVATE_DIR: &str =
    "clippy needs a private working directory under `XDG_CACHE_HOME` or `HOME`, and there is none";

/// `work` applied to each of `items`, the results in `items`' order. The items are shared among
/// up to [`MAX_WORKERS`] scoped threads, no more than [`thread::available_parallelism`] allows,
/// each taking the next unclaimed item until none is left. A worker's panic is resumed here.
fn in_parallel<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = thread::available_parallelism()
        .map_or(1, NonZeroUsize::get)
        .min(MAX_WORKERS)
        .min(items.len());
    if workers <= 1 {
        return items.iter().map(work).collect();
    }
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, R)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            return done;
                        };
                        done.push((index, work(item)));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|panic| resume_unwind(panic)))
            .collect()
    });
    done.sort_unstable_by_key(|(index, _)| *index);
    done.into_iter().map(|(_, result)| result).collect()
}

/// What every Rust tool call in a run shares, read once from the root.
#[derive(Debug)]
struct RustSetup {
    /// The toolchain the root's `rust-toolchain(.toml)` pins.
    toolchain: Toolchain,
    /// `--config-path` for an in-root `rustfmt.toml`, or nothing.
    config: Vec<OsString>,
    /// The edition for a block that names none: the root `Cargo.toml`'s, else 2024.
    edition: Edition,
    /// The root `Cargo.toml`'s lint levels as clippy flags.
    lints: Vec<OsString>,
    /// `CLIPPY_CONF_DIR` for an in-root clippy config, or nothing.
    clippy_conf: Option<OsString>,
}

impl RustSetup {
    /// Reads `root`'s toolchain file, `Cargo.toml` and finds its `rustfmt.toml` and clippy
    /// config, each only inside `root`.
    ///
    /// Effect: reads file metadata, resolves symlinks, and reads the toolchain file and
    /// `Cargo.toml`.
    fn read(root: &Path) -> Self {
        Self {
            toolchain: toolchain::read(root),
            config: rust::config_args(root),
            edition: toolchain::cargo_edition(root).unwrap_or(Edition::E2024),
            lints: toolchain::cargo_lints(root),
            clippy_conf: rust::clippy_conf_dir(root),
        }
    }

    /// The edition for `record`'s block: its own attribute's, else the project's.
    fn edition_of(&self, record: &BlockRecord) -> Edition {
        record.edition.unwrap_or(self.edition)
    }
}

/// What every tool run in one `check` shares.
#[derive(Debug)]
struct Session<'run> {
    /// The environment tools get.
    env: &'run ToolEnv,
    /// The project root.
    root: &'run Path,
    /// How long one run may take. `None` is no limit.
    timeout: Option<Duration>,
    /// The result cache, when there is one.
    cache: Option<&'run Cache>,
}

/// Runs `tool`, found at `program`, on `record`'s block from `session`'s root with its env, under
/// its timeout, or reads the run from its cache when the key is there,
/// adding `rust`'s config arguments and toolchain variables for a Rust tool:
/// its verdict and findings, an unformatted block's error carrying the fix to the formatter's
/// output, or followed by a `code-block-fix-unsafe` warning when [`format::safe_fix`] finds that
/// output could end the block early. `None` when the block's language has no format step yet or its input
/// couldn't be read, so it stays pending. A run whose verdict [`format::Verdict::cacheable`]
/// allows is stored in the cache.
///
/// Effect: reads the cache, runs the tool, killing it at the timeout, and writes the cache.
fn run_formatter(
    tool: &Tool,
    program: &Path,
    record: &BlockRecord,
    session: &Session<'_>,
    rust: Option<&RustSetup>,
) -> Option<(format::Verdict, Vec<Finding>)> {
    let Session { root, timeout, .. } = *session;
    let pending = record.pending.as_ref()?;
    let input = pending.input.as_str();
    let doc = root.join(record.at.file.as_str());
    let edition = record
        .edition
        .or_else(|| rust.map(|rust| rust.edition))
        .unwrap_or(Edition::E2024);
    let mut args = format::args(tool, &doc, record.index, edition)?;
    if tool.language == Language::Shell {
        args.extend(shell::dialect_args(record.dialect));
    }
    if let Some(rust) = rust {
        args.extend(rust.config.iter().cloned());
    }
    let vars = rust.map(|rust| rust.toolchain.env()).unwrap_or_default();
    let extra: Vec<(&str, &OsStr)> = vars
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let run = cache::Run {
        tool,
        program,
        args: &args,
        extra: &extra,
        input,
        doc: &doc,
    };
    let verdict = cached_run(
        &run,
        session,
        |call| runner::run(call, session.env, session.root),
        |result| format::judge(tool, input, result),
        format::Verdict::cacheable,
    );
    let at = match &verdict {
        format::Verdict::ParseFailed {
            line: Some(line), ..
        } => rust::LineMap::of(&pending.block, pending.doc.text())
            .and_then(|map| map.doc_line(*line).cloned())
            .unwrap_or_else(|| record.at.clone()),
        _ => record.at.clone(),
    };
    let finding = format::finding(&verdict, tool, &at, timeout);
    let findings = match (&verdict, finding) {
        (format::Verdict::Unformatted(output), Some(unformatted)) => format::fixed(
            unformatted,
            &pending.block,
            &pending.doc,
            output,
            tool,
            pending.hidden.as_ref(),
        ),
        (_, finding) => finding.into_iter().collect(),
    };
    Some((verdict, findings))
}

/// Runs `tool`, a linter found at `program`, on `record`'s block as [`cached_run`] does: its
/// verdict and findings, one `code-block-lint` error per diagnostic or the tool's failure. `None`
/// when the block's language has no lint step yet or its input couldn't be read. A Rust block
/// goes to clippy through [`run_clippy`] with `rust`'s setup.
///
/// Effect: reads the cache, runs the tool, killing it at the timeout, and writes the cache.
fn run_linter(
    tool: &Tool,
    program: &Path,
    record: &BlockRecord,
    session: &Session<'_>,
    rust: Option<&RustSetup>,
) -> Option<LintRan> {
    let pending = record.pending.as_ref()?;
    let doc = session.root.join(record.at.file.as_str());
    let mut args = lint::args(tool, &doc, record.index)?;
    if let Some(rust) = rust {
        return Some(run_clippy(tool, program, record, pending, session, rust));
    }
    if tool.language == Language::Shell {
        args = shell::lint_dialect_args(record.dialect)
            .into_iter()
            .chain(args)
            .collect();
    }
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let run = cache::Run {
        tool,
        program,
        args: &args,
        extra: &[],
        input: &pending.input,
        doc: &doc,
    };
    let verdict = cached_run(
        &run,
        session,
        |call| runner::run(call, session.env, session.root),
        |result| match tool.language {
            Language::Shell => shell::judge(tool, &pending.input, result),
            Language::Python => lint::judge(tool, result),
            Language::Rust => rust::lint_verdict(result),
        },
        lint::Verdict::cacheable,
    );
    let findings = lint::findings(
        &verdict,
        tool,
        &pending.block,
        pending.doc.text(),
        |_| 0,
        session.timeout,
    );
    Some(Ok((verdict, findings)))
}

/// Runs `tool`, clippy found at `program`, on `record`'s Rust block alone, read as `pending`, as [`run_linter`]
/// does: `clippy-driver` with [`rust::clippy_args`] for the block's edition and `rust`'s project
/// lints, `rust`'s toolchain variables and `CLIPPY_CONF_DIR`, and nothing else from the project.
/// Its metadata goes to a fresh [`rust::EmitDir`], removed after the run whatever its outcome. A
/// diagnostic on a hidden line lands past its `# `.
///
/// # Errors
///
/// [`NO_PRIVATE_DIR`] when [`runner::neutral_dir`] falls back to `/`, so clippy isn't run.
///
/// Effect: reads the cache, creates and removes the emit directory, runs clippy, killing it at
/// the timeout, and writes the cache.
fn run_clippy(
    tool: &Tool,
    program: &Path,
    record: &BlockRecord,
    pending: &Pending,
    session: &Session<'_>,
    rust: &RustSetup,
) -> LintRan {
    let neutral = runner::neutral_dir(session.env, session.root);
    if neutral == Path::new("/") {
        return Err(NO_PRIVATE_DIR);
    }
    let doc = session.root.join(record.at.file.as_str());
    let edition = rust.edition_of(record);
    let key_args = rust::clippy_args(edition, None, &rust.lints);
    let key_args: Vec<&OsStr> = key_args.iter().map(OsString::as_os_str).collect();
    let mut vars = rust.toolchain.env();
    vars.extend(
        rust.clippy_conf
            .iter()
            .map(|dir| ("CLIPPY_CONF_DIR", dir.clone())),
    );
    let extra: Vec<(&str, &OsStr)> = vars
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();
    let run = cache::Run {
        tool,
        program,
        args: &key_args,
        extra: &extra,
        input: &pending.input,
        doc: &doc,
    };
    let spawn = |call: &Call<'_>| {
        let emit = rust::EmitDir::create(&neutral).ok_or_else(|| {
            runner::Failure::Spawn(std::io::Error::other(
                "supdoc couldn't create a private directory for its metadata",
            ))
        })?;
        let args = rust::clippy_args(edition, Some(&emit.file()), &rust.lints);
        let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        let result = runner::run(
            &Call {
                args: &args,
                ..*call
            },
            session.env,
            session.root,
        );
        drop(emit);
        result
    };
    let verdict = cached_run(
        &run,
        session,
        spawn,
        rust::lint_verdict,
        lint::Verdict::cacheable,
    );
    let hidden = pending.hidden.as_ref();
    let findings = lint::findings(
        &verdict,
        tool,
        &pending.block,
        pending.doc.text(),
        |line| hidden.map_or(0, |hidden| hidden.removed(line)),
        session.timeout,
    );
    Ok((verdict, findings))
}

/// `judge` applied to `run`, read from `session`'s cache when its key is there, otherwise run
/// through `spawn` (which runs the call from the root with the session's env) under its timeout,
/// and stored in the cache when `cacheable` allows the verdict.
///
/// Effect: reads the cache, runs the tool through `spawn`, and writes the cache.
fn cached_run<V>(
    run: &cache::Run<'_>,
    session: &Session<'_>,
    spawn: impl FnOnce(&Call<'_>) -> Result<Ran, runner::Failure>,
    judge: impl FnOnce(Result<Ran, runner::Failure>) -> V,
    cacheable: impl FnOnce(&V) -> bool,
) -> V {
    let Session {
        root,
        timeout,
        cache,
        ..
    } = *session;
    let key = cache.and_then(|_| Cache::key(run, root));
    if let Some(ran) = cache.zip(key).and_then(|(cache, key)| cache.get(key)) {
        return judge(Ok(ran));
    }
    let call = Call {
        program: run.program,
        name: run.tool.binary,
        args: run.args,
        extra: run.extra,
        stdin: run.input.as_bytes(),
        timeout,
    };
    let result = spawn(&call);
    let stored = result.as_ref().ok().cloned();
    let verdict = judge(result);
    if let (Some(cache), Some(key), Some(ran)) = (cache, key, stored)
        && cacheable(&verdict)
    {
        cache.put(key, &ran);
    }
    verdict
}

/// Each resolved `uses(...)` value of `doc_name` with its state against `recorded`, then each
/// `unresolved` one as missing.
fn use_states(
    resolved: &[Written],
    unresolved: Vec<String>,
    recorded: &BTreeMap<(&str, String), Hash>,
    doc_name: &str,
) -> Vec<(String, UseState)> {
    let found = resolved.iter().map(|value| {
        let hash = recorded.get(&(doc_name, value.target.clone()));
        (value.target.clone(), UseState::of(value, hash))
    });
    let missing = unresolved
        .into_iter()
        .map(|value| (value, UseState::Missing));
    found.chain(missing).collect()
}

/// Each use the lockfile records, keyed by its doc and target.
fn recorded_by_target(lock: &Lockfile) -> BTreeMap<(&str, String), Hash> {
    lock.recorded_uses()
        .map(|(doc, target, hash)| ((doc, target), hash))
        .collect()
}

/// The edition `block`'s own `edition20xx` attribute names, when it has one.
fn block_edition(block: &CodeBlock) -> Option<Edition> {
    let info = block.info.as_ref()?;
    rust::attribute_edition(&info.parsed().attributes)
}

/// Counts `records`' states and steps.
fn tally(records: &[BlockRecord]) -> CodeBlockCounts {
    CodeBlockCounts::tally(records.iter().map(|record| (&record.state, &record.steps)))
}

/// The formatter `code_blocks` selects for `language`: the named one, or the default. `None` when
/// the language has none.
fn selected(code_blocks: &CodeBlocks, language: Language) -> Option<&'static Tool> {
    match code_blocks.formatter(language) {
        Some(Choice::Named(name)) => TOOLS.iter().find(|tool| {
            tool.language == language && tool.step == Step::Format && tool.name == name
        }),
        Some(Choice::Off) | None => formatter(language),
    }
}

/// Why `record`'s Rust block isn't run under `rust`, when it isn't: the root's toolchain file
/// was refused (adding its one warning to `slots` unless `warned`), or the block reads at compile
/// time or is too large to scan (adding that warning). When the block `compiles`, because
/// clippy will lint it, [`rust::before_compiling`]'s stricter rules apply, which also refuse an
/// outline module and an attribute off the allowlist. With the state, a reason naming that
/// attribute or module when there is one, which replaces the state's own.
fn rust_refusal(
    record: &BlockRecord,
    rust: &RustSetup,
    compiles: bool,
    warned: &mut bool,
    slots: &mut Vec<Slot>,
) -> Option<(NotChecked, Option<String>)> {
    if let Toolchain::Refused { refusal, file } = rust.toolchain {
        if !*warned {
            *warned = true;
            slots.push(Slot::Found(toolchain_warning(refusal, file, &record.at)));
        }
        return Some((NotChecked::ToolchainPath, None));
    }
    let scan = record.pending.as_ref().map_or(Scan::Clean, |pending| {
        if compiles {
            rust::before_compiling(&pending.input)
        } else {
            rust::compile_time_read(&pending.input)
        }
    });
    slots.extend(scan_warning(&scan, &record.at).map(Slot::Found));
    match scan {
        Scan::Clean => None,
        Scan::Read(rust::Found::OutlineModule(name)) => Some((
            NotChecked::OutlineModule,
            name.map(|name| format!("outline module `{name}`")),
        )),
        Scan::Read(rust::Found::Attribute(name)) if !name.is_empty() => Some((
            NotChecked::CompileTimeRead,
            Some(format!("compile-time read: `{name}` attribute")),
        )),
        Scan::Read(_) => Some((NotChecked::CompileTimeRead, None)),
        Scan::TooLarge => Some((NotChecked::TooLargeToScan, None)),
    }
}

/// Looks up each of `tools` on `env`'s `PATH` (once per tool, remembered in `looked_up`), adding
/// to `slots` the warning for one not found outside `root`, at `at`. Each found tool's run, in
/// `tools`' order, and the state for the first one that wasn't, with that tool.
///
/// Effect: reads file metadata along `PATH` and resolves symlinks.
fn found_runs(
    tools: [Option<&'static Tool>; 2],
    looked_up: &mut Vec<(&'static str, Found)>,
    slots: &mut Vec<Slot>,
    env: &ToolEnv,
    root: &Path,
    at: &Location,
) -> ([Option<ToolRun>; 2], Option<(BlockState, &'static Tool)>) {
    let mut runs = [None, None];
    let mut refused = None;
    for (run, tool) in runs.iter_mut().zip(tools) {
        let Some(tool) = tool else {
            continue;
        };
        let found = if let Some((_, found)) = looked_up.iter().find(|(name, _)| *name == tool.name)
        {
            found.clone()
        } else {
            let found = find(tool.binary, env, root);
            slots.extend(tool_warning(tool, &found, at).map(Slot::Found));
            looked_up.push((tool.name, found.clone()));
            found
        };
        let state = match found {
            Found::OnPath(program) => {
                *run = Some(ToolRun { tool, program });
                continue;
            }
            Found::Missing => BlockState::ToolMissing,
            Found::InsideRoot(_) => BlockState::NotChecked(NotChecked::ToolInsideRoot),
        };
        let _first = refused.get_or_insert((state, tool));
    }
    (runs, refused)
}

/// The formatter and linter `code_blocks` selects for a block in `language` whose `steps` are
/// due: each `None` when its step isn't due or the language has none.
fn selected_tools(
    code_blocks: &CodeBlocks,
    steps: &Steps,
    language: Option<Language>,
) -> (Option<&'static Tool>, Option<&'static Tool>) {
    let language = |step: &StepState| (*step == StepState::NotRun).then_some(language).flatten();
    (
        language(&steps.format).and_then(|language| selected(code_blocks, language)),
        language(&steps.lint).and_then(|language| selected_linter(code_blocks, language)),
    )
}

/// The linter `code_blocks` selects for `language`: the named one, or the default. `None` when
/// the language has none.
fn selected_linter(code_blocks: &CodeBlocks, language: Language) -> Option<&'static Tool> {
    match code_blocks.linter(language) {
        Some(Choice::Named(name)) => TOOLS
            .iter()
            .find(|tool| tool.language == language && tool.step == Step::Lint && tool.name == name),
        Some(Choice::Off) | None => linter(language),
    }
}

/// The one warning for `tool`, found as `found`, at `at`, the first block that needs it. `None`
/// when it is on `PATH` outside the root.
fn tool_warning(tool: &Tool, found: &Found, at: &Location) -> Option<Finding> {
    let name = tool.name;
    let finding = match found {
        Found::OnPath(_) => return None,
        Found::Missing => Finding::new(
            TOOL_MISSING,
            Severity::Warning,
            format!("`{name}` isn't on PATH, so its blocks aren't checked"),
        )
        .with_help(format!("install it with `{}`", tool.install)),
        Found::InsideRoot(real) => Finding::new(
            NOT_CHECKED,
            Severity::Warning,
            format!(
                "`{name}` on PATH is `{}`, inside the checkout, so its blocks aren't checked",
                real.display()
            ),
        )
        .with_help(format!(
            "put a `{}` from outside the project first on PATH",
            tool.binary
        )),
    };
    Some(finding.with_location(at.clone()))
}

/// The one `code-block-not-checked` warning, at `at`, for the root's toolchain `file` refused for
/// `refusal`, which leaves every Rust block unchecked.
fn toolchain_warning(refusal: Refusal, file: &str, at: &Location) -> Finding {
    let (why, help) = match refusal {
        Refusal::Path => (
            "names a toolchain `path`, which would run a binary the checkout chose".to_owned(),
            format!("pin a channel such as `stable` or `1.85.0` in `{file}` instead of a `path`"),
        ),
        Refusal::InvalidChannel => (
            "names a channel supdoc doesn't accept, or doesn't parse".to_owned(),
            format!(
                "in `{file}`, name `stable`, `beta`, `nightly` or a version such as `1.85.0`, optionally with `-YYYY-MM-DD`"
            ),
        ),
        Refusal::Outside => (
            "is a link that resolves outside the project".to_owned(),
            format!("replace `{file}` with a regular file inside the project"),
        ),
        Refusal::Unreadable => (
            "couldn't be read as a regular UTF-8 file of at most 64 KiB".to_owned(),
            format!("make `{file}` a readable UTF-8 text file of at most 64 KiB"),
        ),
    };
    Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        format!("the project's `{file}` {why}, so its Rust blocks aren't checked"),
    )
    .with_help(help)
    .with_location(at.clone())
}

/// The `code-block-not-checked` warning, at `at`, for a Rust block `scan` refused: one holding a
/// compile-time read such as `include_str!` or something that could hide one, or one too large
/// to scan. `None` for a clean block.
fn scan_warning(scan: &Scan, at: &Location) -> Option<Finding> {
    let finding = match scan {
        Scan::Clean => return None,
        Scan::Read(found) => Finding::new(
            NOT_CHECKED,
            Severity::Warning,
            format!("the block {}, so it isn't checked", found.why()),
        )
        .with_help(found.help()),
        Scan::TooLarge => Finding::new(
            NOT_CHECKED,
            Severity::Warning,
            format!(
                "the block is over {} KiB, too large to scan for compile-time reads, so it isn't checked",
                rust::MAX_SCAN_BYTES / 1024
            ),
        )
        .with_help("split the block, or mark it `ignore(<why>)`"),
    };
    Some(finding.with_location(at.clone()))
}

/// The text of `doc`, relative to the project root, as it is on disk now. `None` when it can't
/// be read; its blocks then get no stale finding.
///
/// Effect: reads the doc.
fn read_doc(config: &Config, doc: &str) -> Option<String> {
    std::fs::read_to_string(config.root().join_path(Path::new(doc))).ok()
}

/// What a block's `from(...)` and `uses(...)` values resolved to.
struct Sources {
    /// One `code-block-source-missing` error for each value that doesn't resolve.
    missing: Vec<Finding>,
    /// Every `from(...)` part's text, in written order, when the block has one and each
    /// resolves.
    parts: Option<Vec<SourceText>>,
    /// Every `uses(...)` value that resolves, with its target and current hash.
    uses: Vec<Written>,
    /// Whether some `uses(...)` value doesn't resolve.
    use_missing: bool,
    /// Each `uses(...)` value that doesn't resolve, as written.
    unresolved: Vec<String>,
    /// Whether some `from(...)` value doesn't resolve.
    from_missing: bool,
}

/// One `code-block-unknown-attribute` warning at `info` for a language word holding commas, and
/// one for each attribute outside the closed list.
fn unknown_attributes(info: &Info) -> Vec<Finding> {
    let parsed = info.parsed();
    let comma = parsed.spaced_language().map(|spaced| {
        Finding::new(
            UNKNOWN_ATTRIBUTE,
            Severity::Warning,
            format!(
                "the info string `{}` joins its attributes with commas",
                parsed.language
            ),
        )
        .with_location(info.location.clone())
        .with_help(format!(
            "separate the language and each attribute with spaces: write `{spaced}`"
        ))
    });
    let unknown = parsed
        .attributes
        .iter()
        .filter(|attribute| !is_known_attribute(attribute))
        .map(|attribute| {
            Finding::new(
                UNKNOWN_ATTRIBUTE,
                Severity::Warning,
                format!(
                    "the `{}` block's attribute `{attribute}` isn't one supdoc knows",
                    parsed.language
                ),
            )
            .with_location(info.location.clone())
            .with_help(match parenthesized_spelling(attribute) {
                Some(spelling) => format!("write `{attribute}` as `{spelling}`"),
                None => format!("remove `{attribute}` or correct it to one of {KNOWN_ATTRIBUTES}"),
            })
        });
    comma.into_iter().chain(unknown).collect()
}

/// One `code-block-ignore-reason` warning at `info` for each `ignore`, `no_format` or `no_lint`
/// without a reason. The attribute still takes effect.
fn missing_reasons(info: &Info) -> Vec<Finding> {
    let parsed = info.parsed();
    parsed
        .attributes
        .iter()
        .filter_map(|attribute| reasoned(attribute))
        .filter(|found| found.reason.is_none())
        .map(|found| {
            let name = found.name;
            Finding::new(
                IGNORE_REASON,
                Severity::Warning,
                format!("the `{}` block's `{name}` doesn't say why", parsed.language),
            )
            .with_location(info.location.clone())
            .with_help(format!("write it as `{name}(<why>)`"))
        })
        .collect()
}

/// Reads every `from(...)` and `uses(...)` value in `info`: one `code-block-source-missing`
/// error at each that doesn't resolve to a contained, readable file or symbol, the `from(...)`
/// parts' text when all of them resolve, and each resolved `uses(...)` target with its hash.
///
/// # Errors
///
/// [`CartridgeLoadError`] when a source's built-in cartridge doesn't load.
fn sources(
    config: &Config,
    languages: &Languages,
    info: &Info,
) -> Result<Sources, CartridgeLoadError> {
    let doc = info.location.file.as_str();
    let mut missing = Vec::new();
    let mut parts = Vec::new();
    let mut uses = Vec::new();
    let mut part_missing = false;
    let mut use_missing = false;
    let mut unresolved_uses = Vec::new();
    for attribute in source_attributes(&info.text, info.in_rust_doc()) {
        let at = value_location(info, attribute.range);
        let result = match attribute.kind {
            SourceKind::From => {
                source::read(config, languages, doc, attribute.kind, attribute.value)
                    .map(|text| parts.push(text))
            }
            SourceKind::Uses => {
                source::read_use(config, languages, doc, attribute.value).map(|found| {
                    uses.extend(at.clone().map(|at| Written {
                        at,
                        target: found.target,
                        hash: found.hash,
                    }));
                })
            }
        };
        let unresolved = match result {
            Ok(()) => continue,
            Err(Unresolved::CartridgeLoad(error)) => return Err(error),
            Err(Unresolved::Missing(unresolved)) => unresolved,
        };
        part_missing |= attribute.kind == SourceKind::From;
        use_missing |= attribute.kind == SourceKind::Uses;
        if attribute.kind == SourceKind::Uses {
            unresolved_uses.push(attribute.value.to_owned());
        }
        missing.extend(at.map(|at| unresolved.finding(at)));
    }
    let parts = (!part_missing && !parts.is_empty()).then_some(parts);
    Ok(Sources {
        missing,
        parts,
        uses,
        use_missing,
        unresolved: unresolved_uses,
        from_missing: part_missing,
    })
}

/// Where `range`, a byte range of `info`'s text, lies in the doc. An info string is one line, so
/// the value shares its line.
pub(super) fn value_location(info: &Info, range: Range<usize>) -> Option<Location> {
    let before = info.text.get(..range.start)?.chars().count();
    let start = info.location.range.start + range.start;
    Some(Location {
        file: info.location.file.clone(),
        range: start..info.location.range.start + range.end,
        line: info.location.line,
        column: info.location.column + u32::try_from(before).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::{MAX_WORKERS, in_parallel};

    #[test]
    fn in_parallel_keeps_the_items_order_when_later_items_finish_first() {
        let items: Vec<u64> = (0..12).collect();

        let results = in_parallel(&items, |item| {
            std::thread::sleep(Duration::from_millis(5 * (12 - item)));
            item * 10
        });

        let expected: Vec<u64> = items.iter().map(|item| item * 10).collect();
        assert_eq!(results, expected);
    }

    #[test]
    fn in_parallel_runs_no_more_than_max_workers_at_once() {
        let items: Vec<usize> = (0..MAX_WORKERS * 4).collect();
        let running = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);

        let results = in_parallel(&items, |item| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            let _before = most.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(10));
            let _before = running.fetch_sub(1, Ordering::SeqCst);
            *item
        });

        assert_eq!(
            results, items,
            "every item should have one result, in order"
        );
        let most = most.load(Ordering::SeqCst);
        assert!(
            (1..=MAX_WORKERS).contains(&most),
            "{most} runs went at once, past the cap of {MAX_WORKERS}"
        );
    }

    #[test]
    fn in_parallel_of_nothing_is_empty() {
        let results = in_parallel(&[] as &[u8], |item| *item);

        assert!(results.is_empty(), "{results:?}");
    }
}
