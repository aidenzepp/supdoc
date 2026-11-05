//! The per-block pipeline that `check` runs: judges each code block and returns its findings
//! and the count of blocks in each state.

use std::borrow::Cow;
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
use crate::trust::Grant;

use super::biome;
use super::block::{CodeBlock, Info};
use super::cache::{self, Cache};
use super::excerpt;
use super::format::{self, FixDoc};
use super::fragment::{Frame, Kind};
use super::info::{
    KNOWN_ATTRIBUTES, SourceKind, StepValue, fragment_kind, is_fragment_word, is_known_attribute,
    parenthesized_spelling, reasoned, replacement_for_removed, source_attributes, step_attribute,
};
use super::lint;
use super::project::{self, BuildFailed, Plan, Refused};
use super::rumdl;
use super::runner::{self, Call, Ran};
use super::rust::{self, Scan};
use super::source::{self, SourceText, Unresolved};
use super::states::{
    self, Excerpt, NotChecked, StepState, Steps, block_fragment, state_of, tag_of,
    unsupported_reason,
};
use super::toolchain::{self, Refusal, Toolchain};
use super::tools::{self, Found, Invocation, Step, Tier, ToolCartridge, ToolEnv, find};
use super::uses::{self, UseState, Written};

pub(crate) use super::states::{BlockState, CodeBlockCounts};

