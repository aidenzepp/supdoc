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
use crate::config::code_blocks::{CodeBlocks, Dialect, Edition, Language};
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
    KNOWN_ATTRIBUTES, SourceKind, StepValue, is_known_attribute, parenthesized_spelling, reasoned,
    replacement_for_removed, source_attributes, step_attribute,
};
use super::lint;
use super::runner::{self, Call, Ran};
use super::rust::{self, Scan};
use super::source::{self, SourceText, Unresolved};
use super::states::{self, Excerpt, NotChecked, StepState, Steps, state_of, tag_of};
use super::toolchain::{self, Refusal, Toolchain};
use super::tools::{self, Found, Invocation, Step, Tier, ToolCartridge, ToolEnv, find};
use super::uses::{self, UseState, Written};

pub(crate) use super::states::{BlockState, CodeBlockCounts};

/// The code for an info-string attribute supdoc doesn't know.
const UNKNOWN_ATTRIBUTE: &str = "code-block-unknown-attribute";

/// The code for an `ignore`, `lint(off)` or `format(off)` written without a reason.
const MISSING_REASON: &str = "code-block-missing-reason";

/// The code for a selected tool that isn't on `PATH`.
const TOOL_MISSING: &str = "code-block-tool-missing";

/// The code for blocks a hardening rule refused to check.
const NOT_CHECKED: &str = "code-block-not-checked";

/// The code for a step held because the operator hasn't granted the checkout trust.
const NOT_TRUSTED: &str = "code-block-not-trusted";

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
    tool: Option<&'static ToolCartridge>,
    /// The linter the config selects for it, while the block is pending and its lint step due.
    linter: Option<&'static ToolCartridge>,
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
    /// What its tools read: its body as written.
    input: String,
}

impl Pending {
    /// `block`, found in `doc`, ready for its steps. `None` when its body isn't inside `doc`.
    fn of(block: &CodeBlock, doc: &Arc<FixDoc>) -> Option<Self> {
        Some(Self {
            input: format::input(block, doc.text())?,
            block: block.clone(),
            doc: Arc::clone(doc),
        })
    }
}

/// The findings for `blocks`, in block order, reading each `from(...)` and `uses(...)` source
/// with `languages`, and one `code-block-stale` error for each `from(...)` block whose body
/// differs from its parts, followed by a `code-block-fix-unsafe` warning when
/// [`format::safe_fix`] refuses its fix. Then the `uses(...)` findings against `lock`'s
/// `[code-blocks]` section, judging a doc's recorded targets obsolete only when `in_scope` holds for it and none
/// of its `uses(...)` values fails to resolve. Their help commands name each doc as `paths`
/// shows it, relative to the current directory. With them, each block's [`BlockState`], counted.
/// Nothing when `config` leaves `[features.code-blocks]` off.
///
/// Each due step's tool goes through the tier gate ([`tier_gate`]): `granted` is
/// [`Tier::Project`] when the operator trusts the checkout, else [`Tier::Static`]. The first
/// block with a step held for want of trust gets the run's one `code-block-not-trusted` warning,
/// after the other findings.
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
    granted: Tier,
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
            findings.extend(step_values(block, info, code_blocks));
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
                reread(&mut doc, file, read);
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
        let mut steps = Steps::of(block, &state, code_blocks);
        let checked = language.as_deref().and_then(Language::from_tag);
        let (tool, linter) = selected_tools(code_blocks, &mut steps, checked, &state, granted);
        let pending = (state == BlockState::NotChecked(NotChecked::Pending))
            .then(|| {
                let file = block.fence.file.as_str();
                reread(&mut doc, file, read);
                doc.as_ref()?
                    .1
                    .as_ref()
                    .and_then(|text| Pending::of(block, text))
            })
            .flatten();
        records.push(BlockRecord {
            at: block.fence.clone(),
            reason: first_reason(&state, &steps, language.as_deref(), tool),
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
    findings.extend(not_trusted(&records));
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
            let linter = record.linter;
            let Some(tool) = record.tool.or(linter) else {
                continue;
            };
            let rust = (tool.language == Language::Rust)
                .then(|| &*rust_setup.get_or_insert_with(|| RustSetup::read(root)));
            if let Some(rust) = rust {
                let compiles = linter.is_some_and(|linter| linter.tier == Tier::Project);
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
                let ran = [format.is_some(), lint.is_some()];
                refuse_unfound(&mut record.steps, [record.tool, linter], ran, &looked_up);
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
                record.reason = record.state.reason(
                    &record.steps,
                    record.language.as_deref(),
                    Some(tool.binary),
                );
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
        self.settle_pending();
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
    /// steps both didn't run, leaves its block to [`Self::settle_pending`].
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
            }
        }
    }

    /// Settles each block still pending once its runs are in, so no block reaches output
    /// pending: one with a tool selected whose text couldn't be read from its doc is
    /// [`NotChecked::InputUnreadable`]; any other, none of whose steps ran, is
    /// [`NotChecked::NoStepRan`], with its steps' reasons.
    fn settle_pending(&mut self) {
        for record in &mut self.records {
            if record.state != BlockState::NotChecked(NotChecked::Pending) {
                continue;
            }
            let tool = record.tool.or(record.linter);
            let unreadable = tool.is_some() && record.pending.is_none();
            record.state = BlockState::NotChecked(if unreadable {
                NotChecked::InputUnreadable
            } else {
                NotChecked::NoStepRan
            });
            record.reason = record.state.reason(
                &record.steps,
                record.language.as_deref(),
                tool.map(|tool| tool.name),
            );
        }
    }
}