/// The code for an info-string attribute supdoc doesn't know.
const UNKNOWN_ATTRIBUTE: &str = "code-block-unknown-attribute";
/// The code of the warning for a fragment kind the step's tool doesn't check.
const FRAGMENT_UNSUPPORTED: &str = "code-block-fragment-unsupported";

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
    /// How long building the project may take, from `[limits] code-block-build-timeout`. `None`
    /// is no limit.
    build_timeout: Option<Duration>,
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
    /// The kind its `fragment(<kind>)` attribute declares, if it writes one.
    pub(crate) fragment: Option<Kind>,
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
    /// The config and virtual path biome is given for it. `None` until the tools are looked up, and for
    /// any other tool.
    given: Option<biome::Given>,
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
/// Each due step's tool goes through the tier gate ([`tier_gate`]): `granted` is what the
/// operator granted the checkout, the project tier only when it is [`Grant::Trusted`]. The first
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
    granted: Grant,
) -> Result<Evaluation, CartridgeLoadError> {
    let Some(code_blocks) = config.code_blocks() else {
        return Ok(Evaluation::default());
    };
    let mut findings = Vec::new();
    let mut doc: Option<(&str, Option<Arc<FixDoc>>)> = None;
    let limits = config.limits_in_effect();
    let read = |file: &str| {
        read_doc(config, file)
            .map(|text| Arc::new(FixDoc::new(Arc::from(text), limits.parse_timeout.value)))
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
            findings.extend(attribute_findings(block, info, code_blocks));
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
        findings.extend(fragment_unsupported(block, &steps));
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
            given: None,
            language,
            state,
            steps,
            fragment: block_fragment(block),
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
        timeout: limits.code_block_timeout.value.get(),
        build_timeout: limits.code_block_build_timeout.value.get(),
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
    /// Before the first project-tier linter (clippy) runs on a block that passed the strict scan,
    /// the root's own `Cargo.toml` is planned and, when it names a lib, built once for the run
    /// (see [`link_against`]); every later block links against that build. A refused manifest,
    /// no cargo outside the root or no private build directory leaves those blocks' lint steps
    /// unrun; a failed build adds one error and fails each block that needed it.
    ///
    /// The runs for distinct blocks go in parallel (see [`in_parallel`]), each under its own
    /// timeout; findings and states come out as a serial run gives them, in block order.
    ///
    /// Effect: reads file metadata along `PATH`, resolves symlinks, reads and writes the result
    /// cache, builds the project with cargo when clippy needs it, and runs each found tool once
    /// per block not in it, killing it at the timeout.
    pub(crate) fn look_up_tools(&mut self, env: &ToolEnv, root: &Path) {
        let mut cache: Option<Option<Cache>> = None;
        let mut looked_up: Vec<(&'static str, Found)> = Vec::new();
        let mut rust_setup: Option<RustSetup> = None;
        let mut against: Option<Against> = None;
        let build_timeout = self.build_timeout;
        let timeout = self.timeout;
        let mut configs = ToolConfigs::default();
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
                .then(|| &*rust_setup.get_or_insert_with(|| RustSetup::read(env, root, timeout)));
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
            if !configs.give(record, tool, (env, root), &mut slots) {
                continue;
            }
            let (runs, refused) = found_runs(
                [record.tool, linter],
                &mut looked_up,
                &mut slots,
                env,
                root,
                &record.at,
            );
            let [format, mut lint] = runs;
            if format.is_some() || lint.is_some() {
                let ran = [format.is_some(), lint.is_some()];
                refuse_unfound(&mut record.steps, [record.tool, linter], ran, &looked_up);
                let compiles = lint
                    .as_ref()
                    .is_some_and(|run| run.tool.tier == Tier::Project);
                let unbuilt = rust.filter(|_| compiles).and_then(|rust| {
                    against
                        .get_or_insert_with(|| {
                            link_against(env, root, rust, build_timeout, &record.at, &mut slots)
                        })
                        .gate(&mut record.steps, &mut lint)
                });
                if format.is_none() && lint.is_none() && unbuilt.is_none() {
                    // Settled with its steps' reasons by `settle_pending`.
                    continue;
                }
                let _opened = cache.get_or_insert_with(|| Cache::open(env, root));
                slots.push(Slot::Run(jobs.len()));
                jobs.push(Job {
                    record: position,
                    format,
                    lint,
                    unbuilt,
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
            linked: Against::linked(against.as_ref()),
        };
        let ran = run_jobs(&jobs, &self.records, &session, rust_setup.as_ref());
        self.settle(slots, &jobs, ran);
        self.conclude(cache);
    }

    /// Settles the blocks still pending, prunes `cache` and recounts the states.
    ///
    /// Effect: prunes the result cache.
    fn conclude(&mut self, cache: Option<Cache>) {
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
    /// before lint) deciding it and naming its tool in the reason. A job whose lint step needed a
    /// project build that failed has its lint step unrun and fails with [`PROJECT_UNBUILT`] as
    /// its reason. A run with no result, or whose steps both didn't run, leaves its block to
    /// [`Self::settle_pending`].
    fn settle(&mut self, slots: Vec<Slot>, jobs: &[Job], ran: Vec<Option<StepsRan>>) {
        let mut ran = ran.into_iter();
        let mut linted_nothing_warned = false;
        let mut indent_unknown_warned = false;
        let mut reshaped_warned = false;
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
            // Each outcome with the tool that decided it, `None` for the project build.
            let mut outcomes: Vec<(BlockState, Option<&ToolCartridge>)> = Vec::new();
            match (format, job.format.as_ref()) {
                (Some(Ok((verdict, findings))), Some(run)) => {
                    self.findings.extend(findings);
                    record.steps.format = StepState::Ran;
                    outcomes.push((verdict.state(), Some(run.tool)));
                }
                (Some(Err(reason)), Some(_)) => {
                    if reason == FRAME_INDENT_UNKNOWN && !indent_unknown_warned {
                        indent_unknown_warned = true;
                        self.findings.push(indent_unknown(&record.at));
                    }
                    if reason == FRAME_RESHAPED && !reshaped_warned {
                        reshaped_warned = true;
                        self.findings.push(frame_reshaped(&record.at));
                    }
                    record.steps.format = StepState::Unrunnable { reason };
                }
                _ => {}
            }
            match (lint, job.lint.as_ref()) {
                (Some(Ok((verdict, findings))), Some(run)) => {
                    self.findings.extend(findings);
                    record.steps.lint = StepState::Ran;
                    outcomes.push((lint_state(&verdict), Some(run.tool)));
                }
                (Some(Err(reason)), Some(_)) => {
                    if reason == BIOME_LINTED_NOTHING && !linted_nothing_warned {
                        linted_nothing_warned = true;
                        self.findings.push(linted_nothing(&record.at));
                    }
                    record.steps.lint = StepState::Unrunnable { reason };
                }
                _ => {}
            }
            if let Some(failure) = job.unbuilt {
                record.steps.lint = StepState::Unrunnable {
                    reason: PROJECT_UNBUILT,
                };
                outcomes.push((BlockState::Failed(failure), None));
            }
            let decided = outcomes
                .iter()
                .find(|(state, _)| matches!(state, BlockState::Failed(_)))
                .or_else(|| outcomes.first());
            if let Some((state, tool)) = decided {
                record.state = state.clone();
                record.reason = match tool {
                    Some(tool) => record.state.reason(
                        &record.steps,
                        record.language.as_deref(),
                        Some(tool.name),
                    ),
                    None => Some(PROJECT_UNBUILT.to_owned()),
                };
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

/// Runs each of `jobs` on its block in `records` with `session`, in parallel (see
/// [`in_parallel`]): its formatter, then its linter unless the formatter found the block doesn't
/// parse, each Rust tool with `rust`'s setup. One result per job, in order.
///
/// Effect: reads and writes the result cache and runs the tools, killing each at its timeout.
fn run_jobs(
    jobs: &[Job],
    records: &[BlockRecord],
    session: &Session<'_>,
    rust: Option<&RustSetup>,
) -> Vec<Option<StepsRan>> {
    in_parallel(jobs, |job| {
        let record = records.get(job.record)?;
        let format = job.format.as_ref().and_then(|run| {
            let rust = rust.filter(|_| run.tool.language == Language::Rust);
            let frame = Frame::of(run.tool, record.fragment);
            run_formatter(run.tool, &run.program, record, frame, session, rust)
        });
        let parses = parses(format.as_ref());
        let lint = job.lint.as_ref().filter(|_| parses).and_then(|run| {
            let rust = rust.filter(|_| run.tool.language == Language::Rust);
            run_linter(run.tool, &run.program, record, session, rust)
        });
        Some(StepsRan { format, lint })
    })
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

/// The run's one `code-block-not-checked` warning, at `at`, the first framed block whose format
/// step didn't run because supdoc couldn't read its formatter's indent from the project's config.
fn indent_unknown(at: &Location) -> Finding {
    Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        "supdoc couldn't read the indent the formatter uses from the project's config, so the \
         format step didn't run on blocks it reads inside a frame",
    )
    .with_help(
        "set the indent plainly in the formatter's config (rustfmt's `hard_tabs`, `tab_spaces` \
         and `max_width`; biome's `indentStyle` and `indentWidth`, outside `overrides`), or mark \
         the blocks `format(off: <reason>)`",
    )
    .with_location(at.clone())
}

/// The run's one `code-block-not-checked` warning, at `at`, the first framed block whose format
/// step didn't run because the project's rustfmt config reshapes the frame.
fn frame_reshaped(at: &Location) -> Finding {
    Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        "the project's rustfmt settings reshape the frame supdoc puts around a fragment, so the \
         format step didn't run on blocks rustfmt reads inside a frame",
    )
    .with_help(
        "leave `brace_style`, `fn_single_line` and `blank_lines_lower_bound` at their defaults in \
         rustfmt's config, or mark the blocks `format(off: <reason>)`",
    )
    .with_location(at.clone())
}

/// The reason a framed block's format step didn't run: the project's rustfmt config sets an
/// option that reshapes the frame's own lines ([`FRAME_SHAPING`]), so rustfmt wouldn't print it
/// back.
pub(super) const FRAME_RESHAPED: &str =
    "the project's rustfmt settings reshape the fragment's frame";

/// The reason a framed block's format step didn't run: supdoc couldn't read the indent its
/// formatter uses, so it couldn't tell the block's lines from the frame's indent.
pub(super) const FRAME_INDENT_UNKNOWN: &str =
    "supdoc couldn't read the formatter's indent from the project's config";

/// The run's one `code-block-not-checked` warning, at `at`, the first block whose biome lint
/// step didn't run because biome checked no file.
fn linted_nothing(at: &Location) -> Finding {
    Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        "biome lint checked no file for this block, so its TypeScript, JavaScript and JSON blocks' \
         lint step didn't run: the project's biome config leaves them out",
    )
    .with_help(
        "find the setting in the project's biome config that excludes the doc's blocks, or mark \
         the blocks `lint(off: <reason>)`",
    )
    .with_location(at.clone())
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
    /// How the block fails when its linter needed the project build and the build failed.
    unbuilt: Option<states::Failure>,
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
    /// The format step's, or why it couldn't run.
    format: Option<FormatRan>,
    /// The lint step's, or why it couldn't run.
    lint: Option<LintRan>,
}

/// What a lint step gave: its verdict and findings, or why supdoc couldn't run it safely.
type LintRan = Result<(lint::Verdict, Vec<Finding>), &'static str>;

/// What a format step gave: its verdict and findings, or why supdoc couldn't run it.
type FormatRan = Result<(format::Verdict, Vec<Finding>), &'static str>;

/// Whether a format step that gave `format` let the block parse, so its lint step may run: not
/// when the formatter couldn't parse it, nor when a frame line showed it isn't its kind.
fn parses(format: Option<&FormatRan>) -> bool {
    !matches!(
        format,
        Some(Ok((
            format::Verdict::ParseFailed { .. } | format::Verdict::NotKind(_),
            _
        )))
    )
}

/// Why a step whose cartridge is above the effective tier doesn't run when the operator hasn't
/// granted trust: clippy compiles the block, so it belongs to the project tier.
pub(super) const CLIPPY_NEEDS_TRUST: &str =
    "clippy runs only in the project tier, which needs `supdoc trust`";

/// Why such a step doesn't run when `SUPDOC_TRUST=0` turned trust off for the run.
pub(super) const CLIPPY_TRUST_OFF: &str =
    "clippy runs only in the project tier, and `SUPDOC_TRUST=0` turns trust off for this run";

/// Why such a step doesn't run when the project itself lowered the tier.
const TIER_LOWERED: &str = "`tier = \"static\"` in `[features.code-blocks]`";

/// `tool` as the tiers allow it: `None` for a cartridge above the effective tier, the lower of
/// the tier `granted` allows (the project tier only for [`Grant::Trusted`]) and `ceiling` (the
/// project's `tier`). Its `step` then doesn't run, with [`TIER_LOWERED`] as its reason when the
/// ceiling holds it, [`CLIPPY_TRUST_OFF`] when `SUPDOC_TRUST=0` turned trust off, else
/// [`CLIPPY_NEEDS_TRUST`]. A project-tier cartridge runs only in a trusted checkout whose
/// ceiling allows it. The gate sits where the tool is chosen, before any scan, cache key or spawn.
fn tier_gate(
    tool: Option<&'static ToolCartridge>,
    step: &mut StepState,
    granted: Grant,
    ceiling: Tier,
) -> Option<&'static ToolCartridge> {
    let found = tool?;
    let allowed = if granted == Grant::Trusted {
        Tier::Project
    } else {
        Tier::Static
    };
    let effective = allowed.min(ceiling);
    if found.tier <= effective {
        return tool;
    }
    let reason = if found.tier > ceiling {
        TIER_LOWERED
    } else if granted == Grant::TurnedOff {
        CLIPPY_TRUST_OFF
    } else {
        CLIPPY_NEEDS_TRUST
    };
    *step = StepState::Unrunnable { reason };
    None
}

/// The run's one `code-block-not-trusted` warning, at the first of `records` with a step held
/// because the operator hasn't granted trust. When `SUPDOC_TRUST=0` turned trust off, it says so
/// and how to undo that, since `supdoc trust` can't help then. `None` when no step was held for
/// want of trust.
fn not_trusted(records: &[BlockRecord]) -> Option<Finding> {
    let (at, reason) = records
        .iter()
        .find_map(|record| Some((&record.at, record.steps.held_for_trust()?)))?;
    let finding = if reason == CLIPPY_TRUST_OFF {
        Finding::new(
            NOT_TRUSTED,
            Severity::Warning,
            "clippy didn't run on Rust blocks: it compiles them, and `SUPDOC_TRUST=0` turns \
             trust off for this run",
        )
        .with_help(
            "unset `SUPDOC_TRUST` to let `supdoc trust` decide, or set it to `1` to trust this \
             run; to stop asking, set `tier = \"static\"` in `[features.code-blocks]` or \
             `lint = \"off\"` in `[features.code-blocks.rust.clippy]`",
        )
    } else {
        Finding::new(
            NOT_TRUSTED,
            Severity::Warning,
            "clippy didn't run on Rust blocks: it compiles them, and this checkout isn't trusted",
        )
        .with_help(
            "run `supdoc trust` to trust this checkout, or set `SUPDOC_TRUST=1` for one run (as \
             in CI); to stop asking, set `tier = \"static\"` in `[features.code-blocks]` or \
             `lint = \"off\"` in `[features.code-blocks.rust.clippy]`",
        )
    };
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

/// The project config each non-Rust tool is given, found once per doc directory.
#[derive(Debug, Default)]
struct ToolConfigs {
    /// biome's.
    biome: BiomeConfigs,
    /// rumdl's.
    rumdl: RumdlConfigs,
}

impl ToolConfigs {
    /// Gives `record` the config its tool gets, as [`BiomeConfigs::give`] and
    /// [`RumdlConfigs::give`] do. Whether the block goes on to its tools: not when its config was
    /// refused.
    fn give(
        &mut self,
        record: &mut BlockRecord,
        tool: &ToolCartridge,
        place: (&ToolEnv, &Path),
        slots: &mut Vec<Slot>,
    ) -> bool {
        self.biome.give(record, tool, place, slots).is_ok()
            && self.rumdl.give(record, tool, place, slots).is_ok()
    }
}

/// The biome config flags found for each doc directory, and the refusals already warned about.
#[derive(Debug, Default)]
struct BiomeConfigs {
    /// Each doc directory's result, in the order they were asked for.
    found: Vec<(PathBuf, Result<biome::Given, biome::Refusal>)>,
    /// The refusals a warning has been added for.
    warned: Vec<biome::Refusal>,
}

impl BiomeConfigs {
    /// Gives `record`, a TypeScript, JavaScript or JSON block, the flags biome gets for its doc,
    /// found once per directory. A refusal settles the block as not checked, with its one
    /// warning per refusal added to `slots`.
    ///
    /// Effect: reads the config and may create supdoc's private directory.
    ///
    /// # Errors
    ///
    /// The refusal that left the block not checked.
    fn give(
        &mut self,
        record: &mut BlockRecord,
        tool: &ToolCartridge,
        (env, root): (&ToolEnv, &Path),
        slots: &mut Vec<Slot>,
    ) -> Result<(), biome::Refusal> {
        if !matches!(
            tool.language,
            Language::TypeScript | Language::JavaScript | Language::Json
        ) {
            return Ok(());
        }
        let doc = root.join(record.at.file.as_str());
        let dir = doc.parent().unwrap_or(root).to_path_buf();
        let found = if let Some((_, found)) = self.found.iter().find(|(known, _)| *known == dir) {
            found.clone()
        } else {
            let found = biome::project_args(env, root, &doc);
            self.found.push((dir, found.clone()));
            found
        };
        match found {
            Ok(project) => {
                record.given = Some(project);
                Ok(())
            }
            Err(refusal) => {
                let (reason, warning) = biome_refusal(&refusal, &record.at);
                if self.first_warning(&refusal) {
                    slots.push(Slot::Found(warning));
                }
                record.state = BlockState::NotChecked(NotChecked::ToolConfig);
                record.reason = Some(reason);
                Err(refusal)
            }
        }
    }

    /// Whether `refusal` hasn't been warned about yet, remembering that it now has.
    fn first_warning(&mut self, refusal: &biome::Refusal) -> bool {
        let new = !self.warned.contains(refusal);
        if new {
            self.warned.push(refusal.clone());
        }
        new
    }
}

/// The state reason and the `code-block-not-checked` warning, at `at`, for a block biome isn't
/// given because of `refusal`.
fn biome_refusal(refusal: &biome::Refusal, at: &Location) -> (String, Finding) {
    let (reason, message, help) = match refusal {
        biome::Refusal::Config { file, why, help } => (
            format!("biome config `{file}` {why}"),
            format!(
                "the project's `{file}` {why}, so its TypeScript, JavaScript and JSON blocks aren't \
                 checked"
            ),
            format!("in `{file}`: {help}"),
        ),
        biome::Refusal::NoPrivateDir => (
            BIOME_NO_PRIVATE_DIR.to_owned(),
            format!("{BIOME_NO_PRIVATE_DIR}, so its blocks aren't checked"),
            "set `XDG_CACHE_HOME` or `HOME` to a directory outside the project".to_owned(),
        ),
    };
    let finding = Finding::new(NOT_CHECKED, Severity::Warning, message)
        .with_help(help)
        .with_location(at.clone());
    (reason, finding)
}

/// Why biome didn't run when supdoc had no private directory to keep the config it gives biome in.
const BIOME_NO_PRIVATE_DIR: &str = "biome needs a private directory under `XDG_CACHE_HOME` or `HOME` for the config supdoc gives it, and there is none";

/// The rumdl config flags found for each doc directory, and the refusals already warned about.
#[derive(Debug, Default)]
struct RumdlConfigs {
    /// Each doc directory's result, in the order they were asked for.
    found: Vec<(PathBuf, Result<rumdl::Project, rumdl::Refusal>)>,
    /// The refusals a warning has been added for.
    warned: Vec<rumdl::Refusal>,
}

impl RumdlConfigs {
    /// Gives `record`, a Markdown block, the flags rumdl gets for its doc, found once per
    /// directory. A refusal settles the block as not checked, with its one warning per refusal
    /// added to `slots`.
    ///
    /// Effect: reads the config and may create supdoc's private directory and file for it.
    ///
    /// # Errors
    ///
    /// The refusal that left the block not checked.
    fn give(
        &mut self,
        record: &mut BlockRecord,
        tool: &ToolCartridge,
        (env, root): (&ToolEnv, &Path),
        slots: &mut Vec<Slot>,
    ) -> Result<(), rumdl::Refusal> {
        if tool.language != Language::Markdown {
            return Ok(());
        }
        let doc = root.join(record.at.file.as_str());
        let dir = doc.parent().unwrap_or(root).to_path_buf();
        let found = if let Some((_, found)) = self.found.iter().find(|(known, _)| *known == dir) {
            found.clone()
        } else {
            let found = rumdl::project_args(env, root, &doc);
            self.found.push((dir, found.clone()));
            found
        };
        match found {
            Ok(project) => {
                record.given = Some(biome::Given {
                    mirror: Some(project.doc_as_seen(&doc)),
                    flags: project.flags,
                    config: None,
                    names: None,
                });
                Ok(())
            }
            Err(refusal) => {
                let (reason, warning) = rumdl_refusal(&refusal, &record.at);
                if !self.warned.contains(&refusal) {
                    self.warned.push(refusal.clone());
                    slots.push(Slot::Found(warning));
                }
                record.state = BlockState::NotChecked(NotChecked::ToolConfig);
                record.reason = Some(reason);
                Err(refusal)
            }
        }
    }
}

/// The state reason and the `code-block-not-checked` warning, at `at`, for a block rumdl isn't
/// given because of `refusal`.
fn rumdl_refusal(refusal: &rumdl::Refusal, at: &Location) -> (String, Finding) {
    let (reason, message, help) = match refusal {
        rumdl::Refusal::Config { file, why } => (
            format!("rumdl config `{file}` {why}"),
            format!("the project's `{file}` {why}, so its Markdown blocks aren't checked"),
            format!(
                "remove the `code-block-tools`, `command` and `extends` from `{file}`, or mark the blocks `ignore(<why>)`"
            ),
        ),
        rumdl::Refusal::NoPrivateDir => (
            RUMDL_NO_PRIVATE_DIR.to_owned(),
            format!("{RUMDL_NO_PRIVATE_DIR}, so its blocks aren't checked"),
            "set `XDG_CACHE_HOME` or `HOME` to a directory outside the project".to_owned(),
        ),
    };
    let finding = Finding::new(NOT_CHECKED, Severity::Warning, message)
        .with_help(help)
        .with_location(at.clone());
    (reason, finding)
}

/// Why rumdl didn't run when supdoc had no private directory to keep its config in.
const RUMDL_NO_PRIVATE_DIR: &str = "rumdl needs a private directory under `XDG_CACHE_HOME` or `HOME` for its config, and there is none";

/// Why clippy didn't run on a block when [`runner::neutral_dir`] found no private working
/// directory: its metadata would have nowhere private to go.
const NO_PRIVATE_DIR: &str =
    "clippy needs a private working directory under `XDG_CACHE_HOME` or `HOME`, and there is none";

/// Why clippy didn't run on a block when [`project::target_dir`] found no private directory to
/// build the project in.
const NO_PRIVATE_BUILD_DIR: &str =
    "clippy needs a private build directory under `XDG_CACHE_HOME` or `HOME`, and there is none";

/// Why clippy didn't run on a block when the root's `Cargo.toml` resolves outside the root.
const MANIFEST_OUTSIDE: &str = "`Cargo.toml` links outside the checkout";

/// Why clippy didn't run on a block when the root's `Cargo.toml` isn't a file supdoc reads.
const MANIFEST_UNREADABLE: &str = "`Cargo.toml` isn't a regular file supdoc can read";

/// Why clippy didn't run on a block when the root's `Cargo.toml` has both package tables.
const MANIFEST_AMBIGUOUS: &str = "`Cargo.toml` has both `[package]` and `[project]`";

/// Why clippy didn't run on a block when the root package would build in an outside workspace.
const OUTSIDE_WORKSPACE: &str =
    "`Cargo.toml` would build in a cargo workspace outside the trusted directory";

/// Why a block whose clippy needed the project build failed when that build failed.
const PROJECT_UNBUILT: &str = "the project didn't build";

/// What clippy links each trusted Rust block against, settled once per run by
/// [`link_against`].
#[derive(Debug)]
enum Against {
    /// The block alone: the root has no `Cargo.toml`, or no lib a block can name.
    Alone,
    /// The project build.
    Built {
        /// Its `--extern` flags, one per project lib, then `-L dependency=<deps>`.
        flags: Vec<OsString>,
        /// Its fingerprint, which names it in the result cache.
        fingerprint: [u8; 32],
    },
    /// supdoc won't build the project, for the reason held, so clippy doesn't run.
    Unrunnable(&'static str),
    /// cargo is missing from `PATH`, or inside the checkout.
    CargoRefused {
        /// Whether cargo on `PATH` is inside the checkout, rather than missing.
        inside_root: bool,
    },
    /// The project didn't build: how each block that needed it fails.
    Unbuilt(states::Failure),
}

impl Against {
    /// Applies this to a block whose linter is `lint`, with `steps`: drops the lint run, marking
    /// the lint step unrunnable or its tool refused, unless clippy can link the block. `Some` with
    /// how the block fails when the project didn't build.
    fn gate(&self, steps: &mut Steps, lint: &mut Option<ToolRun>) -> Option<states::Failure> {
        let unbuilt = match self {
            Self::Alone | Self::Built { .. } => return None,
            Self::Unrunnable(reason) => {
                steps.lint = StepState::Unrunnable { reason };
                None
            }
            Self::CargoRefused { inside_root } => {
                steps.lint = StepState::ToolRefused {
                    binary: "cargo",
                    inside_root: *inside_root,
                };
                None
            }
            // `settle` marks the lint step and gives the block its reason.
            Self::Unbuilt(failure) => Some(*failure),
        };
        *lint = None;
        unbuilt
    }

    /// What clippy links each block against under `against`: the build's flags and
    /// fingerprint, or nothing.
    fn linked(against: Option<&Self>) -> Linked<'_> {
        match against {
            Some(Self::Built { flags, fingerprint }) => Linked {
                flags,
                fingerprint: Some(fingerprint),
            },
            _ => Linked::default(),
        }
    }
}

/// What clippy links a block against: the project build's flags and fingerprint, empty and
/// `None` when it compiles alone.
#[derive(Debug, Clone, Copy, Default)]
struct Linked<'run> {
    /// The `--extern` and `-L` flags.
    flags: &'run [OsString],
    /// The build's fingerprint.
    fingerprint: Option<&'run [u8; 32]>,
}

/// What clippy links `root`'s blocks against, decided at `at`, the first block that needs it,
/// with `rust`'s toolchain: [`Against::Alone`] when the root's `Cargo.toml` plans nothing, else
/// the project built once with cargo under `timeout`. A refused manifest adds one
/// `code-block-not-checked` warning naming why; cargo not found outside `root` adds its
/// `code-block-tool-missing` or `code-block-not-checked` warning; a failed build adds its one
/// error (see [`format::build_finding`]). Each goes to `slots` before the block's own findings.
///
/// Effect: reads the root's `Cargo.toml` and those above it, looks cargo up on `PATH`, creates
/// the build directory, and runs `cargo check`, which runs the project's build scripts and proc
/// macros, killing it at the timeout.
fn link_against(
    env: &ToolEnv,
    root: &Path,
    rust: &RustSetup,
    timeout: Option<Duration>,
    at: &Location,
    slots: &mut Vec<Slot>,
) -> Against {
    let plan = project::plan(root);
    let Some(project) = plan.project() else {
        return match &plan {
            Plan::Refused(refused) => {
                let (reason, warning) = refused_build(refused, at);
                slots.push(Slot::Found(warning));
                Against::Unrunnable(reason)
            }
            Plan::None | Plan::Package(_) | Plan::Workspace(_) => Against::Alone,
        };
    };
    let found = find("cargo", env, root);
    slots.extend(tool_warning("cargo", "install Rust with rustup", &found, at).map(Slot::Found));
    let cargo = match found {
        Found::OnPath(cargo) => cargo,
        Found::Missing => return Against::CargoRefused { inside_root: false },
        Found::InsideRoot(_) => return Against::CargoRefused { inside_root: true },
    };
    let Some(target) = project::target_dir(env, root) else {
        return Against::Unrunnable(NO_PRIVATE_BUILD_DIR);
    };
    let toolchain = rust.toolchain.env();
    let builder = project::Builder {
        cargo: &cargo,
        env,
        toolchain: &toolchain,
        target: &target,
        root,
        timeout,
    };
    match project::build(project, &builder) {
        Ok(build) => {
            let mut flags = Vec::new();
            for (name, file) in &build.externs {
                let mut named = OsString::from(name);
                named.push("=");
                named.push(file);
                flags.extend([OsString::from("--extern"), named]);
            }
            let mut deps = OsString::from("dependency=");
            deps.push(&build.deps);
            flags.extend([OsString::from("-L"), deps]);
            Against::Built {
                flags,
                fingerprint: build.fingerprint,
            }
        }
        Err(failed) => {
            slots.push(Slot::Found(format::build_finding(&failed, at, timeout)));
            Against::Unbuilt(match failed {
                BuildFailed::ToolFailed(_) => states::Failure::ToolFailed,
                BuildFailed::TimedOut => states::Failure::ToolTimeout,
            })
        }
    }
}

/// The lint step's reason for `refused`, and the run's one `code-block-not-checked` warning, at
/// `at`, saying why the project isn't built and what to change.
fn refused_build(refused: &Refused, at: &Location) -> (&'static str, Finding) {
    let (reason, why, help) = match refused {
        Refused::Manifest(Refusal::Outside) => (
            MANIFEST_OUTSIDE,
            "the project's `Cargo.toml` is a link that resolves outside the project".to_owned(),
            "replace `Cargo.toml` with a regular file inside the project",
        ),
        Refused::Manifest(_) => (
            MANIFEST_UNREADABLE,
            "the project's `Cargo.toml` couldn't be read as a regular UTF-8 file of at most 64 KiB"
                .to_owned(),
            "make `Cargo.toml` a readable UTF-8 text file of at most 64 KiB",
        ),
        Refused::Ambiguous => (
            MANIFEST_AMBIGUOUS,
            "the project's `Cargo.toml` has both `[package]` and its legacy name `[project]`"
                .to_owned(),
            "keep one of `[package]` and `[project]` in `Cargo.toml`",
        ),
        Refused::Workspace(outside) => (OUTSIDE_WORKSPACE, outside.reason(), outside.help()),
    };
    let warning = Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        format!("{why}, so clippy doesn't lint the project's Rust blocks"),
    )
    .with_help(help)
    .with_location(at.clone());
    (reason, warning)
}

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
    /// The toolchain the root's `rust-toolchain(.toml)` pins, else the one rustup names for the
    /// root: one value for rustfmt, clippy-driver and the project build's cargo alike.
    toolchain: Toolchain,
    /// `--config-path` for an in-root `rustfmt.toml`, or nothing.
    config: Vec<OsString>,
    /// The edition for a block that names none: the root `Cargo.toml`'s, else 2024.
    edition: Edition,
    /// The root `Cargo.toml`'s lint levels as clippy flags.
    lints: Vec<OsString>,
    /// `CLIPPY_CONF_DIR` for an in-root clippy config, or nothing.
    clippy_conf: Option<OsString>,
    /// The indent and width rustfmt's config gives it: `None` when the config can't be read for
    /// them.
    layout: Option<RustfmtLayout>,
}

impl RustSetup {
    /// Reads `root`'s toolchain file, `Cargo.toml` and finds its `rustfmt.toml` and clippy
    /// config, each only inside `root`. When the toolchain file pins no channel, asks the
    /// `rustup` on `env`'s `PATH` which toolchain the root gets, under `timeout` (see
    /// [`toolchain::active`]).
    ///
    /// Effect: reads file metadata, resolves symlinks, reads the toolchain file and
    /// `Cargo.toml`, and runs `rustup` at most once, killing it at the timeout.
    fn read(env: &ToolEnv, root: &Path, timeout: Option<Duration>) -> Self {
        let toolchain = match toolchain::read(root) {
            Toolchain::Default => toolchain::active(env, root, timeout),
            pinned => pinned,
        };
        let config = rust::config_args(root);
        let layout = RustfmtLayout::read(&config).map(|layout| layout.under(&toolchain));
        Self {
            toolchain,
            layout,
            config,
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

/// The width heuristics rustfmt derives from `max_width`, each with its share of a 100-column
/// line, as rustfmt's `WidthHeuristics::scaled` lists them.
const WIDTH_HEURISTICS: [(&str, u16); 8] = [
    ("fn_call_width", 60),
    ("attr_fn_like_width", 70),
    ("struct_lit_width", 18),
    ("struct_variant_width", 35),
    ("array_width", 60),
    ("chain_width", 60),
    ("single_line_if_else_max_width", 50),
    ("single_line_let_else_max_width", 50),
];

/// rustfmt's options that reshape a function's own braces or the lines at the start of its
/// body, so the frame around a block wouldn't come back as written, each with the value that
/// leaves the frame alone. Each is unstable: only a nightly rustfmt applies it.
const FRAME_SHAPING: [(&str, &str); 3] = [
    ("brace_style", "\"SameLineWhere\""),
    ("fn_single_line", "false"),
    ("blank_lines_lower_bound", "0"),
];

/// The indent and width settings rustfmt formats with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RustfmtLayout {
    /// `hard_tabs`: indent with a tab.
    hard_tabs: bool,
    /// `tab_spaces`: the spaces in one indent, and a tab's width under `hard_tabs`.
    tab_spaces: usize,
    /// `max_width`: the widest a line may be.
    max_width: usize,
    /// The value rustfmt gives each of [`WIDTH_HEURISTICS`] for `max_width`: the config's own
    /// (never above `max_width`), else the one `use_small_heuristics` derives. `None` for one
    /// `use_small_heuristics = "Off"` sets to a value no width changes.
    heuristics: [Option<usize>; 8],
    /// Whether the config sets one of [`FRAME_SHAPING`] to another value, and the toolchain
    /// may apply it.
    reshapes_frame: bool,
}

impl RustfmtLayout {
    /// The settings `config`, rustfmt's `--config-path` arguments, give it: its defaults with no
    /// config. `None` when the config file can't be read or parsed, or any of the keys holds a
    /// value rustfmt wouldn't take.
    ///
    /// Effect: reads the config file.
    fn read(config: &[OsString]) -> Option<Self> {
        let table = match config
            .iter()
            .skip_while(|arg| *arg != "--config-path")
            .nth(1)
        {
            None => toml::Table::new(),
            Some(path) => std::fs::read_to_string(path).ok()?.parse().ok()?,
        };
        Self::of(&table)
    }

    /// The settings `table`, a rustfmt config, gives rustfmt. `None` when one of the keys holds
    /// a value rustfmt wouldn't take.
    fn of(table: &toml::Table) -> Option<Self> {
        let hard_tabs = table
            .get("hard_tabs")
            .map_or(Some(false), toml::Value::as_bool)?;
        let number = |key: &str| {
            table
                .get(key)
                .map(|value| usize::try_from(value.as_integer()?).ok())
        };
        let width = |key: &str, default: usize| match number(key) {
            None => Some(default),
            Some(width) => width.filter(|width| *width > 0),
        };
        let tab_spaces = width("tab_spaces", 4)?;
        let max_width = width("max_width", 100)?;
        let derived = match table.get("use_small_heuristics") {
            None => "Default",
            Some(value) => value.as_str()?,
        };
        let scale = scale(max_width)?;
        let mut heuristics = [None; 8];
        for (slot, (key, share)) in heuristics.iter_mut().zip(WIDTH_HEURISTICS) {
            *slot = match (number(key), derived) {
                (Some(set), _) => Some(set?.min(max_width)),
                (None, "Default") => Some(scaled(share, scale)),
                (None, "Max") => Some(max_width),
                (None, "Off") => None,
                (None, _) => return None,
            };
        }
        let reshapes_frame = FRAME_SHAPING.iter().any(|(key, plain)| {
            table
                .get(*key)
                .is_some_and(|value| value.to_string() != *plain)
        });
        Some(Self {
            hard_tabs,
            tab_spaces,
            max_width,
            heuristics,
            reshapes_frame,
        })
    }

    /// These settings, run by `toolchain`'s rustfmt: one from a stable or beta channel ignores
    /// the unstable options, so they reshape nothing.
    fn under(self, toolchain: &Toolchain) -> Self {
        let stable = matches!(
            toolchain,
            Toolchain::Channel(name)
                if name.starts_with("stable")
                    || name.starts_with("beta")
                    || name.starts_with(|c: char| c.is_ascii_digit())
        );
        Self {
            reshapes_frame: self.reshapes_frame && !stable,
            ..self
        }
    }

    /// The indent unit: a tab under `hard_tabs`, else `tab_spaces` spaces.
    fn unit(self) -> String {
        if self.hard_tabs {
            "\t".to_owned()
        } else {
            " ".repeat(self.tab_spaces)
        }
    }

    /// The arguments that make rustfmt use these settings, on a block whose every line it reads
    /// led by one indent unit, whatever config it finds on its own, such as one above its working
    /// directory or in the user's home: `max_width` grows by the unit's width (a tab's is
    /// `tab_spaces`), so a block line fits exactly when it fits the project's width as written,
    /// and each width heuristic is held to the value the project's own `max_width` gives it, so
    /// the wider frame changes no other width rustfmt decides by.
    fn framed_args(self) -> [OsString; 2] {
        let pinned: String = WIDTH_HEURISTICS
            .iter()
            .zip(self.heuristics)
            .filter_map(|((key, _), value)| value.map(|value| format!(",{key}={value}")))
            .collect();
        let config = format!(
            "hard_tabs={},tab_spaces={},max_width={}{pinned}",
            self.hard_tabs,
            self.tab_spaces,
            self.max_width.saturating_add(self.tab_spaces)
        );
        [OsString::from("--config"), OsString::from(config)]
    }
}

/// The factor rustfmt scales its default width heuristics by for `max_width`, computed as its
/// `WidthHeuristics::scaled` does, in `f32`: 1 up to 100 columns, else the width over 100
/// rounded to the nearest tenth. `None` for a width past `u16::MAX`.
fn scale(max_width: usize) -> Option<f32> {
    let width = f32::from(u16::try_from(max_width).ok()?);
    Some(if max_width > 100 {
        (width / 100.0 * 10.0).round() / 10.0
    } else {
        1.0
    })
}

/// A width heuristic whose share of a 100-column line is `share`, scaled by `scale` and rounded
/// as rustfmt rounds it.
fn scaled(share: u16, scale: f32) -> usize {
    let value = (f32::from(share) * scale).round();
    // The whole number `value` is, found without a lossy cast: the largest `u16` not above it.
    let (mut low, mut high) = (0_u16, u16::MAX);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if f32::from(mid) <= value {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    usize::from(low)
}

/// The indent unit biome uses for a `language` block under `given`'s config: its language
/// section's `formatter.indentStyle` and `indentWidth`, else the top-level `formatter`'s, else
/// biome's defaults (a tab; two spaces when the style is `space`). No config is biome's defaults.
/// `None` when the config can't be read or parsed, has an override that sets an indent (which
/// would give some paths another one), or holds a value biome wouldn't take.
///
/// Effect: reads the private config file.
fn biome_indent(language: Language, given: Option<&biome::Given>) -> Option<String> {
    let section = match language {
        Language::TypeScript | Language::JavaScript => "javascript",
        _ => return None,
    };
    let Some(path) = given.and_then(|given| given.config.as_ref()) else {
        return Some("\t".to_owned());
    };
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let indent_keys = ["indentStyle", "indentWidth"];
    let sets_indent = |override_: &serde_json::Value| {
        ["formatter", section].iter().any(|table| {
            indent_keys.iter().any(|key| {
                let at = if *table == "formatter" {
                    format!("/formatter/{key}")
                } else {
                    format!("/{table}/formatter/{key}")
                };
                override_.pointer(&at).is_some()
            })
        })
    };
    match config.get("overrides") {
        None => {}
        Some(serde_json::Value::Array(overrides)) => {
            if overrides.iter().any(sets_indent) {
                return None;
            }
        }
        Some(_) => return None,
    }
    let setting = |key: &str| {
        config
            .pointer(&format!("/{section}/formatter/{key}"))
            .or_else(|| config.pointer(&format!("/formatter/{key}")))
    };
    match setting("indentStyle").map_or(Some("tab"), serde_json::Value::as_str)? {
        "tab" => Some("\t".to_owned()),
        "space" => {
            let width = setting("indentWidth").map_or(Some(2), serde_json::Value::as_u64)?;
            let width = usize::try_from(width).ok().filter(|width| *width > 0)?;
            Some(" ".repeat(width))
        }
        _ => None,
    }
}

/// How a formatter runs on a block inside a frame.
#[derive(Debug)]
struct Framing {
    /// The indent unit before each line of the block.
    indent: String,
    /// The arguments that hold the tool to the project's layout.
    args: Vec<OsString>,
    /// The columns the tool's line width grew by to make room for `indent`.
    widened: usize,
}

impl Framing {
    /// [`format::judge_framed`] on `result`, `tool`'s run on the block whose body is `input`
    /// inside `frame`, with rustfmt's widths restated in the project's terms
    /// ([`rust::unwidened`]).
    fn judge(
        &self,
        tool: &ToolCartridge,
        frame: Frame,
        input: &str,
        result: Result<Ran, runner::Failure>,
    ) -> format::Verdict {
        let result = result.map(|ran| Ran {
            stderr: if self.widened == 0 {
                ran.stderr.clone()
            } else {
                rust::unwidened(&ran.stderr, self.widened, self.indent.chars().count())
            },
            ..ran
        });
        format::judge_framed(tool, frame, &self.indent, input, result)
    }
}

/// How `tool` formats a block inside a frame, read from the project's config for it (`rust`'s
/// for rustfmt, `given`'s for biome). [`FRAME_INDENT_UNKNOWN`] for a formatter whose indent
/// supdoc can't read, and when the config can't be read for it; [`FRAME_RESHAPED`] when
/// rustfmt's config reshapes the frame.
///
/// # Errors
///
/// The reason the format step can't run on the block inside its frame, as above.
///
/// Effect: reads biome's private config file.
fn framing(
    tool: &ToolCartridge,
    rust: Option<&RustSetup>,
    given: Option<&biome::Given>,
) -> Result<Framing, &'static str> {
    match tool.binary {
        "rustfmt" => match rust.and_then(|rust| rust.layout) {
            None => Err(FRAME_INDENT_UNKNOWN),
            Some(layout) if layout.reshapes_frame => Err(FRAME_RESHAPED),
            Some(layout) => Ok(Framing {
                indent: layout.unit(),
                args: layout.framed_args().to_vec(),
                widened: layout.tab_spaces,
            }),
        },
        "biome" => biome_indent(tool.language, given)
            .map(|indent| Framing {
                indent,
                args: Vec::new(),
                widened: 0,
            })
            .ok_or(FRAME_INDENT_UNKNOWN),
        _ => Err(FRAME_INDENT_UNKNOWN),
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
    /// What clippy links each block against.
    linked: Linked<'run>,
}

/// Runs `tool`, found at `program`, on `record`'s block from `session`'s root with its env, under
/// its timeout, or reads the run from its cache when the key is there, adding `rust`'s config
/// arguments and toolchain variables for a Rust tool: its verdict and findings, an unformatted
/// block's error carrying the fix to the formatter's output, or followed by a
/// `code-block-fix-unsafe` warning when [`format::safe_fix`] finds that output could end the
/// block early. `None` when its input couldn't be read, so it stays pending. A run whose verdict
/// [`format::Verdict::cacheable`] allows is stored in the cache.
///
/// A block `tool` reads only inside `frame` (its [`Frame::of`]) is formatted inside it, each
/// line indented by the unit the project's config gives the formatter ([`framing`]), and
/// judged by [`format::judge_framed`], so its fix holds only the author's lines; a frame-level
/// failure is reported at the block's info string, and rustfmt's widths in the project's terms
/// ([`rust::unwidened`]). When that unit can't be read, or rustfmt's config reshapes the frame,
/// the step doesn't run, with [`framing`]'s reason.
///
/// Effect: reads the cache, runs the tool, killing it at the timeout, and writes the cache.
fn run_formatter(
    tool: &ToolCartridge,
    program: &Path,
    record: &BlockRecord,
    frame: Option<Frame>,
    session: &Session<'_>,
    rust: Option<&RustSetup>,
) -> Option<FormatRan> {
    let Session { root, timeout, .. } = *session;
    let pending = record.pending.as_ref()?;
    let given = record.given.as_ref();
    let framed = match frame {
        None => None,
        Some(frame) => match framing(tool, rust, given) {
            Ok(framing) => Some((frame, framing)),
            Err(reason) => return Some(Err(reason)),
        },
    };
    let checked = framed
        .as_ref()
        .map_or(Cow::Borrowed(pending.input.as_str()), |(frame, framing)| {
            Cow::Owned(frame.wrap_indented(&pending.input, &framing.indent))
        });
    let input = checked.as_ref();
    let doc = root.join(record.at.file.as_str());
    let edition = record
        .edition
        .or_else(|| rust.map(|rust| rust.edition))
        .unwrap_or(Edition::E2024);
    let args = tool.args(&Invocation {
        doc: given
            .and_then(|given| given.mirror.as_deref())
            .unwrap_or(&doc),
        index: record.index,
        tag: record.language.as_deref().unwrap_or_default(),
        edition,
        dialect: record.dialect,
        project: rust.map_or(given.map_or(&[], |given| given.flags.as_slice()), |rust| {
            rust.config.as_slice()
        }),
        emit: None,
        externs: &[],
        fragment: record.fragment,
    });
    let vars = rust.map(|rust| rust.toolchain.env()).unwrap_or_default();
    let extra: Vec<(&str, &OsStr)> = vars
        .iter()
        .map(|(name, value)| (*name, value.as_os_str()))
        .collect();
    let held = framed
        .as_ref()
        .map_or(&[][..], |(_, framing)| framing.args.as_slice());
    let args: Vec<&OsStr> = args.iter().chain(held).map(OsString::as_os_str).collect();
    let run = cache::Run {
        tool,
        program,
        args: &args,
        extra: &extra,
        input,
        doc: &doc,
        project: None,
    };
    let verdict = cached_run(
        &run,
        session,
        |call| runner::run(call, session.env, session.root),
        |result| {
            let named = result.map(|ran| Ran {
                stderr: given.map_or_else(|| ran.stderr.clone(), |given| given.named(&ran.stderr)),
                ..ran
            });
            match &framed {
                Some((frame, framing)) => framing.judge(tool, *frame, &pending.input, named),
                None => format::judge(tool, input, named),
            }
        },
        format::Verdict::cacheable,
    );
    let at = match &verdict {
        format::Verdict::NotKind(_) | format::Verdict::FrameReformatted(_) => pending
            .block
            .info
            .as_ref()
            .map_or_else(|| record.at.clone(), |info| info.location.clone()),
        _ => parse_place(&verdict, pending).unwrap_or_else(|| record.at.clone()),
    };
    let finding = format::finding(&verdict, tool, &at, timeout)
        .map(|finding| jsonc_hint(finding, &verdict, record, &pending.input));
    let findings = match (&verdict, finding) {
        (format::Verdict::Unformatted(output), Some(unformatted)) => {
            format::fixed(unformatted, &pending.block, &pending.doc, output, tool)
        }
        (_, finding) => finding.into_iter().collect(),
    };
    Some(Ok((verdict, findings)))
}

/// The doc line a parse failure names in `pending`'s block. `None` for any other verdict, or a
/// line that isn't the block's.
fn parse_place(verdict: &format::Verdict, pending: &Pending) -> Option<Location> {
    let format::Verdict::ParseFailed {
        line: Some(line), ..
    } = verdict
    else {
        return None;
    };
    rust::LineMap::of(&pending.block, pending.doc.text())
        .and_then(|map| map.doc_line(*line).cloned())
}

/// `finding`, with help that names the `jsonc` tag when `verdict` is a parse failure of
/// `record`'s block, tagged `json`, whose body `input` holds a comment: biome reads a `json`
/// block as strict JSON, where a comment is a syntax error. Any other parse failure, such as a
/// trailing comma, keeps its help.
fn jsonc_hint(
    finding: Finding,
    verdict: &format::Verdict,
    record: &BlockRecord,
    input: &str,
) -> Finding {
    let comment = record.language.as_deref() == Some("json")
        && matches!(verdict, format::Verdict::ParseFailed { .. })
        && biome::holds_comment(input);
    if comment {
        finding.with_help(
            "the block holds a comment, which strict JSON doesn't allow: tag it `jsonc` if it is \
             JSON with comments, else remove the comment",
        )
    } else {
        finding
    }
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
    let given = record.given.as_ref();
    let invocation = Invocation {
        doc: given
            .and_then(|given| given.mirror.as_deref())
            .unwrap_or(&doc),
        index: record.index,
        tag: record.language.as_deref().unwrap_or_default(),
        edition: record.edition.unwrap_or(Edition::E2024),
        dialect: record.dialect,
        project: given.map_or(&[], |given| given.flags.as_slice()),
        emit: None,
        externs: &[],
        fragment: record.fragment,
    };
    let args = tool.args(&invocation);
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let frame = Frame::of(tool, record.fragment);
    let input = lint::checked(frame, &pending.input);
    let input = input.as_ref();
    let run = cache::Run {
        tool,
        program,
        args: &args,
        extra: &[],
        input,
        doc: &doc,
        project: None,
    };
    let biome = given.filter(|_| tool.name == BIOME_LINT);
    let spawn = |call: &Call<'_>| match biome {
        Some(given) => biome_lint(tool, call, &invocation, given, input, session),
        None => runner::run(call, session.env, session.root),
    };
    let judge = |result: Result<Ran, runner::Failure>| match result {
        Ok(ran) if biome.is_some() && biome::linted_nothing(&ran.stdout) => {
            Err(BIOME_LINTED_NOTHING)
        }
        result => Ok(tool.lint_verdict(input, frame.is_some(), result)),
    };
    let verdict = match cached_run(&run, session, spawn, judge, |judged| {
        judged.as_ref().is_ok_and(lint::Verdict::cacheable)
    }) {
        Ok(verdict) => verdict,
        Err(reason) => return Some(Err(reason)),
    };
    let read = lint::Read {
        tool,
        block: &pending.block,
        text: pending.doc.text(),
        frame,
    };
    let findings = lint::findings(&verdict, &read, session.timeout, false);
    let findings = match &verdict {
        lint::Verdict::Failed(failed) => {
            let at = frame
                .is_none()
                .then(|| parse_place(failed, pending))
                .flatten();
            findings
                .into_iter()
                .map(|finding| {
                    let finding = jsonc_hint(finding, failed, record, &pending.input);
                    match &at {
                        Some(at) => finding.with_location(at.clone()),
                        None => finding,
                    }
                })
                .collect()
        }
        _ => findings,
    };
    Some(Ok((verdict, findings)))
}

/// The linter that reads the block from a private file rather than stdin.
const BIOME_LINT: &str = "biome-lint";

/// Why biome lint's step counts as not run when its report says it checked no file: something
/// in the project's config still left the block out, so the run says nothing about it.
pub(super) const BIOME_LINTED_NOTHING: &str =
    "biome lint checked no file: the project's biome config leaves the block out";

/// Runs `call`, `tool` being biome lint, on the block `invocation` describes, written with
/// `given`'s config to a fresh [`biome::LintFile`] beside the neutral directory, since biome
/// lint reports nothing for a block on stdin. The directory is removed after the run whatever
/// its outcome.
///
/// # Errors
///
/// The run's failure, or a failed start when there is no private directory to write the block
/// to.
///
/// Effect: creates and removes the private directory, and runs biome, killing it at the
/// timeout.
fn biome_lint(
    tool: &ToolCartridge,
    call: &Call<'_>,
    invocation: &Invocation<'_>,
    given: &biome::Given,
    input: &str,
    session: &Session<'_>,
) -> Result<Ran, runner::Failure> {
    let neutral = runner::neutral_dir(session.env, session.root);
    let place = (invocation.doc, invocation.index, invocation.tag);
    let file = biome::LintFile::create(&neutral, given, place, input).ok_or_else(|| {
        runner::Failure::Spawn(std::io::Error::other(
            "supdoc couldn't create a private directory for the block biome lints",
        ))
    })?;
    let project = [file.config_flag()];
    let args = tool.args(&Invocation {
        project: &project,
        emit: Some(file.file()),
        ..*invocation
    });
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let result = runner::run(
        &Call {
            args: &args,
            ..*call
        },
        session.env,
        session.root,
    )
    .map(|ran| Ran {
        stderr: file.named(&ran.stderr),
        ..ran
    });
    drop(file);
    result
}

/// Runs `tool`, clippy found at `program`, on `record`'s Rust block, read as `pending`, as
/// [`run_linter`] does: `clippy-driver` with its cartridge's arguments for the block's edition,
/// the session's project build flags (none when the block compiles alone) and `rust`'s project
/// lints, `rust`'s toolchain variables and `CLIPPY_CONF_DIR`, and nothing else from the project.
/// The build's fingerprint is part of the cache key, and a missing name's finding says whether
/// the project's crate was passed. Its metadata goes to a fresh [`rust::EmitDir`], removed after
/// the run whatever its outcome.
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
        tag: record.language.as_deref().unwrap_or_default(),
        edition,
        dialect: record.dialect,
        project: &rust.lints,
        emit: None,
        externs: session.linked.flags,
        fragment: record.fragment,
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
    let frame = Frame::of(tool, record.fragment);
    let input = lint::checked(frame, &pending.input);
    let input = input.as_ref();
    let run = cache::Run {
        tool,
        program,
        args: &key_args,
        extra: &extra,
        input,
        doc: &doc,
        project: session.linked.fingerprint,
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
            tag: "",
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
        |result| tool.lint_verdict(input, frame.is_some(), result),
        lint::Verdict::cacheable,
    );
    let read = lint::Read {
        tool,
        block: &pending.block,
        text: pending.doc.text(),
        frame,
    };
    let findings = lint::findings(
        &verdict,
        &read,
        session.timeout,
        !session.linked.flags.is_empty(),
    );
    Ok((verdict, findings))
}

/// `judge` applied to `run`, read from `session`'s cache when its key is there, otherwise run
/// through `spawn` (which runs the call from the neutral directory with the session's env) under
/// its timeout, and stored in the cache when `cacheable` allows the verdict.
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
/// was refused or rustup couldn't name the root's toolchain (adding its one warning to `slots`
/// unless `warned`), or the block reads at compile
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
    if let Toolchain::Unresolved(unresolved) = &rust.toolchain {
        if !*warned {
            *warned = true;
            slots.push(Slot::Found(unresolved_warning(unresolved, &record.at)));
        }
        return Some((NotChecked::ToolchainUnresolved, None));
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
            Some(format!(
                "macro `{name}` called through a path isn't on supdoc's list"
            )),
        )),
        Scan::Read(rust::Found::UnlistedMacro(name)) => Some((
            NotChecked::CompileTimeRead,
            Some(format!("macro `{name}` isn't on supdoc's list")),
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
                let install = format!("install it with `{}`", tool.install);
                slots.extend(tool_warning(tool.binary, &install, &found, at).map(Slot::Found));
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
    granted: Grant,
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

/// The one warning for `binary`, found as `found`, at `at`, the first block that needs it, a
/// missing one's help saying how to `install` it. `None` when it is on `PATH` outside the root.
fn tool_warning(binary: &str, install: &str, found: &Found, at: &Location) -> Option<Finding> {
    let finding = match found {
        Found::OnPath(_) => return None,
        Found::Missing => Finding::new(
            TOOL_MISSING,
            Severity::Warning,
            format!("`{binary}` isn't on PATH, so its blocks aren't checked"),
        )
        .with_help(install.to_owned()),
        Found::InsideRoot(real) => Finding::new(
            NOT_CHECKED,
            Severity::Warning,
            format!(
                "`{binary}` on PATH is `{}`, inside the checkout, so its blocks aren't checked",
                real.display()
            ),
        )
        .with_help(format!(
            "put a `{binary}` from outside the project first on PATH"
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

/// The one `code-block-not-checked` warning, at `at`, for a root that pins no toolchain and whose
/// toolchain rustup couldn't name for `unresolved`, which leaves every Rust block unchecked. It
/// names the cause and quotes what rustup said.
fn unresolved_warning(unresolved: &toolchain::Unresolved, at: &Location) -> Finding {
    use toolchain::Cause;

    const PIN: &str = "or add a `rust-toolchain.toml` to the project naming a channel such as \
                       `stable` or `1.85.0`";
    let (cause, said) = match unresolved {
        toolchain::Unresolved::InsideRoot => {
            return unresolved_finding(
                "the `rustup` on PATH is inside the checkout",
                "put a `rustup` from outside the project first on PATH".to_owned(),
                at,
            );
        }
        toolchain::Unresolved::Rustup { cause, said } => (*cause, said.as_str()),
    };
    let quoted = if said.is_empty() {
        String::new()
    } else {
        format!(" (rustup said: `{said}`)")
    };
    let (why, help) = match cause {
        Cause::PathOrCustom => (
            format!(
                "`rustup show active-toolchain` named a path or custom toolchain, not a \
                 channel{quoted}, as a `path` key or a custom toolchain above it makes it"
            ),
            format!("remove the `path` toolchain or override above the project, {PIN}"),
        ),
        Cause::NotInstalled => (
            format!(
                "the toolchain `rustup show active-toolchain` chose isn't installed, and supdoc \
                 never installs one{quoted}"
            ),
            format!("install it with `rustup toolchain install`, {PIN}"),
        ),
        Cause::Unparseable => (
            format!(
                "`rustup show active-toolchain` couldn't parse a toolchain file above it{quoted}"
            ),
            format!("fix or remove that toolchain file, {PIN}"),
        ),
        Cause::NoDefault => (
            format!("rustup has no default toolchain to give it{quoted}"),
            format!("set one with `rustup default stable`, {PIN}"),
        ),
        Cause::Failed => (
            format!("`rustup show active-toolchain` failed{quoted}"),
            format!("run `rustup show active-toolchain` in the project to see why, {PIN}"),
        ),
        Cause::TimedOut => (
            "`rustup show active-toolchain` ran past `[limits] code-block-timeout`".to_owned(),
            format!(
                "run `rustup show active-toolchain` in the project to see why it hangs, raise \
                 `code-block-timeout`, {PIN}"
            ),
        ),
        Cause::OverCap => (
            "`rustup show active-toolchain` printed more than 1 MiB".to_owned(),
            format!(
                "run `rustup show active-toolchain` in the project to see what it prints, {PIN}"
            ),
        ),
    };
    unresolved_finding(&why, help, at)
}

/// The `code-block-not-checked` warning, at `at`, for a root that pins no toolchain, rustup
/// couldn't name one for it because `why`, and `help` says what to change.
fn unresolved_finding(why: &str, help: String, at: &Location) -> Finding {
    Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        format!("the project pins no Rust toolchain and {why}, so its Rust blocks aren't checked"),
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

/// The warnings about the attributes written in `info`: unknown ones, step values no tool
/// accepts and missing reasons.
fn attribute_findings(block: &CodeBlock, info: &Info, code_blocks: &CodeBlocks) -> Vec<Finding> {
    let mut found = unknown_attributes(info);
    found.extend(step_values(block, info, code_blocks));
    found.extend(missing_reasons(info));
    found
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
                    (None, None) if is_fragment_word(attribute) => {
                        format!("write `fragment(<kind>)` with one of {}", Kind::listed())
                    }
                    (None, None) => {
                        format!("remove `{attribute}` or correct it to one of {KNOWN_ATTRIBUTES}")
                    }
                },
            )
        });
    let repeated = parsed
        .attributes
        .iter()
        .filter(|attribute| fragment_kind(attribute).is_some())
        .skip(1)
        .map(|attribute| {
            Finding::new(
                UNKNOWN_ATTRIBUTE,
                Severity::Warning,
                format!(
                    "the `{}` block's attribute `{attribute}` repeats its first `fragment(...)`",
                    parsed.language
                ),
            )
            .with_location(info.location.clone())
            .with_help(format!(
                "keep one `fragment(<kind>)`, naming one of {}",
                Kind::listed()
            ))
        });
    comma.into_iter().chain(unknown).chain(repeated).collect()
}

/// One `code-block-fragment-unsupported` warning at `info` for each of `steps` that didn't run
/// because its tool doesn't check the block's fragment kind, naming the tools of `language`
/// that do and how to turn the step off.
fn fragment_unsupported(block: &CodeBlock, steps: &Steps) -> Vec<Finding> {
    let Some(info) = block.info.as_ref() else {
        return Vec::new();
    };
    let tag = tag_of(block).unwrap_or_default();
    let language = Language::from_tag(tag);
    [(Step::Format, &steps.format), (Step::Lint, &steps.lint)]
        .into_iter()
        .filter_map(|(step, state)| {
            let StepState::Unsupported { tool, kind } = state else {
                return None;
            };
            let checking: Vec<String> = tools::CARTRIDGES
                .iter()
                .filter(|other| Some(other.language) == language && other.supports(*kind, tag))
                .map(|other| format!("`{}`", other.name))
                .collect();
            let key = step.key();
            let off = format!("write `{key}(off: <why>)` to skip this step");
            let help = if checking.is_empty() {
                format!("no tool checks a `{}` fragment here: {off}", kind.name())
            } else {
                format!(
                    "{} check a `{}` fragment: select one, or {off}",
                    checking.join(", "),
                    kind.name()
                )
            };
            Some(
                Finding::new(
                    FRAGMENT_UNSUPPORTED,
                    Severity::Warning,
                    format!(
                        "the {key} step didn't run: {}",
                        unsupported_reason(tool, *kind)
                    ),
                )
                .with_location(info.location.clone())
                .with_help(help),
            )
        })
        .collect()
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
        BlockRecord, Evaluation, Linked, LintRan, MANIFEST_AMBIGUOUS, MANIFEST_OUTSIDE,
        MANIFEST_UNREADABLE, MAX_WORKERS, OUTSIDE_WORKSPACE, PROJECT_UNBUILT, Pending, RustSetup,
        Session, in_parallel, refused_build, run_clippy, rust_refusal, unresolved_warning,
    };
    use crate::cartridges::markdown::cartridge::cartridge as markdown;
    use crate::code_blocks::block::blocks_in;
    use crate::code_blocks::format::FixDoc;
    use crate::code_blocks::lint::Verdict;
    use crate::code_blocks::project::{OutsideWorkspace, Refused};
    use crate::code_blocks::states::{BlockState, NotChecked, Steps};
    use crate::code_blocks::states::{Failure, StepState};
    use crate::code_blocks::toolchain::Refusal;
    use crate::code_blocks::tools::{CARTRIDGES, Found, Invocation, ToolCartridge, ToolEnv, find};
    use crate::config::code_blocks::{Dialect, Edition};
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
        rust_records(root, text)
            .into_iter()
            .next()
            .expect("the doc should hold one block")
    }

    /// The records of each Rust block in `text`, a doc at `<root>/docs/a.md`, pending with clippy
    /// as their linter.
    fn rust_records(root: &Path, text: &str) -> Vec<BlockRecord> {
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
        let doc = Arc::new(FixDoc::new(
            Arc::from(text),
            "10s".parse().expect("the parse limit should be valid"),
        ));
        blocks
            .iter()
            .enumerate()
            .map(|(position, block)| BlockRecord {
                at: block.fence.clone(),
                language: Some("rust".to_owned()),
                state: BlockState::NotChecked(NotChecked::Pending),
                steps: Steps::default(),
                fragment: None,
                reason: None,
                uses: Vec::new(),
                index: position + 1,
                tool: None,
                linter: Some(clippy()),
                dialect: Dialect::default(),
                edition: None,
                given: None,
                pending: Pending::of(block, &doc),
            })
            .collect()
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
            linked: Linked::default(),
        };
        run_clippy(
            clippy(),
            program,
            &record,
            pending,
            &session,
            &RustSetup::read(&env, root, Some(Duration::from_secs(120))),
        )
    }

    /// A temporary directory.
    fn temporary() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("a temporary directory should be creatable")
    }

    /// The probe `e2_rawcstr_2018_leak.rs`, its marker path a fixed non-existent one: before
    /// edition 2021 `cr#"` is `cr`, `#` and a string, which hides an `include!` from a scan that
    /// reads it as a raw C string.
    const E2_PROBE: &str = "pub const A: &str = stringify!(cr#\" \");\n\
                            include!(\"/nonexistent/supdoc-probe/secret.rs\"); // \"#;\n";

    /// The probe's record with `edition` as its attribute's, under a project at
    /// `project_edition`: the scan's verdict for it with clippy as its linter, and the
    /// `--edition` pair in the argv `run_clippy` builds for it.
    fn scan_and_argv(
        attribute: Option<Edition>,
        project_edition: &str,
    ) -> (Option<(NotChecked, Option<String>)>, Vec<String>) {
        let root = temporary();
        std::fs::write(
            root.path().join("Cargo.toml"),
            format!("[package]\nname = \"p\"\nedition = \"{project_edition}\"\n"),
        )
        .expect("the manifest should be writable");
        let mut record = rust_record(root.path(), &format!("```rust\n{E2_PROBE}```\n"));
        record.edition = attribute;
        let rust = RustSetup::read(&ToolEnv::default(), root.path(), None);
        let refusal = rust_refusal(&record, &rust, true, &mut false, &mut Vec::new());
        let doc = root.path().join("docs/a.md");
        let args = clippy().args(&Invocation {
            doc: &doc,
            tag: "",
            index: record.index,
            edition: rust.edition_of(&record),
            dialect: record.dialect,
            project: &rust.lints,
            emit: None,
            externs: &[],
            fragment: None,
        });
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        (refusal, args)
    }

    /// Asserts that the probe is refused as a compile-time read and clippy gets `--edition 2018`.
    fn assert_read_as_2018(attribute: Option<Edition>, project_edition: &str) {
        let (refusal, args) = scan_and_argv(attribute, project_edition);

        assert!(
            matches!(refusal, Some((NotChecked::CompileTimeRead, _))),
            "{refusal:?}"
        );
        assert!(
            args.windows(2).any(|pair| pair == ["--edition", "2018"]),
            "{args:?}"
        );
    }

    #[test]
    fn the_scan_reads_a_block_as_its_attribute_edition_like_clippy() {
        assert_read_as_2018(Some(Edition::E2018), "2024");
    }

    #[test]
    fn the_scan_reads_a_block_as_the_project_edition_like_clippy() {
        assert_read_as_2018(None, "2018");
    }

    #[test]
    fn a_2021_block_of_the_probe_is_not_refused_the_2018_way_and_gets_2021() {
        let (refusal, args) = scan_and_argv(Some(Edition::E2021), "2018");

        assert!(
            !matches!(refusal, Some((NotChecked::CompileTimeRead, _))),
            "{refusal:?}"
        );
        assert!(
            args.windows(2).any(|pair| pair == ["--edition", "2021"]),
            "{args:?}"
        );
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
            build_timeout: None,
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

    /// Writes an executable `sh` script at `path` running `body`.
    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}")).expect("the stub should be writable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
    }

    /// A project root with a stub `cargo` and `clippy-driver` beside it, each recording its runs.
    struct Linking {
        /// Holds the root, the stubs, their records and the cache.
        dir: tempfile::TempDir,
    }

    impl Linking {
        /// A root whose `Cargo.toml` is `manifest` (none when `None`) with a `src/lib.rs`, and
        /// stubs where cargo, after `before`, writes `rmeta`'s bytes as the `proj` lib's rmeta
        /// and prints its artifact line, and clippy-driver records its argv and passes.
        fn new(manifest: Option<&str>, before: &str) -> Self {
            let linking = Self { dir: temporary() };
            let root = linking.root();
            for dir in [root.join("src"), linking.bin(), linking.records()] {
                std::fs::create_dir_all(dir).expect("the fixture directories should be creatable");
            }
            std::fs::write(root.join("src/lib.rs"), "").expect("the lib should be writable");
            if let Some(manifest) = manifest {
                std::fs::write(root.join("Cargo.toml"), manifest)
                    .expect("the manifest should be writable");
            }
            linking.set_rmeta("rmeta");
            let records = linking.records();
            let real_manifest = root
                .canonicalize()
                .expect("the root should resolve")
                .join("Cargo.toml");
            script(
                &linking.bin().join("cargo"),
                &format!(
                    "echo build >> '{records}/cargo-runs'\n\
                     echo \"$RUSTUP_TOOLCHAIN|$RUSTUP_AUTO_INSTALL\" >> '{records}/cargo-toolchain'\n\
                     {before}\n\
                     deps=\"$CARGO_TARGET_DIR/debug/deps\"\n/bin/mkdir -p \"$deps\"\n\
                     /bin/cat '{records}/rmeta' > \"$deps/libproj-1.rmeta\"\n\
                     printf '{{\"reason\":\"compiler-artifact\",\"manifest_path\":\"{manifest}\",\
                     \"target\":{{\"name\":\"proj\",\"kind\":[\"lib\"]}},\
                     \"filenames\":[\"%s\"]}}\\n' \"$deps/libproj-1.rmeta\"\n",
                    records = records.display(),
                    manifest = real_manifest.display(),
                ),
            );
            script(
                &linking.bin().join("clippy-driver"),
                &format!(
                    "echo \"$*\" >> '{records}/clippy-runs'\n\
                     echo \"$RUSTUP_TOOLCHAIN|$RUSTUP_AUTO_INSTALL\" >> '{records}/clippy-toolchain'\n",
                    records = records.display()
                ),
            );
            linking
        }

        /// The project root.
        fn root(&self) -> PathBuf {
            self.dir.path().join("root")
        }

        /// The directory the stubs are in, the only `PATH` entry.
        fn bin(&self) -> PathBuf {
            self.dir.path().join("bin")
        }

        /// The directory the stubs record into.
        fn records(&self) -> PathBuf {
            self.dir.path().join("records")
        }

        /// Makes the stub cargo write `bytes` as the lib's rmeta from its next run.
        fn set_rmeta(&self, bytes: &str) {
            std::fs::write(self.records().join("rmeta"), bytes)
                .expect("the rmeta bytes should be writable");
        }

        /// The lines the stub `name` recorded, one per run.
        fn runs(&self, name: &str) -> Vec<String> {
            std::fs::read_to_string(self.records().join(name))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// The environment the tools get: the stubs alone on `PATH`, and a cache in the fixture.
        fn env(&self) -> ToolEnv {
            let bin = self.bin().into_os_string();
            let cache = self.dir.path().join("cache").into_os_string();
            let home = self.dir.path().join("home").into_os_string();
            ToolEnv::from_vars(|name| match name {
                "PATH" => Some(bin.clone()),
                "XDG_CACHE_HOME" => Some(cache.clone()),
                "HOME" => Some(home.clone()),
                _ => None,
            })
        }

        /// [`Evaluation::look_up_tools`] on two trusted Rust blocks, clippy their linter as if the
        /// T6 hold were lifted: the evaluation after it.
        fn look_up(&self) -> Evaluation {
            self.look_up_blocks(
                "# D\n\n```rust\npub fn a() {}\n```\n\n```rust\npub fn b() {}\n```\n",
                None,
                Duration::from_secs(120),
            )
        }

        /// [`Evaluation::look_up_tools`] on the Rust blocks of `doc`, clippy their linter as if the
        /// T6 hold were lifted and `formatter` their formatter, the build under `build_timeout`.
        fn look_up_blocks(
            &self,
            doc: &str,
            formatter: Option<&'static ToolCartridge>,
            build_timeout: Duration,
        ) -> Evaluation {
            let root = self.root();
            let mut records = rust_records(&root, doc);
            for record in &mut records {
                record.tool = formatter;
            }
            let mut evaluation = Evaluation {
                findings: Vec::new(),
                counts: None,
                records,
                timeout: Some(Duration::from_secs(120)),
                build_timeout: Some(build_timeout),
            };
            evaluation.look_up_tools(&self.env(), &root);
            evaluation
        }

        /// A stub `rustup` beside the others that records each run and answers `printed` with
        /// exit status `status`, as `rustup show active-toolchain` does.
        fn rustup(&self, printed: &str, status: u8) {
            script(
                &self.bin().join("rustup"),
                &format!(
                    "echo \"$(pwd) $* $RUSTUP_AUTO_INSTALL\" >> '{}/rustup-runs'\n\
                     /bin/cat <<'ANSWER'\n{printed}ANSWER\nexit {status}\n",
                    self.records().display()
                ),
            );
        }

        /// The build's dependency directory, by real path.
        fn deps(&self) -> PathBuf {
            crate::code_blocks::project::target_dir(&self.env(), &self.root())
                .expect("the target dir should be usable")
                .path()
                .join("debug/deps")
                .canonicalize()
                .expect("the stub's deps dir should resolve")
        }
    }

    /// The finding codes of `evaluation`, in order.
    fn codes(evaluation: &Evaluation) -> Vec<&str> {
        evaluation
            .findings
            .iter()
            .map(|finding| finding.code)
            .collect()
    }

    /// A root package with a lib, built on its own.
    const PROJ: &str = "[package]\nname = \"proj\"\n\n[workspace]\n";

    #[test]
    fn trusted_blocks_link_against_one_project_build_with_its_externs_before_the_lints() {
        let linking = Linking::new(Some(PROJ), "");

        let evaluation = linking.look_up();

        assert_eq!(linking.runs("cargo-runs"), ["build"], "one build per run");
        let deps = linking.deps();
        let linked = format!(
            "--error-format=json --extern proj={} -L dependency={} -A unused",
            deps.join("libproj-1.rmeta").display(),
            deps.display()
        );
        let clippy = linking.runs("clippy-runs");
        assert_eq!(clippy.len(), 2, "{clippy:?}");
        for argv in &clippy {
            assert!(argv.contains(&linked), "{argv}\nshould hold {linked}");
        }
        assert!(codes(&evaluation).is_empty(), "{:?}", evaluation.findings);
        for record in &evaluation.records {
            assert_eq!(record.state, BlockState::Passed, "{record:?}");
        }
    }

    #[test]
    fn a_changed_project_artifact_changes_the_cache_key() {
        let linking = Linking::new(Some(PROJ), "");

        let _first = linking.look_up();
        let _cached = linking.look_up();
        let cached = linking.runs("clippy-runs").len();
        linking.set_rmeta("rmeta, rebuilt");
        let _rebuilt = linking.look_up();

        assert_eq!(
            cached, 2,
            "an unchanged build should reuse both blocks' runs"
        );
        assert_eq!(linking.runs("cargo-runs").len(), 3, "one build per run");
        assert_eq!(
            linking.runs("clippy-runs").len(),
            4,
            "a changed artifact should run both blocks again"
        );
    }

    #[test]
    fn a_failing_project_build_is_one_tool_failed_error_and_fails_each_block() {
        let linking = Linking::new(Some(PROJ), "echo 'error: lib.rs: boom' >&2\nexit 101");

        let evaluation = linking.look_up();

        assert_eq!(codes(&evaluation), ["code-block-tool-failed"]);
        let failed = evaluation
            .findings
            .first()
            .expect("the build failure should be reported");
        assert!(failed.message.contains("`cargo check`"), "{failed:?}");
        assert!(failed.message.contains("boom"), "{failed:?}");
        assert_eq!(evaluation.records.len(), 2);
        for record in &evaluation.records {
            assert_eq!(
                record.state,
                BlockState::Failed(Failure::ToolFailed),
                "{record:?}"
            );
            assert_eq!(record.reason.as_deref(), Some(PROJECT_UNBUILT));
            assert_eq!(
                record.steps.lint,
                StepState::Unrunnable {
                    reason: PROJECT_UNBUILT
                }
            );
        }
        assert_eq!(linking.runs("cargo-runs"), ["build"]);
        assert!(linking.runs("clippy-runs").is_empty(), "clippy never runs");
    }

    #[test]
    fn with_no_cargo_toml_a_block_compiles_alone_and_cargo_never_runs() {
        let linking = Linking::new(None, "");

        let evaluation = linking.look_up();

        assert_eq!(linking.runs("cargo-runs"), [] as [String; 0]);
        let clippy = linking.runs("clippy-runs");
        assert_eq!(clippy.len(), 2, "{clippy:?}");
        for argv in &clippy {
            assert!(
                argv.contains("--error-format=json -A unused"),
                "today's argv exactly: {argv}"
            );
            assert!(
                !argv.contains("--extern") && !argv.contains("-L "),
                "{argv}"
            );
        }
        assert!(codes(&evaluation).is_empty(), "{:?}", evaluation.findings);
    }

    #[test]
    fn a_refused_manifest_leaves_clippy_unrun_with_one_warning() {
        let linking = Linking::new(
            Some("[package]\nname = \"proj\"\n\n[project]\nname = \"proj\"\n"),
            "",
        );

        let evaluation = linking.look_up();

        assert_eq!(codes(&evaluation), ["code-block-not-checked"]);
        for record in &evaluation.records {
            assert_eq!(
                record.steps.lint,
                StepState::Unrunnable {
                    reason: MANIFEST_AMBIGUOUS
                },
                "{record:?}"
            );
            assert_ne!(record.state, BlockState::Passed, "{record:?}");
            assert_eq!(record.reason.as_deref(), Some(MANIFEST_AMBIGUOUS));
        }
        assert_eq!(linking.runs("cargo-runs"), [] as [String; 0]);
        assert_eq!(linking.runs("clippy-runs"), [] as [String; 0]);
    }

    #[test]
    fn a_missing_cargo_refuses_the_lint_step_with_one_warning() {
        let linking = Linking::new(Some(PROJ), "");
        std::fs::remove_file(linking.bin().join("cargo")).expect("the stub should be removable");

        let evaluation = linking.look_up();

        assert_eq!(codes(&evaluation), ["code-block-tool-missing"]);
        let missing = evaluation
            .findings
            .first()
            .expect("cargo's warning should be reported");
        assert!(missing.message.contains("`cargo`"), "{missing:?}");
        for record in &evaluation.records {
            assert_eq!(
                record.steps.lint,
                StepState::ToolRefused {
                    binary: "cargo",
                    inside_root: false
                },
                "{record:?}"
            );
        }
        assert_eq!(linking.runs("clippy-runs"), [] as [String; 0]);
    }

    #[test]
    fn each_refused_manifest_has_its_own_reason_and_a_warning_with_help() {
        let at = rust_record(Path::new("/p"), "```rust\npub fn f() {}\n```\n").at;
        let outside = PathBuf::from("/w/Cargo.toml");
        let cases = [
            (
                Refused::Manifest(Refusal::Outside),
                MANIFEST_OUTSIDE,
                "outside the project",
            ),
            (
                Refused::Manifest(Refusal::Unreadable),
                MANIFEST_UNREADABLE,
                "64 KiB",
            ),
            (Refused::Ambiguous, MANIFEST_AMBIGUOUS, "`[project]`"),
            (
                Refused::Workspace(OutsideWorkspace::Ancestor(outside)),
                OUTSIDE_WORKSPACE,
                "/w/Cargo.toml",
            ),
        ];

        for (refused, expected, named) in cases {
            let (reason, warning) = refused_build(&refused, &at);

            assert_eq!(reason, expected, "{refused:?}");
            assert_eq!(warning.code, "code-block-not-checked", "{refused:?}");
            assert!(warning.message.contains(named), "{warning:?}");
            assert!(
                warning
                    .message
                    .ends_with("so clippy doesn't lint the project's Rust blocks"),
                "{warning:?}"
            );
            assert!(
                warning.help.as_deref().is_some_and(|help| !help.is_empty()),
                "{warning:?}"
            );
        }
    }

    /// A rustup answer naming a channel for a parent's pin.
    const PARENT_CHANNEL: &str =
        "1.85.0-x86_64-unknown-linux-gnu (overridden by '/w/rust-toolchain.toml')\n";

    #[test]
    fn a_parent_channel_rustup_names_is_pinned_for_cargo_and_clippy_alike() {
        let linking = Linking::new(Some(PROJ), "");
        std::fs::write(
            linking.dir.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.85.0\"\n",
        )
        .expect("the parent's toolchain file should be writable");
        linking.rustup(PARENT_CHANNEL, 0);

        let evaluation = linking.look_up();

        let root = linking
            .root()
            .canonicalize()
            .expect("the root should resolve");
        assert_eq!(
            linking.runs("rustup-runs"),
            [format!("{} show active-toolchain 0", root.display())],
            "rustup runs once, from the root, never installing"
        );
        let pinned = "1.85.0-x86_64-unknown-linux-gnu|0";
        assert_eq!(linking.runs("cargo-toolchain"), [pinned]);
        assert_eq!(linking.runs("clippy-toolchain"), [pinned, pinned]);
        assert!(codes(&evaluation).is_empty(), "{:?}", evaluation.findings);
    }

    #[test]
    fn a_parent_path_toolchain_never_runs_its_cargo_and_leaves_the_blocks_unchecked() {
        let linking = Linking::new(Some(PROJ), "");
        let evil = linking.dir.path().join("evil");
        std::fs::create_dir_all(evil.join("bin")).expect("the path toolchain should be creatable");
        script(
            &evil.join("bin/cargo"),
            &format!(
                "echo build >> '{}/evil-cargo-runs'\n",
                linking.records().display()
            ),
        );
        std::fs::write(
            linking.dir.path().join("rust-toolchain.toml"),
            "[toolchain]\npath = \"evil\"\n",
        )
        .expect("the parent's toolchain file should be writable");
        linking.rustup(
            &format!(
                "{} (overridden by '{}')\n",
                evil.display(),
                linking.dir.path().join("rust-toolchain.toml").display()
            ),
            0,
        );

        let evaluation = linking.look_up();

        assert_eq!(codes(&evaluation), ["code-block-not-checked"]);
        let warning = evaluation
            .findings
            .first()
            .expect("the run's one warning should be reported");
        assert!(
            warning.message.contains("rustup show active-toolchain"),
            "{warning:?}"
        );
        for record in &evaluation.records {
            assert_eq!(
                record.state,
                BlockState::NotChecked(NotChecked::ToolchainUnresolved),
                "{record:?}"
            );
            assert_eq!(record.reason.as_deref(), Some("toolchain unresolved"));
        }
        for stub in ["evil-cargo-runs", "cargo-runs", "clippy-runs"] {
            assert_eq!(
                linking.runs(stub),
                [] as [String; 0],
                "{stub} should stay empty"
            );
        }
    }

    #[test]
    fn each_unresolved_cause_has_its_own_warning_quoting_rustup() {
        use crate::code_blocks::toolchain::{Cause, Unresolved};

        let root = temporary();
        let record = rust_records(root.path(), "# D\n\n```rust\npub fn a() {}\n```\n")
            .pop()
            .expect("the doc should hold one block");
        let said = "error: rustup's own words";
        let rustup = |cause| Unresolved::Rustup {
            cause,
            said: said.to_owned(),
        };
        let quoted = format!("(rustup said: `{said}`)");
        for (unresolved, message, help) in [
            (
                Unresolved::InsideRoot,
                "the `rustup` on PATH is inside the checkout".to_owned(),
                "put a `rustup` from outside the project first on PATH",
            ),
            (
                rustup(Cause::PathOrCustom),
                format!("named a path or custom toolchain, not a channel {quoted}"),
                "remove the `path` toolchain or override above the project",
            ),
            (
                rustup(Cause::NotInstalled),
                format!("isn't installed, and supdoc never installs one {quoted}"),
                "install it with `rustup toolchain install`",
            ),
            (
                rustup(Cause::Unparseable),
                format!("couldn't parse a toolchain file above it {quoted}"),
                "fix or remove that toolchain file",
            ),
            (
                rustup(Cause::NoDefault),
                format!("rustup has no default toolchain to give it {quoted}"),
                "set one with `rustup default stable`",
            ),
            (
                rustup(Cause::Failed),
                format!("`rustup show active-toolchain` failed {quoted}"),
                "in the project to see why,",
            ),
            (
                rustup(Cause::TimedOut),
                "ran past `[limits] code-block-timeout`".to_owned(),
                "raise `code-block-timeout`",
            ),
            (
                rustup(Cause::OverCap),
                "printed more than 1 MiB".to_owned(),
                "to see what it prints",
            ),
        ] {
            let warning = unresolved_warning(&unresolved, &record.at);
            assert_eq!(warning.code, "code-block-not-checked", "{warning:?}");
            assert!(
                warning
                    .message
                    .starts_with("the project pins no Rust toolchain and ")
                    && warning.message.contains(&message)
                    && warning
                        .message
                        .ends_with(", so its Rust blocks aren't checked"),
                "{unresolved:?}: {}",
                warning.message
            );
            let given = warning.help.as_deref().unwrap_or_default();
            assert!(given.contains(help), "{unresolved:?}: {given}");
            if unresolved != Unresolved::InsideRoot {
                assert!(given.contains("`rust-toolchain.toml`"), "{given}");
            }
        }
        let silent = Unresolved::Rustup {
            cause: Cause::Failed,
            said: String::new(),
        };
        let warning = unresolved_warning(&silent, &record.at);
        assert!(
            !warning.message.contains("rustup said"),
            "nothing to quote: {}",
            warning.message
        );
    }

    #[test]
    fn a_failing_or_inside_rustup_leaves_the_blocks_unchecked_and_nothing_cached() {
        let linking = Linking::new(Some(PROJ), "");
        linking.rustup("stable-aarch64-apple-darwin (default)\n", 1);

        let _first = linking.look_up();
        let second = linking.look_up();

        assert_eq!(linking.runs("rustup-runs").len(), 2, "once per run");
        assert_eq!(codes(&second), ["code-block-not-checked"]);
        for record in &second.records {
            assert_eq!(
                record.state,
                BlockState::NotChecked(NotChecked::ToolchainUnresolved),
                "{record:?}"
            );
        }
        assert_eq!(linking.runs("cargo-runs"), [] as [String; 0]);
        assert_eq!(linking.runs("clippy-runs"), [] as [String; 0]);

        let inside = Linking::new(Some(PROJ), "");
        let bin = inside.root().join("bin");
        std::fs::create_dir_all(&bin).expect("the in-root bin should be creatable");
        script(&bin.join("rustup"), "echo stable\n");
        std::fs::rename(inside.bin().join("cargo"), bin.join("unused"))
            .expect("the stub should be movable");
        let env = ToolEnv::from_vars(|name| match name {
            "PATH" => Some(bin.clone().into_os_string()),
            _ => None,
        });
        let rust = RustSetup::read(&env, &inside.root(), None);
        assert_eq!(
            rust.toolchain,
            crate::code_blocks::toolchain::Toolchain::Unresolved(
                crate::code_blocks::toolchain::Unresolved::InsideRoot
            )
        );
    }

    #[test]
    fn no_rustup_pins_nothing_and_the_resolved_channel_keys_the_cache() {
        let linking = Linking::new(Some(PROJ), "");

        let _unpinned = linking.look_up();
        assert_eq!(linking.runs("cargo-toolchain"), ["|0"], "no rustup, no pin");
        linking.rustup(PARENT_CHANNEL, 0);
        let _pinned = linking.look_up();
        let _cached = linking.look_up();

        assert_eq!(
            linking.runs("clippy-runs").len(),
            4,
            "a new channel should run both blocks again, and the same one reuse them"
        );
    }

    #[test]
    fn a_block_the_strict_scan_refuses_starts_no_project_build() {
        let linking = Linking::new(Some(PROJ), "");

        let evaluation = linking.look_up_blocks(
            "# D\n\n```rust\nconst S: &str = include_str!(\"/etc/hosts\");\n```\n",
            None,
            Duration::from_secs(120),
        );

        assert_eq!(linking.runs("cargo-runs"), [] as [String; 0]);
        assert_eq!(linking.runs("clippy-runs"), [] as [String; 0]);
        assert_eq!(
            evaluation.records.first().map(|record| &record.state),
            Some(&BlockState::NotChecked(NotChecked::CompileTimeRead))
        );
    }

    #[test]
    fn a_timed_out_build_is_one_timeout_error_fails_each_block_and_caches_nothing() {
        let linking = Linking::new(Some(PROJ), "/bin/sleep 5");
        let doc = "# D\n\n```rust\npub fn a() {}\n```\n\n```rust\npub fn b() {}\n```\n";

        let first = linking.look_up_blocks(doc, None, Duration::from_millis(200));
        let _second = linking.look_up_blocks(doc, None, Duration::from_millis(200));

        assert_eq!(codes(&first), ["code-block-tool-timeout"]);
        for record in &first.records {
            assert_eq!(
                record.state,
                BlockState::Failed(Failure::ToolTimeout),
                "{record:?}"
            );
        }
        assert_eq!(linking.runs("cargo-runs").len(), 2, "nothing is cached");
        assert_eq!(linking.runs("clippy-runs"), [] as [String; 0]);
    }

    #[test]
    fn a_refused_manifest_keeps_the_format_verdict_and_never_passes_the_lint_step() {
        let linking = Linking::new(
            Some("[package]\nname = \"proj\"\n\n[project]\nname = \"proj\"\n"),
            "",
        );
        script(&linking.bin().join("rustfmt"), "/bin/cat\n");
        let rustfmt = CARTRIDGES
            .iter()
            .find(|tool| tool.name == "rustfmt")
            .expect("the cartridges should include rustfmt");

        let evaluation = linking.look_up_blocks(
            "# D\n\n```rust\npub fn a() {}\n```\n",
            Some(rustfmt),
            Duration::from_secs(120),
        );

        assert_eq!(codes(&evaluation), ["code-block-not-checked"]);
        let record = evaluation
            .records
            .first()
            .expect("the doc should hold one block");
        assert_eq!(record.steps.format, StepState::Ran, "{record:?}");
        assert_eq!(
            record.steps.lint,
            StepState::Unrunnable {
                reason: MANIFEST_AMBIGUOUS
            },
            "{record:?}"
        );
        assert_eq!(
            record.state,
            BlockState::Passed,
            "the format verdict decides it"
        );
        assert_eq!(linking.runs("cargo-runs"), [] as [String; 0]);
        assert_eq!(linking.runs("clippy-runs"), [] as [String; 0]);
    }

    /// The format step on a block of a kind its formatter reads only inside a frame.
    mod frame_format {
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::{Path, PathBuf};
        use std::time::Duration;

        use super::super::{
            Evaluation, FRAME_INDENT_UNKNOWN, FRAME_RESHAPED, FormatRan, Job, RustSetup,
            RustfmtLayout, Session, Slot, StepsRan, ToolRun, biome_indent, parses, run_formatter,
        };
        use super::{rust_record, rust_records, temporary};
        use crate::code_blocks::biome::Given;
        use crate::code_blocks::format::Verdict;
        use crate::code_blocks::fragment::{Frame, Kind};
        use crate::code_blocks::states::StepState;
        use crate::code_blocks::toolchain::Toolchain;
        use crate::code_blocks::tools::{ToolEnv, formatter};
        use crate::config::code_blocks::Language;
        use crate::finding::Finding;

        /// A doc whose one Rust block declares `fragment(statements)` and holds `body`.
        fn doc(body: &str) -> String {
            format!("# D\n\n```rust fragment(statements)\n{body}```\n")
        }

        /// [`run_formatter`] with `program` as framed rustfmt on the one block of `text` under
        /// `root`, with the test process's environment.
        fn format_run(program: &Path, root: &Path, text: &str) -> FormatRan {
            format_run_cached(program, root, text, None)
        }

        /// [`format_run`], with `XDG_CACHE_HOME`, which holds the tools' neutral working
        /// directory, at `cache` when given.
        fn format_run_cached(
            program: &Path,
            root: &Path,
            text: &str,
            cache: Option<&Path>,
        ) -> FormatRan {
            let mut record = rust_record(root, text);
            record.fragment = Some(Kind::Statements);
            record.linter = None;
            let env = ToolEnv::from_vars(|name| match cache {
                Some(cache) if name == "XDG_CACHE_HOME" => Some(cache.as_os_str().to_owned()),
                _ => std::env::var_os(name),
            });
            let session = Session {
                env: &env,
                root,
                timeout: Some(Duration::from_secs(120)),
                cache: None,
                linked: super::Linked::default(),
            };
            let rust = RustSetup::read(&env, root, Some(Duration::from_secs(120)));
            let rustfmt = formatter(Language::Rust).expect("Rust has a formatter");
            let frame = Frame::of(rustfmt, Some(Kind::Statements));
            assert!(frame.is_some(), "rustfmt reads statements inside a frame");
            run_formatter(rustfmt, program, &record, frame, &session, Some(&rust))
                .expect("the block's body should be in its doc")
        }

        /// A stub formatter at `<dir>/rustfmt` running `body`.
        fn stub(dir: &Path, body: &str) -> PathBuf {
            let path = dir.join("rustfmt");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))
                .expect("the stub should be writable");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("the stub should be executable");
            path
        }

        /// `text` with the one fix in `findings` applied.
        fn applied(text: &str, findings: &[Finding]) -> String {
            let fix = findings
                .iter()
                .find_map(|finding| finding.fix.as_deref())
                .expect("one finding should carry the fix");
            let mut out = text.to_owned();
            out.replace_range(fix.range.clone(), &fix.replacement);
            out
        }

        /// The real rustfmt on `PATH`, for the test named `test`. `None` skips the test when it
        /// is missing, unless `SUPDOC_REQUIRE_REAL_TOOLS` is set (CI sets it).
        fn real_rustfmt(test: &str) -> Option<PathBuf> {
            let found = std::env::var_os("PATH")
                .iter()
                .flat_map(std::env::split_paths)
                .map(|dir| dir.join("rustfmt"))
                .find(|path| path.is_absolute() && path.is_file());
            let required = std::env::var_os("SUPDOC_REQUIRE_REAL_TOOLS")
                .is_some_and(|value| !value.is_empty());
            assert!(
                found.is_some() || !required,
                "{test} needs the real `rustfmt` on PATH because SUPDOC_REQUIRE_REAL_TOOLS is set"
            );
            let note = match &found {
                Some(path) => format!("REAL TOOL {test}: runs {}\n", path.display()),
                None => format!("SKIPPED {test}: rustfmt is not on PATH\n"),
            };
            let _ignored = std::io::Write::write_all(&mut std::io::stderr(), note.as_bytes());
            found
        }

        /// The verdict and findings of a run that ran.
        fn ran(run: FormatRan) -> (Verdict, Vec<Finding>) {
            run.unwrap_or_else(|reason| panic!("the format step should run: {reason}"))
        }

        #[test]
        fn frame_format_the_formatter_reads_the_block_indented_inside_the_frame() {
            let root = temporary();
            let stub = stub(root.path(), "exec /bin/cat");

            let (verdict, findings) = ran(format_run(&stub, root.path(), &doc("let x = 1;\n")));

            assert_eq!(verdict, Verdict::Formatted, "{findings:?}");
            assert!(findings.is_empty(), "{findings:?}");
        }

        #[test]
        fn frame_format_the_fix_from_a_framed_run_holds_only_the_authors_lines() {
            let root = temporary();
            let stub = stub(root.path(), "exec /usr/bin/sed 's/x=1/x = 1/'");
            let text = doc("let x=1;\n");

            let (verdict, findings) = ran(format_run(&stub, root.path(), &text));

            assert_eq!(verdict, Verdict::Unformatted("let x = 1;\n".to_owned()));
            assert_eq!(applied(&text, &findings), doc("let x = 1;\n"));
        }

        #[test]
        fn frame_format_a_frame_failure_is_reported_at_the_info_string() {
            let root = temporary();
            let text = doc("let x = 1;\n");
            let lost = stub(root.path(), "/bin/cat >/dev/null; echo garbage");
            let (lost, lost_findings) = ran(format_run(&lost, root.path(), &text));
            let on_frame = stub(
                root.path(),
                "/bin/cat >/dev/null; printf 'error: expected item\\n --> <stdin>:1:1\\n' >&2; exit 1",
            );
            let (not_kind, not_kind_findings) = ran(format_run(&on_frame, root.path(), &text));

            assert_eq!(lost, Verdict::FrameReformatted(Kind::Statements));
            assert_eq!(not_kind, Verdict::NotKind(Kind::Statements));
            for finding in lost_findings.iter().chain(&not_kind_findings) {
                let at = finding
                    .location
                    .as_ref()
                    .expect("the finding has a location");
                assert_eq!(
                    (at.line, at.column),
                    (3, 4),
                    "{finding:?} should be at the info string, not the fence at column 1"
                );
            }
        }

        #[test]
        fn frame_format_a_block_that_isnt_its_kind_holds_back_its_lint_step() {
            assert!(!parses(Some(&Ok((
                Verdict::NotKind(Kind::Statements),
                Vec::new()
            )))));
            assert!(parses(Some(&Ok((
                Verdict::FrameReformatted(Kind::Statements),
                Vec::new()
            )))));
            assert!(parses(Some(&Err(FRAME_INDENT_UNKNOWN))));
        }

        #[test]
        fn frame_format_an_unreadable_indent_leaves_the_step_not_run() {
            let root = temporary();
            std::fs::write(root.path().join("rustfmt.toml"), "tab_spaces = \"two\"\n")
                .expect("the config should be writable");
            let stub = stub(root.path(), "exit 9");

            let run = format_run(&stub, root.path(), &doc("let x = 1;\n"));

            assert_eq!(run, Err(FRAME_INDENT_UNKNOWN));
        }

        /// The `--config` value framed rustfmt runs with under the rustfmt config `toml`.
        fn framed_config(toml: &str) -> Option<String> {
            let table: toml::Table = toml.parse().expect("the test config should parse");
            RustfmtLayout::of(&table).map(|layout| {
                layout
                    .framed_args()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .join(" ")
            })
        }

        #[test]
        fn frame_format_pins_each_width_heuristic_to_the_projects_own_value() {
            let at = |widths: [usize; 8]| {
                format!(
                    ",fn_call_width={},attr_fn_like_width={},struct_lit_width={},\
                     struct_variant_width={},array_width={},chain_width={},\
                     single_line_if_else_max_width={},single_line_let_else_max_width={}",
                    widths[0],
                    widths[1],
                    widths[2],
                    widths[3],
                    widths[4],
                    widths[5],
                    widths[6],
                    widths[7]
                )
            };
            let default = at([60, 70, 18, 35, 60, 60, 50, 50]);
            for (config, expected) in [
                (
                    "",
                    format!("--config hard_tabs=false,tab_spaces=4,max_width=104{default}"),
                ),
                (
                    "max_width = 102\n",
                    format!("--config hard_tabs=false,tab_spaces=4,max_width=106{default}"),
                ),
                (
                    "tab_spaces = 8\n",
                    format!("--config hard_tabs=false,tab_spaces=8,max_width=108{default}"),
                ),
                (
                    "max_width = 40\n",
                    format!("--config hard_tabs=false,tab_spaces=4,max_width=44{default}"),
                ),
                (
                    "max_width = 120\n",
                    format!(
                        "--config hard_tabs=false,tab_spaces=4,max_width=124{}",
                        at([72, 84, 22, 42, 72, 72, 60, 60])
                    ),
                ),
                (
                    "max_width = 115\n",
                    format!(
                        "--config hard_tabs=false,tab_spaces=4,max_width=119{}",
                        at([72, 84, 22, 42, 72, 72, 60, 60])
                    ),
                ),
                (
                    "max_width = 105\n",
                    format!(
                        "--config hard_tabs=false,tab_spaces=4,max_width=109{}",
                        at([66, 77, 20, 39, 66, 66, 55, 55])
                    ),
                ),
                (
                    "max_width = 110\n",
                    format!(
                        "--config hard_tabs=false,tab_spaces=4,max_width=114{}",
                        at([66, 77, 20, 39, 66, 66, 55, 55])
                    ),
                ),
                (
                    "max_width = 90\nuse_small_heuristics = \"Max\"\n",
                    format!(
                        "--config hard_tabs=false,tab_spaces=4,max_width=94{}",
                        at([90; 8])
                    ),
                ),
                (
                    "use_small_heuristics = \"Off\"\nchain_width = 30\n",
                    "--config hard_tabs=false,tab_spaces=4,max_width=104,chain_width=30".to_owned(),
                ),
                (
                    "max_width = 80\nfn_call_width = 90\n",
                    format!(
                        "--config hard_tabs=false,tab_spaces=4,max_width=84,fn_call_width=80{}",
                        at([0, 70, 18, 35, 60, 60, 50, 50])
                            .split_once(",attr")
                            .map(|(_, rest)| format!(",attr{rest}"))
                            .unwrap_or_default()
                    ),
                ),
            ] {
                assert_eq!(framed_config(config), Some(expected), "{config:?}");
            }
            for unreadable in [
                "use_small_heuristics = \"Wide\"\n",
                "use_small_heuristics = 1\n",
                "fn_call_width = \"wide\"\n",
                "max_width = 70000\n",
            ] {
                assert_eq!(framed_config(unreadable), None, "{unreadable:?}");
            }
        }

        #[test]
        fn frame_format_a_frame_shaping_option_reshapes_only_under_a_nightly_rustfmt() {
            let layout = |toml: &str| {
                let table: toml::Table = toml.parse().expect("the test config should parse");
                RustfmtLayout::of(&table).expect("the test config should be readable")
            };
            let channel = |name: &str| Toolchain::Channel(name.to_owned());
            for shaping in [
                "brace_style = \"AlwaysNextLine\"\n",
                "brace_style = \"PreferSameLine\"\n",
                "fn_single_line = true\n",
                "blank_lines_lower_bound = 1\n",
            ] {
                assert!(layout(shaping).reshapes_frame, "{shaping:?}");
                for nightly in ["nightly", "nightly-2026-08-15"] {
                    assert!(
                        layout(shaping).under(&channel(nightly)).reshapes_frame,
                        "{shaping:?} under {nightly}"
                    );
                }
                for stable in ["stable", "beta", "1.93", "1.93.0"] {
                    assert!(
                        !layout(shaping).under(&channel(stable)).reshapes_frame,
                        "{shaping:?} under {stable}"
                    );
                }
            }
            for plain in [
                "",
                "brace_style = \"SameLineWhere\"\n",
                "fn_single_line = false\n",
                "blank_lines_lower_bound = 0\n",
                "control_brace_style = \"AlwaysNextLine\"\n",
            ] {
                assert!(
                    !layout(plain).under(&channel("nightly")).reshapes_frame,
                    "{plain:?}"
                );
            }
        }

        #[test]
        fn frame_format_a_nightly_frame_shaping_config_leaves_the_step_not_run_and_warns_once() {
            let root = temporary();
            std::fs::write(
                root.path().join("rustfmt.toml"),
                "brace_style = \"AlwaysNextLine\"\n",
            )
            .expect("the config should be writable");
            std::fs::write(
                root.path().join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"nightly\"\n",
            )
            .expect("the toolchain file should be writable");
            let stub = stub(root.path(), "exit 9");

            let run = format_run(&stub, root.path(), &doc("let x = 1;\n"));

            assert_eq!(run, Err(FRAME_RESHAPED), "the stub never runs");
            let text = doc("let x = 1;\n") + &doc("let y = 2;\n");
            let rustfmt = formatter(Language::Rust).expect("Rust has a formatter");
            let mut evaluation = Evaluation {
                records: rust_records(root.path(), &text),
                ..Evaluation::default()
            };
            let jobs: Vec<Job> = (0..2)
                .map(|record| Job {
                    record,
                    format: Some(ToolRun {
                        tool: rustfmt,
                        program: PathBuf::from("/bin/rustfmt"),
                    }),
                    lint: None,
                    unbuilt: None,
                })
                .collect();
            let unrun = || {
                Some(StepsRan {
                    format: Some(Err(FRAME_RESHAPED)),
                    lint: None,
                })
            };

            evaluation.settle(
                vec![Slot::Run(0), Slot::Run(1)],
                &jobs,
                vec![unrun(), unrun()],
            );

            let warnings: Vec<_> = evaluation
                .findings
                .iter()
                .map(|finding| (finding.code, finding.message.as_str()))
                .collect();
            assert_eq!(
                warnings,
                [(
                    "code-block-not-checked",
                    "the project's rustfmt settings reshape the frame supdoc puts around a \
                     fragment, so the format step didn't run on blocks rustfmt reads inside a \
                     frame"
                )]
            );
            for record in &evaluation.records {
                assert_eq!(
                    record.steps.format,
                    StepState::Unrunnable {
                        reason: FRAME_RESHAPED
                    }
                );
            }
        }

        #[test]
        fn frame_format_biome_indent_follows_its_config_and_refuses_overrides() {
            let dir = temporary();
            let given = |config: &str| {
                let path = dir.path().join("biome.json");
                std::fs::write(&path, config).expect("the config should be writable");
                Given {
                    flags: Vec::new(),
                    config: Some(path),
                    mirror: None,
                    names: None,
                }
            };
            let indent = |config: &str| biome_indent(Language::TypeScript, Some(&given(config)));

            assert_eq!(
                biome_indent(Language::TypeScript, None).as_deref(),
                Some("\t")
            );
            assert_eq!(indent("{}").as_deref(), Some("\t"));
            assert_eq!(
                indent(r#"{"formatter":{"indentStyle":"space"}}"#).as_deref(),
                Some("  ")
            );
            assert_eq!(
                indent(
                    r#"{"formatter":{"indentStyle":"tab"},
                        "javascript":{"formatter":{"indentStyle":"space","indentWidth":4}}}"#
                )
                .as_deref(),
                Some("    ")
            );
            assert_eq!(indent(r#"{"overrides":[]}"#).as_deref(), Some("\t"));
            assert_eq!(
                indent(r#"{"overrides":[{"includes":["**/*.ts"],"linter":{"enabled":false}}]}"#)
                    .as_deref(),
                Some("\t"),
                "an override that sets no indent leaves the top-level one"
            );
            assert_eq!(
                indent(r#"{"overrides":[{"formatter":{"indentWidth":3}}]}"#),
                None
            );
            assert_eq!(
                indent(r#"{"overrides":[{"javascript":{"formatter":{"indentStyle":"space"}}}]}"#),
                None
            );
            assert_eq!(indent(r#"{"formatter":{"indentStyle":"wide"}}"#), None);
        }

        #[test]
        fn frame_format_real_rustfmt_keeps_a_literals_lines_exact() {
            let test = "frame_format_real_rustfmt_keeps_a_literals_lines_exact";
            let Some(rustfmt) = real_rustfmt(test) else {
                return;
            };
            let root = temporary();
            let continued = doc("let s = \"a\n    b\";\n");
            let raw = doc("let j = r#\"{\n    \"a\": 1\n}\"#;\n");
            let spaced = doc("let  s = \"a\n    b\";\n");

            let (continued, continued_findings) =
                ran(format_run(&rustfmt, root.path(), &continued));
            let (raw, raw_findings) = ran(format_run(&rustfmt, root.path(), &raw));
            let (_, spaced_findings) = ran(format_run(&rustfmt, root.path(), &spaced));

            assert_eq!(continued, Verdict::Formatted, "{continued_findings:?}");
            assert_eq!(raw, Verdict::Formatted, "{raw_findings:?}");
            assert_eq!(
                applied(&spaced, &spaced_findings),
                doc("let s = \"a\n    b\";\n"),
                "the fix changes the spacing and never the string's value"
            );
        }

        #[test]
        fn frame_format_real_rustfmt_uses_the_projects_indent() {
            let test = "frame_format_real_rustfmt_uses_the_projects_indent";
            let Some(rustfmt) = real_rustfmt(test) else {
                return;
            };
            for (config, body) in [
                ("tab_spaces = 2\n", "if x {\n  y();\n}\n"),
                ("hard_tabs = true\n", "if x {\n\ty();\n}\n"),
                ("tab_spaces = 8\n", "if x {\n        y();\n}\n"),
            ] {
                let root = temporary();
                std::fs::write(root.path().join("rustfmt.toml"), config)
                    .expect("the config should be writable");

                let (verdict, findings) = ran(format_run(&rustfmt, root.path(), &doc(body)));

                assert_eq!(verdict, Verdict::Formatted, "{config}: {findings:?}");
                assert!(findings.is_empty(), "{config}: {findings:?}");
            }
        }

        #[test]
        fn frame_format_an_unreadable_indent_warns_once_and_leaves_each_step_not_run() {
            let root = temporary();
            let text = doc("let x = 1;\n") + &doc("let y = 2;\n");
            let rustfmt = formatter(Language::Rust).expect("Rust has a formatter");
            let mut evaluation = Evaluation {
                records: rust_records(root.path(), &text),
                ..Evaluation::default()
            };
            let jobs: Vec<Job> = (0..2)
                .map(|record| Job {
                    record,
                    format: Some(ToolRun {
                        tool: rustfmt,
                        program: PathBuf::from("/bin/rustfmt"),
                    }),
                    lint: None,
                    unbuilt: None,
                })
                .collect();
            let unrun = || {
                Some(StepsRan {
                    format: Some(Err(FRAME_INDENT_UNKNOWN)),
                    lint: None,
                })
            };

            evaluation.settle(
                vec![Slot::Run(0), Slot::Run(1)],
                &jobs,
                vec![unrun(), unrun()],
            );

            let warnings: Vec<_> = evaluation
                .findings
                .iter()
                .map(|finding| finding.code)
                .collect();
            assert_eq!(warnings, ["code-block-not-checked"]);
            for record in &evaluation.records {
                assert_eq!(
                    record.steps.format,
                    StepState::Unrunnable {
                        reason: FRAME_INDENT_UNKNOWN
                    }
                );
            }
        }

        #[test]
        fn frame_format_real_rustfmt_keeps_supdocs_indent_over_a_config_it_finds() {
            let test = "frame_format_real_rustfmt_keeps_supdocs_indent_over_a_config_it_finds";
            let Some(rustfmt) = real_rustfmt(test) else {
                return;
            };
            let root = temporary();
            let cache = temporary();
            std::fs::write(cache.path().join("rustfmt.toml"), "tab_spaces = 8\n")
                .expect("the config should be writable");
            let nested = doc("if x {\n    y();\n}\n");

            let (verdict, findings) = ran(format_run_cached(
                &rustfmt,
                root.path(),
                &nested,
                Some(cache.path()),
            ));

            assert_eq!(verdict, Verdict::Formatted, "{findings:?}");
            assert!(
                findings.is_empty(),
                "the fix never adds indent: {findings:?}"
            );
        }

        #[test]
        fn frame_format_real_rustfmt_parse_detail_names_the_blocks_position() {
            let test = "frame_format_real_rustfmt_parse_detail_names_the_blocks_position";
            let Some(rustfmt) = real_rustfmt(test) else {
                return;
            };
            let root = temporary();

            let (verdict, findings) = ran(format_run(
                &rustfmt,
                root.path(),
                &doc("let x = 1\nlet y = 2;\n"),
            ));

            let Verdict::ParseFailed { detail, line } = verdict else {
                panic!("a missing `;` is a parse failure: {findings:?}");
            };
            assert_eq!(line, Some(0));
            assert!(
                detail.contains("<stdin>:1:10") && detail.contains("1 | let x = 1"),
                "{detail}"
            );
            assert!(!detail.contains("supdoc_fragment"), "{detail}");
        }
    }
}