/// `state`'s reason as [`evaluate`] first records it, for `steps` and the formatter `tool`.
/// `None` while the block is pending: [`Evaluation::settle_pending`] gives it one.
fn first_reason(
    state: &BlockState,
    steps: &Steps,
    language: Option<&str>,
    tool: Option<&ToolCartridge>,
) -> Option<String> {
    (*state != BlockState::NotChecked(NotChecked::Pending))
        .then(|| state.reason(steps, language, tool.map(|tool| tool.name)))
        .flatten()
}

/// Marks each of `steps`, format then lint, whose tool in `tools` is selected but didn't run
/// (per `ran`) as [`StepState::ToolRefused`], missing or inside the checkout by what `looked_up`
/// found for its binary.
fn refuse_unfound(
    steps: &mut Steps,
    tools: [Option<&'static ToolCartridge>; 2],
    ran: [bool; 2],
    looked_up: &[(&'static str, Found)],
) {
    for ((step, tool), ran) in [&mut steps.format, &mut steps.lint]
        .into_iter()
        .zip(tools)
        .zip(ran)
    {
        let Some(tool) = tool.filter(|_| !ran) else {
            continue;
        };
        let inside_root = looked_up
            .iter()
            .any(|(binary, found)| *binary == tool.binary && matches!(found, Found::InsideRoot(_)));
        *step = StepState::ToolRefused {
            binary: tool.binary,
            inside_root,
        };
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
    tool: &'static ToolCartridge,
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

/// Why a step whose cartridge is above the effective tier doesn't run when the operator hasn't
/// granted trust: clippy compiles the block, so it belongs to the project tier.
pub(super) const CLIPPY_NEEDS_TRUST: &str =
    "clippy runs only in the project tier, which needs `supdoc trust`";

/// Why such a step doesn't run when the project itself lowered the tier.
const TIER_LOWERED: &str = "`tier = \"static\"` in `[features.code-blocks]`";

/// Why a project-tier step doesn't run even in a trusted checkout: clippy waits for a
/// re-audit of the scanner in front of it.
const CLIPPY_AWAITS_AUDIT: &str = "clippy doesn't run yet, even in a trusted project";

/// `tool` as the tiers allow it: `None` for a cartridge above the effective tier, the lower of
/// `granted` (what the operator trusts the checkout with) and `ceiling` (the project's `tier`).
/// Its `step` then doesn't run, with [`TIER_LOWERED`] as its reason when the ceiling holds it,
/// else [`CLIPPY_NEEDS_TRUST`]. A project-tier cartridge the effective tier allows still doesn't
/// run, with [`CLIPPY_AWAITS_AUDIT`], until a later release. The gate sits where the tool is chosen,
/// before any scan, cache key or spawn.
fn tier_gate(
    tool: Option<&'static ToolCartridge>,
    step: &mut StepState,
    granted: Tier,
    ceiling: Tier,
) -> Option<&'static ToolCartridge> {
    let found = tool?;
    let effective = granted.min(ceiling);
    let reason = if found.tier > effective {
        if found.tier > ceiling {
            TIER_LOWERED
        } else {
            CLIPPY_NEEDS_TRUST
        }
    } else if found.tier > Tier::Static {
        CLIPPY_AWAITS_AUDIT
    } else {
        return tool;
    };
    *step = StepState::Unrunnable { reason };
    None
}

/// The run's one `code-block-not-trusted` warning, at the first of `records` with a step held
/// because the operator hasn't granted trust. `None` when no step was held for that.
fn not_trusted(records: &[BlockRecord]) -> Option<Finding> {
    let at = &records
        .iter()
        .find(|record| record.steps.held_for_trust())?
        .at;
    let finding = Finding::new(
        NOT_TRUSTED,
        Severity::Warning,
        "clippy didn't run on Rust blocks: it compiles them, and this checkout isn't trusted",
    )
    .with_help(
        "run `supdoc trust` to trust this checkout, or set `SUPDOC_TRUST=1` for one run (as in \
         CI); to stop asking, set `tier = \"static\"` in `[features.code-blocks]` or \
         `lint = \"off\"` in `[features.code-blocks.rust.clippy]`",
    );
    Some(finding.with_location(at.clone()))
}

/// Points `doc` at `file`, read with `read`, unless it already holds it.
fn reread<'doc>(
    doc: &mut Option<(&'doc str, Option<Arc<FixDoc>>)>,
    file: &'doc str,
    read: impl Fn(&str) -> Option<Arc<FixDoc>>,
) {
    if doc.as_ref().is_none_or(|(held, _)| *held != file) {
        *doc = Some((file, read(file)));
    }
}

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
/// its timeout, or reads the run from its cache when the key is there, adding `rust`'s config
/// arguments and toolchain variables for a Rust tool: its verdict and findings, an unformatted
/// block's error carrying the fix to the formatter's output, or followed by a
/// `code-block-fix-unsafe` warning when [`format::safe_fix`] finds that output could end the
/// block early. `None` when its input couldn't be read, so it stays pending. A run whose verdict
/// [`format::Verdict::cacheable`] allows is stored in the cache.
///
/// Effect: reads the cache, runs the tool, killing it at the timeout, and writes the cache.
fn run_formatter(
    tool: &ToolCartridge,
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
    let args = tool.args(&Invocation {
        doc: &doc,
        index: record.index,
        edition,
        dialect: record.dialect,
        project: rust.map_or(&[], |rust| rust.config.as_slice()),
        emit: None,
    });
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
        (format::Verdict::Unformatted(output), Some(unformatted)) => {
            format::fixed(unformatted, &pending.block, &pending.doc, output, tool)
        }
        (_, finding) => finding.into_iter().collect(),
    };
    Some((verdict, findings))
}

/// Runs `tool`, a linter found at `program`, on `record`'s block as [`cached_run`] does: its
/// verdict and findings, one `code-block-lint` error per diagnostic or the tool's failure. `None`
/// when its input couldn't be read. A Rust block goes to clippy through [`run_clippy`] with
/// `rust`'s setup.
///
/// Effect: reads the cache, runs the tool, killing it at the timeout, and writes the cache.
fn run_linter(
    tool: &ToolCartridge,
    program: &Path,
    record: &BlockRecord,
    session: &Session<'_>,
    rust: Option<&RustSetup>,
) -> Option<LintRan> {
    let pending = record.pending.as_ref()?;
    if let Some(rust) = rust {
        return Some(run_clippy(tool, program, record, pending, session, rust));
    }
    let doc = session.root.join(record.at.file.as_str());
    let args = tool.args(&Invocation {
        doc: &doc,
        index: record.index,
        edition: record.edition.unwrap_or(Edition::E2024),
        dialect: record.dialect,
        project: &[],
        emit: None,
    });
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
        |result| tool.lint_verdict(&pending.input, result),
        lint::Verdict::cacheable,
    );
    let findings = lint::findings(
        &verdict,
        tool,
        &pending.block,
        pending.doc.text(),
        session.timeout,
    );
    Some(Ok((verdict, findings)))
}

/// Runs `tool`, clippy found at `program`, on `record`'s Rust block alone, read as `pending`, as
/// [`run_linter`] does: `clippy-driver` with its cartridge's arguments for the block's edition
/// and `rust`'s project lints, `rust`'s toolchain variables and `CLIPPY_CONF_DIR`, and nothing
/// else from the project. Its metadata goes to a fresh [`rust::EmitDir`], removed after the run whatever its outcome.
///
/// # Errors
///
/// [`NO_PRIVATE_DIR`] when [`runner::neutral_dir`] falls back to `/`, so clippy isn't run.
///
/// Effect: reads the cache, creates and removes the emit directory, runs clippy, killing it at
/// the timeout, and writes the cache.
fn run_clippy(
    tool: &ToolCartridge,
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
    let invocation = Invocation {
        doc: &doc,
        index: record.index,
        edition,
        dialect: record.dialect,
        project: &rust.lints,
        emit: None,
    };
    let key_args = tool.args(&invocation);
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
        let file = emit.file();
        let args = tool.args(&Invocation {
            emit: Some(&file),
            ..invocation
        });
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
        |result| tool.lint_verdict(&pending.input, result),
        lint::Verdict::cacheable,
    );
    let findings = lint::findings(
        &verdict,
        tool,
        &pending.block,
        pending.doc.text(),
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
        cwd: runner::Cwd::Neutral,
        cap: runner::CAP,
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
            rust::before_compiling(&pending.input, rust.edition_of(record))
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
        Scan::Read(rust::Found::Unscannable(rust::FRONTMATTER)) => Some((
            NotChecked::CompileTimeRead,
            Some("compile-time read: frontmatter".to_owned()),
        )),
        Scan::Read(rust::Found::Character(c)) => Some((
            NotChecked::CompileTimeRead,
            Some(format!(
                "compile-time read: character {}",
                rust::code_point(c)
            )),
        )),
        Scan::Read(rust::Found::QualifiedMacro(name)) => Some((
            NotChecked::CompileTimeRead,
            Some(format!("compile-time read: path-qualified `{name}` macro")),
        )),
        Scan::Read(rust::Found::UnlistedMacro(name)) => Some((
            NotChecked::CompileTimeRead,
            Some(format!("compile-time read: `{name}` macro")),
        )),
        Scan::Read(rust::Found::Attribute(name)) if !name.is_empty() => Some((
            NotChecked::CompileTimeRead,
            Some(format!("compile-time read: `{name}` attribute")),
        )),
        Scan::Read(_) => Some((NotChecked::CompileTimeRead, None)),
        Scan::TooLarge => Some((NotChecked::TooLargeToScan, None)),
    }
}

/// Looks up each of `tools` on `env`'s `PATH` (once per binary, remembered in `looked_up`, so two
/// tools sharing one binary, such as `ruff-format` and `ruff`, warn once), adding to `slots` the
/// warning for one not found outside `root`, at `at`. Each found tool's run, in
/// `tools`' order, and the state for the first one that wasn't, with that tool.
///
/// Effect: reads file metadata along `PATH` and resolves symlinks.
fn found_runs(
    tools: [Option<&'static ToolCartridge>; 2],
    looked_up: &mut Vec<(&'static str, Found)>,
    slots: &mut Vec<Slot>,
    env: &ToolEnv,
    root: &Path,
    at: &Location,
) -> (
    [Option<ToolRun>; 2],
    Option<(BlockState, &'static ToolCartridge)>,
) {
    let mut runs = [None, None];
    let mut refused = None;
    for (run, tool) in runs.iter_mut().zip(tools) {
        let Some(tool) = tool else {
            continue;
        };
        let found =
            if let Some((_, found)) = looked_up.iter().find(|(binary, _)| *binary == tool.binary) {
                found.clone()
            } else {
                let found = find(tool.binary, env, root);
                slots.extend(tool_warning(tool, &found, at).map(Slot::Found));
                looked_up.push((tool.binary, found.clone()));
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

/// The formatter and linter `code_blocks` selects for a block in `language`, in `state`, whose
/// `steps` are due (none unless the block is pending), each through the [`tier_gate`] for
/// `granted` and the config's `tier`:
/// each `None` when its step isn't due, the language has none, or the gate turned its step in
/// `steps` unrunnable.
fn selected_tools(
    code_blocks: &CodeBlocks,
    steps: &mut Steps,
    language: Option<Language>,
    state: &BlockState,
    granted: Tier,
) -> (
    Option<&'static ToolCartridge>,
    Option<&'static ToolCartridge>,
) {
    let pending = *state == BlockState::NotChecked(NotChecked::Pending);
    let language = |step: &StepState| {
        (pending && *step == StepState::NotRun)
            .then_some(language)
            .flatten()
    };
    let tool = language(&steps.format)
        .and_then(|language| tools::selected(code_blocks, language, Step::Format));
    let linter = language(&steps.lint)
        .and_then(|language| tools::selected(code_blocks, language, Step::Lint));
    (
        tier_gate(tool, &mut steps.format, granted, code_blocks.tier()),
        tier_gate(linter, &mut steps.lint, granted, code_blocks.tier()),
    )
}

/// The one warning for `tool`'s binary, found as `found`, at `at`, the first block that needs it.
/// `None` when it is on `PATH` outside the root.
fn tool_warning(tool: &ToolCartridge, found: &Found, at: &Location) -> Option<Finding> {
    let finding = match found {
        Found::OnPath(_) => return None,
        Found::Missing => Finding::new(
            TOOL_MISSING,
            Severity::Warning,
            format!(
                "`{}` isn't on PATH, so its blocks aren't checked",
                tool.binary
            ),
        )
        .with_help(format!("install it with `{}`", tool.install)),
        Found::InsideRoot(real) => Finding::new(
            NOT_CHECKED,
            Severity::Warning,
            format!(
                "`{}` on PATH is `{}`, inside the checkout, so its blocks aren't checked",
                tool.binary,
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
            .with_help(
                match (
                    parenthesized_spelling(attribute),
                    replacement_for_removed(attribute),
                ) {
                    (Some(spelling), _) => format!("write `{attribute}` as `{spelling}`"),
                    (None, Some(replacement)) => {
                        format!("`{attribute}` was removed: write `{replacement}` instead")
                    }
                    (None, None) => {
                        format!("remove `{attribute}` or correct it to one of {KNOWN_ATTRIBUTES}")
                    }
                },
            )
        });
    comma.into_iter().chain(unknown).collect()
}

/// One `code-block-unknown-attribute` warning at `info` for each `lint(...)` or `format(...)`
/// that `block`'s language has no tool for under `code_blocks`, and for each whose value the
/// selected tool's cartridge doesn't accept, naming the values it does.
fn step_values(block: &CodeBlock, info: &Info, code_blocks: &CodeBlocks) -> Vec<Finding> {
    let parsed = info.parsed();
    let language = tag_of(block).and_then(Language::from_tag);
    parsed
        .attributes
        .iter()
        .filter_map(|written| step_attribute(written).map(|found| (*written, found)))
        .filter_map(|(written, found)| {
            let step = found.step.key();
            let tool =
                language.and_then(|language| tools::selected(code_blocks, language, found.step));
            let help = match tool {
                None => format!(
                    "remove `{written}`: supdoc has no {step} tool for `{}` blocks",
                    tag_of(block).unwrap_or(parsed.language)
                ),
                Some(tool)
                    if found
                        .value
                        .mode()
                        .is_some_and(|mode| tool.modes.contains(&mode)) =>
                {
                    return None;
                }
                Some(tool) => format!(
                    "write one of {} for {}",
                    tool.modes
                        .iter()
                        .map(|mode| match *mode {
                            "off" => format!("`{step}(off: <why>)`"),
                            mode => format!("`{step}({mode})`"),
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    tool.name
                ),
            };
            Some(
                Finding::new(
                    UNKNOWN_ATTRIBUTE,
                    Severity::Warning,
                    format!(
                        "the `{}` block's attribute `{written}` isn't one supdoc knows",
                        parsed.language
                    ),
                )
                .with_location(info.location.clone())
                .with_help(help),
            )
        })
        .collect()
}

/// One `code-block-missing-reason` warning at `info` for each `ignore`, `lint(off)` or
/// `format(off)` without a reason. The attribute still takes effect.
fn missing_reasons(info: &Info) -> Vec<Finding> {
    let parsed = info.parsed();
    let warn = |written: &str, fixed: String| {
        Finding::new(
            MISSING_REASON,
            Severity::Warning,
            format!(
                "the `{}` block's `{written}` doesn't say why",
                parsed.language
            ),
        )
        .with_location(info.location.clone())
        .with_help(format!("write it as `{fixed}`"))
    };
    parsed
        .attributes
        .iter()
        .filter_map(|attribute| {
            if let Some(found) = reasoned(attribute) {
                return found
                    .reason
                    .is_none()
                    .then(|| warn(attribute, format!("{}(<why>)", found.name)));
            }
            let found = step_attribute(attribute)?;
            matches!(found.value, StepValue::Off { reason: None })
                .then(|| warn(attribute, format!("{}(off: <why>)", found.step.key())))
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

    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::{
        BlockRecord, LintRan, MAX_WORKERS, Pending, RustSetup, Session, in_parallel, run_clippy,
    };
    use crate::cartridges::markdown::cartridge::cartridge as markdown;
    use crate::code_blocks::block::blocks_in;
    use crate::code_blocks::format::FixDoc;
    use crate::code_blocks::lint::Verdict;
    use crate::code_blocks::states::{BlockState, NotChecked, Steps};
    use crate::code_blocks::tools::{CARTRIDGES, Found, ToolCartridge, ToolEnv, find};
    use crate::config::code_blocks::Dialect;
    use crate::finding::Location;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::SourceFile;

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

    /// The clippy cartridge.
    fn clippy() -> &'static ToolCartridge {
        CARTRIDGES
            .iter()
            .find(|tool| tool.name == "clippy")
            .expect("the cartridges should include clippy")
    }

    /// The record of the one Rust block in `text`, a doc at `<root>/docs/a.md`, pending with
    /// clippy as its linter.
    fn rust_record(root: &Path, text: &str) -> BlockRecord {
        let path = AbsolutePath::new(&root.join("docs/a.md"))
            .expect("the fixture path should be absolute");
        let file = SourceFile::parse(path, text.to_owned(), markdown())
            .expect("tree-sitter should return a tree");
        let blocks = blocks_in(&file, |range| {
            let source = "docs/a.md"
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        });
        let block = blocks.first().expect("the doc should hold one block");
        let doc = Arc::new(FixDoc::new(
            Arc::from(text),
            "10s".parse().expect("the parse limit should be valid"),
        ));
        BlockRecord {
            at: block.fence.clone(),
            language: Some("rust".to_owned()),
            state: BlockState::NotChecked(NotChecked::Pending),
            steps: Steps::default(),
            reason: None,
            uses: Vec::new(),
            index: 1,
            tool: None,
            linter: Some(clippy()),
            dialect: Dialect::default(),
            edition: None,
            pending: Pending::of(block, &doc),
        }
    }

    /// [`run_clippy`] with `program` on the one block of `text` under `root`, its neutral
    /// directory under `cache`, with the test process's `PATH`, `HOME` and rustup homes.
    fn clippy_run(program: &Path, root: &Path, cache: &Path, text: &str) -> LintRan {
        let record = rust_record(root, text);
        let pending = record
            .pending
            .as_ref()
            .expect("the block's body should be in its doc");
        let env = ToolEnv::from_vars(|name| {
            if name == "XDG_CACHE_HOME" {
                Some(cache.as_os_str().to_owned())
            } else {
                std::env::var_os(name)
            }
        });
        let session = Session {
            env: &env,
            root,
            timeout: Some(Duration::from_secs(120)),
            cache: None,
        };
        run_clippy(
            clippy(),
            program,
            &record,
            pending,
            &session,
            &RustSetup::read(root),
        )
    }

    /// A temporary directory.
    fn temporary() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("a temporary directory should be creatable")
    }

    #[test]
    fn a_block_whose_text_could_not_be_read_settles_unreadable_not_pending_or_passed() {
        let (root, bin) = (temporary(), temporary());
        let shfmt = bin.path().join("shfmt");
        std::fs::write(&shfmt, "#!/bin/sh\n/bin/cat\n").expect("the stub should be writable");
        std::fs::set_permissions(&shfmt, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        let mut record = rust_record(root.path(), "# A\n\n```sh\necho hi\n```\n");
        record.language = Some("sh".to_owned());
        record.tool = CARTRIDGES.iter().find(|tool| tool.name == "shfmt");
        record.linter = None;
        record.pending = None;
        let mut evaluation = super::Evaluation {
            findings: Vec::new(),
            counts: None,
            records: vec![record],
            timeout: None,
        };
        let env =
            ToolEnv::from_vars(|name| (name == "PATH").then(|| bin.path().as_os_str().to_owned()));

        evaluation.look_up_tools(&env, root.path());

        let record = evaluation
            .records
            .first()
            .expect("the record should remain");
        assert_eq!(
            record.state,
            BlockState::NotChecked(NotChecked::InputUnreadable),
            "{record:?}"
        );
        assert_eq!(
            record.reason.as_deref(),
            Some("supdoc couldn't read the block's text from its doc")
        );
    }

    #[test]
    fn run_clippy_gives_the_block_alone_a_private_metadata_path_and_removes_it() {
        let (root, cache, out) = (temporary(), temporary(), temporary());
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"p\"\nedition = \"2021\"\n\n[lints.clippy]\nptr_arg = \"deny\"\n\
             pedantic = { level = \"warn\", priority = -1 }\ncargo = \"warn\"\n",
        )
        .expect("the manifest should be writable");
        let records = out.path().display();
        let stub = out.path().join("clippy-driver");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{records}/argv'\nenv > '{records}/env'\n\
                 pwd -P > '{records}/cwd'\ncat > '{records}/stdin'\n\
                 for arg in \"$@\"; do case \"$arg\" in --emit=metadata=*) \
                 emit=\"${{arg#--emit=metadata=}}\"; echo meta > \"$emit\"; \
                 echo \"$emit\" > '{records}/emit';; esac; done\n"
            ),
        )
        .expect("the stub should be writable");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        let record =
            |name: &str| std::fs::read_to_string(out.path().join(name)).unwrap_or_default();

        let ran = clippy_run(
            &stub,
            root.path(),
            cache.path(),
            "```rust\n# use std::fmt;\npub fn f() {}\n```\n",
        );

        assert!(matches!(ran, Ok((Verdict::Clean, _))), "{ran:?}");
        let emit = record("emit");
        let emit = Path::new(emit.trim());
        assert_eq!(
            record("argv"),
            format!(
                "--edition\n2021\n--crate-type\nlib\n--emit=metadata={}\n--error-format=json\n\
                 -W\nclippy::pedantic\n-D\nclippy::ptr_arg\n-A\nunused\n-A\ndead_code\n\
                 -A\nmissing_docs\n-A\nclippy::missing_docs_in_private_items\n-\n",
                emit.display()
            ),
            "groups first, `clippy::cargo` left out, relaxations last"
        );
        assert_eq!(
            record("stdin"),
            "# use std::fmt;\npub fn f() {}\n",
            "a `# ` line reaches clippy as written"
        );
        let cwd = PathBuf::from(record("cwd").trim());
        assert!(cwd.ends_with("supdoc/code-blocks/cwd"), "{cwd:?}");
        assert!(
            !emit.starts_with(&cwd),
            "{emit:?} is in the working directory"
        );
        assert!(
            !emit.parent().is_some_and(Path::exists),
            "the run's metadata directory should be gone: {emit:?}"
        );
        let env = record("env");
        assert!(
            env.lines().any(|line| line == "RUSTUP_AUTO_INSTALL=0"),
            "{env}"
        );
        for unwanted in ["RUSTFLAGS=", "CLIPPY_ARGS=", "CLIPPY_CONF_DIR="] {
            assert!(
                !env.lines().any(|line| line.starts_with(unwanted)),
                "{unwanted} reached clippy:\n{env}"
            );
        }
    }

    /// The real `clippy-driver` on `PATH`, or `None` with a note naming `test` when there is none.
    fn real_clippy(test: &str) -> Option<PathBuf> {
        let env = ToolEnv::from_vars(|name| std::env::var_os(name));
        let root = temporary();
        if let Found::OnPath(program) = find("clippy-driver", &env, root.path()) {
            return Some(program);
        }
        let _ignored = std::io::Write::write_all(
            &mut std::io::stderr(),
            format!("SKIPPED {test}: clippy-driver is not on PATH\n").as_bytes(),
        );
        None
    }

    #[test]
    fn run_clippy_with_the_real_clippy_places_a_vec_reference_at_its_doc_line() {
        let Some(program) = real_clippy("run_clippy_with_the_real_clippy") else {
            return;
        };
        let (root, cache) = (temporary(), temporary());
        let run = |text: &str| clippy_run(&program, root.path(), cache.path(), text);

        let flagged = run(
            "# D\n\n```rust\npub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n",
        )
        .expect("clippy should run with a private directory");
        let [finding] = flagged.1.as_slice() else {
            panic!("one ptr_arg finding expected: {flagged:?}");
        };
        let at = finding
            .location
            .as_ref()
            .expect("the finding should be placed");
        assert_eq!((at.line, at.column), (4, 17), "{finding:?}");
        assert!(finding.message.contains("clippy::ptr_arg"), "{finding:?}");

        let expected = run(
            "```rust\n#[expect(clippy::ptr_arg, reason = \"a non-example\")]\n\
             pub fn total(v: &Vec<i32>) -> i32 {\n    v.iter().sum()\n}\n```\n",
        )
        .expect("clippy should run");
        assert_eq!(expected.0, Verdict::Clean, "{expected:?}");

        let unresolved = run("```rust\npub fn f() -> i32 {\n    missing()\n}\n```\n")
            .expect("clippy should run");
        let lines: Vec<u32> = unresolved
            .1
            .iter()
            .filter_map(|finding| finding.location.as_ref().map(|at| at.line))
            .collect();
        assert_eq!(
            lines,
            [3],
            "an unresolved name is a lint finding: {unresolved:?}"
        );
    }
}
