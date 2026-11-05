//! One state per block, what its format and lint steps did, and the counts `check` reports for
//! them.

use std::collections::BTreeMap;

use annotate_snippets::renderer::{AnsiColor, Style};

use crate::config::code_blocks::{CodeBlocks, Language, Mode};

use super::block::CodeBlock;
use super::fragment::Kind;
use super::info::{StepValue, fragment_kind, reasoned, step_attribute};
use super::tools::{self, Hold, Step, Tool, formatter};

/// The one state a code block is in. A block takes the first that applies, in this order:
///
/// 1. A block with a `from(...)` attribute: [`Failed`](Self::Failed) when one of its sources is
///    missing, [`Stale`](Self::Stale) when its body differs from them, otherwise
///    [`Passed`](Self::Passed).
/// 2. `ignore(...)`: [`OptedOut`](Self::OptedOut), with its reason when it gives one.
/// 3. No tag, or `text`, `console` or `output`: [`NotChecked`] for [`NotChecked::Text`]. A fence
///    with no tag in a Rust doc comment is Rust, as rustdoc reads it.
/// 4. A tag of a language whose `[features.code-blocks.<language>]` table is absent:
///    [`NotChecked::LanguageOff`].
/// 5. A tag of a configured language: [`NotChecked::Pending`] until its tool runs.
/// 6. Any other tag: [`UnknownLanguage`](Self::UnknownLanguage).
///
/// `check` then looks up each pending language's tool: a tool not on `PATH` makes its blocks
/// [`ToolMissing`](Self::ToolMissing), and one whose real path is inside the checkout makes them
/// [`NotChecked::ToolInsideRoot`]. A Rust block is refused first when the root's toolchain file
/// can't be used ([`NotChecked::ToolchainPath`]), rustup can't name the toolchain a root with
/// no such file gets ([`NotChecked::ToolchainUnresolved`]), the block reads a file or variable
/// at compile time ([`NotChecked::CompileTimeRead`]), declares an outline module before its
/// linter compiles it ([`NotChecked::OutlineModule`]), or it is too large to scan for one
/// ([`NotChecked::TooLargeToScan`]). A TOML block is refused when it names a schema file
/// tombi would read ([`NotChecked::SchemaFile`]). A tool found outside the checkout runs on each
/// block, which leaves it [`Passed`](Self::Passed) or [`Failed`](Self::Failed).
///
/// A block's [`Steps`] record what its format and lint steps did. Once a step runs, the state is
/// the outcome over the steps that ran: [`Failed`](Self::Failed) when any failed, otherwise
/// [`Passed`](Self::Passed). A step that is off or skipped leaves the state alone, so a block
/// none of whose steps ran is [`NotChecked::NoStepRan`], and one whose text couldn't be read for
/// its tools is [`NotChecked::InputUnreadable`]. [`NotChecked::Pending`] is internal: `check`
/// settles every pending block before output.
///
/// Only what the author wrote and the config decide a state: supdoc never guesses a block into a
/// skip state. `uses(...)` findings leave the state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlockState {
    /// Every check run on the block passed.
    Passed,
    /// A check failed, or an excerpt's source is missing, for the reason held.
    Failed(Failure),
    /// An excerpt differs from its source.
    Stale,
    /// The author wrote `ignore(...)`.
    OptedOut {
        /// The reason in the parentheses, if any.
        reason: Option<String>,
    },
    /// Nothing checked the block, for the reason held.
    NotChecked(NotChecked),
    /// The block's language has no checker in supdoc.
    UnknownLanguage,
    /// The tool the block's language needs isn't on `PATH`.
    ToolMissing,
}

/// Why a block was not checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotChecked {
    /// The block is untagged prose: no tag, `text`, `console` or `output`.
    Text,
    /// The block's language has no table under `[features.code-blocks]`.
    LanguageOff,
    /// The block's language is on and its tools haven't been looked up yet. Internal only: every
    /// pending block is settled before output, and [`BlockState::reason`] asserts it was.
    Pending,
    /// The block's language is on, but none of its steps ran: each is off, skipped, refused or
    /// couldn't run.
    NoStepRan,
    /// The block's text couldn't be read from its doc for its tools.
    InputUnreadable,
    /// The tool's binary on `PATH` is inside the checkout, so the checkout chose it.
    ToolInsideRoot,
    /// The root's `rust-toolchain(.toml)` names a path, an invalid channel, or lies outside the
    /// root, so no Rust tool runs.
    ToolchainPath,
    /// The root pins no toolchain, and the `rustup` on `PATH` is inside the checkout, or couldn't
    /// name the toolchain the root gets as a channel, so no Rust tool runs.
    ToolchainUnresolved,
    /// The project's tool config extends another config, declares plugins or commands, or can't
    /// be handed to the tool, so its blocks aren't given to it.
    ToolConfig,
    /// The Rust block reads a file or an environment variable at compile time.
    CompileTimeRead,
    /// The Rust block is too large for the compile-time-read scan.
    TooLargeToScan,
    /// The Rust block declares an outline module (`mod name;`), which rustc would read from a
    /// file outside the block when its linter compiled it.
    OutlineModule,
    /// The TOML block holds a `#:schema` directive, which would make tombi read the file it
    /// names, wherever it is.
    SchemaFile,
}

/// Why a block failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// One of its `from(...)` sources is missing.
    SourceMissing,
    /// Its formatter's output differs from it.
    Unformatted,
    /// Its tool exited unexpectedly or wrote past the output cap.
    ToolFailed,
    /// Its tool reported a syntax error in it.
    ParseFailed,
    /// Its tool passed the `[limits] code-block-timeout` limit.
    ToolTimeout,
    /// Its linter reported diagnostics on it.
    Lint,
}

/// What a block's `from(...)` values came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Excerpt {
    /// Some value doesn't resolve.
    SourceMissing,
    /// The body differs from the parts.
    Stale,
    /// The body is the parts, or the doc couldn't be read to compare it.
    Current,
}

/// What one step, format or lint, did with a block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum StepState {
    /// The step ran on the block.
    Ran,
    /// Its tool's table sets the step's mode to `"off"`.
    Off {
        /// That table, `[features.code-blocks.<language>.<tool>]`.
        table: String,
        /// The step's key in it, `format` or `lint`.
        key: &'static str,
    },
    /// The author marked the block `format(off: ...)` or `lint(off: ...)`.
    Skipped {
        /// The reason in the parentheses, if any.
        reason: Option<String>,
    },
    /// The step didn't run: it isn't due on the block, or no tool is selected for it.
    #[default]
    NotRun,
    /// The step was due, but its tool was refused before it ran: missing from `PATH`, or found
    /// inside the checkout. Named `not_run` in JSON.
    ToolRefused {
        /// The tool's binary, as its warning names it.
        binary: String,
        /// Whether the binary on `PATH` is inside the checkout, rather than missing.
        inside_root: bool,
    },
    /// The step was due but supdoc couldn't run it safely, for the reason held. Named `not_run`
    /// in JSON, like [`NotRun`](Self::NotRun).
    Unrunnable {
        /// Why, as a block's reason: "no private working directory for clippy".
        reason: &'static str,
    },
    /// The step was due on a `fragment(<kind>)` block, but its tool doesn't check that kind.
    /// Named `not_run` in JSON.
    Unsupported {
        /// The tool's name.
        tool: String,
        /// The kind the block declares.
        kind: Kind,
    },
}

impl StepState {
    /// The step's name in JSON.
    #[must_use]
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Ran => "ran",
            Self::Off { .. } => "off",
            Self::Skipped { .. } => "skipped",
            Self::NotRun
            | Self::ToolRefused { .. }
            | Self::Unrunnable { .. }
            | Self::Unsupported { .. } => "not_run",
        }
    }

    /// Why the step didn't run on a block it is due on, as the block's reason gives it. `None`
    /// when it ran or wasn't due.
    fn why(&self) -> Option<String> {
        match self {
            Self::Ran | Self::NotRun => None,
            Self::Off { table, key } => Some(format!("`{key} = \"off\"` in `{table}`")),
            Self::Skipped { reason } => reason.clone(),
            Self::ToolRefused {
                binary,
                inside_root: false,
            } => Some(format!("`{binary}` isn't on PATH")),
            Self::ToolRefused {
                binary,
                inside_root: true,
            } => Some(format!("`{binary}` is inside the checkout")),
            Self::Unrunnable { reason } => Some((*reason).to_owned()),
            Self::Unsupported { tool, kind } => Some(unsupported_reason(tool, *kind)),
        }
    }
}

/// Why a step didn't run on a fragment its tool can't check: "rustfmt can't check a statements
/// fragment".
#[must_use]
pub(crate) fn unsupported_reason(tool: &str, kind: Kind) -> String {
    format!("{tool} can't check {} fragment", kind.with_article())
}

/// The kind `block` declares with its first `fragment(<kind>)` attribute, if it writes one.
#[must_use]
pub(crate) fn block_fragment(block: &CodeBlock) -> Option<Kind> {
    block
        .info
        .as_ref()?
        .parsed()
        .attributes
        .into_iter()
        .find_map(fragment_kind)
}

/// What a block's format and lint steps did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Steps {
    /// The format step.
    pub(crate) format: StepState,
    /// The lint step.
    pub(crate) lint: StepState,
}

impl Steps {
    /// Each step held because the operator hasn't granted the checkout trust, format first: its
    /// reason is a project-tier tool's [`needs_trust`](tools::Held::needs_trust), or its
    /// [`trust_off`](tools::Held::trust_off) when `SUPDOC_TRUST=0` turned trust off.
    pub(crate) fn held_for_trust(&self) -> impl Iterator<Item = Hold> {
        [&self.format, &self.lint]
            .into_iter()
            .filter_map(|step| match step {
                StepState::Unrunnable { reason } => tools::hold(reason),
                _ => None,
            })
    }

    /// The steps due on `block`, in `state`, under `code_blocks`. Only a
    /// [`NotChecked::Pending`] block has steps. A block attribute the selected tool's cartridge
    /// accepts beats its table, for that block only: each step is [`StepState::Skipped`] when
    /// the block is marked `format(off: ...)` or `lint(off: ...)`, due when it is marked
    /// `format(full)` or `lint(full)`, else [`StepState::Off`] when the tool's table sets the
    /// step's mode to `"off"`, and otherwise due. A due step is [`StepState::NotRun`] until it
    /// runs; a step with no selected tool is [`StepState::NotRun`] too.
    /// Every other block's steps are [`StepState::NotRun`].
    #[must_use]
    pub(crate) fn of(block: &CodeBlock, state: &BlockState, code_blocks: &CodeBlocks) -> Self {
        if *state != BlockState::NotChecked(NotChecked::Pending) {
            return Self::default();
        }
        let tag = tag_of(block).unwrap_or_default();
        let language = Language::from_tag(tag);
        let fragment = block_fragment(block);
        let step = |step: Step| {
            let Some((language, tool)) = language.and_then(|language| {
                tools::selected(code_blocks, language, step).map(|tool| (language, tool))
            }) else {
                return StepState::NotRun;
            };
            let table_off = code_blocks.mode(language, tool.name()) == Mode::Off;
            match block_setting(block, step, &tool) {
                Some(StepValue::Off { reason }) => StepState::Skipped {
                    reason: reason.map(str::to_owned),
                },
                Some(StepValue::Other(_)) | None if table_off => StepState::Off {
                    table: format!(
                        "[features.code-blocks.{}.{}]",
                        language.table(),
                        tool.name()
                    ),
                    key: step.key(),
                },
                Some(StepValue::Full | StepValue::Other(_)) | None => match fragment {
                    Some(kind) if !tool.supports(kind, tag) => StepState::Unsupported {
                        tool: tool.name().to_owned(),
                        kind,
                    },
                    _ => StepState::NotRun,
                },
            }
        };
        Self {
            format: step(Step::Format),
            lint: step(Step::Lint),
        }
    }

    /// Why a block's steps didn't all run, for a block whose state gives no better reason: each
    /// step's reason when it is off, skipped, refused or couldn't run, format then lint, joined by
    /// `; `, so one step's reason never hides the other's. Both steps sharing a reason give it
    /// once. `None` when every due step ran.
    fn reason(&self) -> Option<String> {
        let mut reasons: Vec<String> = [self.format.why(), self.lint.why()]
            .into_iter()
            .flatten()
            .collect();
        reasons.dedup();
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }
}

/// The state of `block`, by the order on [`BlockState`]. `excerpt` is what its `from(...)`
/// values came to, `None` when it has none; `on` says whether a language's table is written.
#[must_use]
pub(crate) fn state_of(
    block: &CodeBlock,
    excerpt: Option<Excerpt>,
    on: impl Fn(Language) -> bool,
) -> BlockState {
    match excerpt {
        Some(Excerpt::SourceMissing) => return BlockState::Failed(Failure::SourceMissing),
        Some(Excerpt::Stale) => return BlockState::Stale,
        Some(Excerpt::Current) => return BlockState::Passed,
        None => {}
    }
    let parsed = block.info.as_ref().map(|info| info.parsed());
    let ignored = parsed
        .iter()
        .flat_map(|parsed| &parsed.attributes)
        .find_map(|attribute| reasoned(attribute).filter(|found| found.name == "ignore"));
    if let Some(found) = ignored {
        return BlockState::OptedOut {
            reason: found.reason.map(str::to_owned),
        };
    }
    match tag_of(block) {
        None | Some("text" | "console" | "output") => BlockState::NotChecked(NotChecked::Text),
        Some(tag) => match Language::from_tag(tag) {
            Some(language) if on(language) => BlockState::NotChecked(NotChecked::Pending),
            Some(_) => BlockState::NotChecked(NotChecked::LanguageOff),
            None => BlockState::UnknownLanguage,
        },
    }
}

/// The language tag `block` is read as: the first word of its info string, or Rust for an untagged
/// fence in a Rust doc comment, as rustdoc reads it. `None` for an untagged block elsewhere.
#[must_use]
pub(crate) fn tag_of(block: &CodeBlock) -> Option<&str> {
    block
        .info
        .as_ref()
        .map(|info| info.parsed().language)
        .filter(|tag| !tag.is_empty())
        .or_else(|| block.default_language())
}

/// The value `block` sets `step` to with a `lint(...)` or `format(...)` attribute, the first one
/// written, when `tool` accepts it. `None` when the block has no such attribute or `tool`
/// refuses its value, which then warns `code-block-unknown-attribute`.
#[must_use]
pub(crate) fn block_setting<'block>(
    block: &'block CodeBlock,
    step: Step,
    tool: &Tool,
) -> Option<StepValue<'block>> {
    block
        .info
        .as_ref()?
        .parsed()
        .attributes
        .into_iter()
        .filter_map(step_attribute)
        .find(|found| found.step == step)
        .map(|found| found.value)
        .filter(|value| {
            value
                .mode()
                .is_some_and(|mode| tool.modes().contains(&mode))
        })
}

impl BlockState {
    /// The state's name in JSON.
    #[must_use]
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed(_) => "failed",
            Self::Stale => "stale",
            Self::OptedOut { .. } => "opted_out",
            Self::NotChecked(_) => "not_checked",
            Self::UnknownLanguage => "unknown_language",
            Self::ToolMissing => "tool_missing",
        }
    }

    /// Why the block is in this state, when it has a reason to give. A passed block, or one none
    /// of whose steps ran, whose `steps` didn't all run gives theirs (see [`Steps`]). `tag` is the block's language
    /// tag and `tool` the name of the tool whose step decided it (the formatter, else the
    /// linter), when one was; a tool refused before it ran, missing or inside the checkout, is
    /// named by its binary, as its warning names it.
    #[must_use]
    pub(crate) fn reason(
        &self,
        steps: &Steps,
        tag: Option<&str>,
        tool: Option<&str>,
    ) -> Option<String> {
        debug_assert_ne!(
            *self,
            Self::NotChecked(NotChecked::Pending),
            "a pending block must be settled before its reason is given"
        );
        if matches!(self, Self::Passed | Self::NotChecked(NotChecked::NoStepRan))
            && let Some(reason) = steps.reason()
        {
            return Some(reason);
        }
        match self {
            Self::Passed | Self::NotChecked(NotChecked::Pending) => None,
            Self::Failed(Failure::SourceMissing) => {
                Some("an excerpt's source is missing".to_owned())
            }
            Self::Failed(Failure::Unformatted) => Some(format!(
                "`{}` would format it differently",
                tool_of(tag, tool)
            )),
            Self::Failed(Failure::ToolFailed) => Some(format!("`{}` failed", tool_of(tag, tool))),
            Self::Failed(Failure::ParseFailed) => {
                Some(format!("`{}` couldn't parse it", tool_of(tag, tool)))
            }
            Self::Failed(Failure::ToolTimeout) => {
                Some(format!("`{}` passed the time limit", tool_of(tag, tool)))
            }
            Self::Failed(Failure::Lint) => {
                Some(format!("`{}` reported lint findings", tool_of(tag, tool)))
            }
            Self::Stale => Some("the block differs from its source".to_owned()),
            Self::OptedOut { reason } => reason.clone(),
            Self::NotChecked(NotChecked::Text) => Some("the block is text, not code".to_owned()),
            Self::NotChecked(NotChecked::LanguageOff) => {
                let table = match tag.and_then(Language::from_tag) {
                    Some(language) => language.table(),
                    None => "language",
                };
                Some(format!("no `[features.code-blocks.{table}]` table"))
            }
            Self::NotChecked(NotChecked::NoStepRan) => Some("none of its steps ran".to_owned()),
            Self::NotChecked(NotChecked::InputUnreadable) => {
                Some("supdoc couldn't read the block's text from its doc".to_owned())
            }
            Self::NotChecked(NotChecked::ToolInsideRoot) => {
                Some(format!("`{}` is inside the checkout", tool_of(tag, tool)))
            }
            Self::NotChecked(NotChecked::ToolchainPath) => Some("toolchain path".to_owned()),
            Self::NotChecked(NotChecked::ToolchainUnresolved) => {
                Some("toolchain unresolved".to_owned())
            }
            Self::NotChecked(NotChecked::ToolConfig) => Some("tool config".to_owned()),
            Self::NotChecked(NotChecked::CompileTimeRead) => Some("compile-time read".to_owned()),
            Self::NotChecked(NotChecked::TooLargeToScan) => Some("too large to scan".to_owned()),
            Self::NotChecked(NotChecked::OutlineModule) => Some("outline module".to_owned()),
            Self::NotChecked(NotChecked::SchemaFile) => {
                Some("the block names a schema file".to_owned())
            }
            Self::UnknownLanguage => {
                Some(format!("supdoc has no checker for `{}`", tag.unwrap_or("")))
            }
            Self::ToolMissing => Some(format!("`{}` isn't on PATH", tool_of(tag, tool))),
        }
    }
}

/// The name of the formatter chosen for the block, else the default for blocks tagged `tag`, or
/// `its tool`.
fn tool_of<'a>(tag: Option<&str>, chosen: Option<&'a str>) -> &'a str {
    chosen.unwrap_or_else(|| {
        tag.and_then(Language::from_tag)
            .and_then(formatter)
            .map_or("its tool", |tool| tool.name)
    })
}

/// How many blocks are in each state, and how many steps were off, skipped or due but not run.
/// `blocks` is the sum of the state counts; the step counts are apart from them, so a block whose
/// format step is off and whose lint step passed counts in both `passed` and `format_off`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub(crate) struct CodeBlockCounts {
    /// Every block counted.
    pub(crate) blocks: usize,
    /// Blocks whose checks all passed.
    pub(crate) passed: usize,
    /// Blocks with a failed check or a missing excerpt source.
    pub(crate) failed: usize,
    /// Excerpts that differ from their source.
    pub(crate) stale: usize,
    /// Blocks the author marked `ignore(...)`.
    pub(crate) opted_out: usize,
    /// Blocks nothing checked: text, a language turned off, or no step that ran.
    pub(crate) not_checked: usize,
    /// Format steps off: blocks whose formatter's table sets `format = "off"`.
    pub(crate) format_off: usize,
    /// Format steps skipped: blocks the author marked `format(off: ...)`.
    pub(crate) format_skipped: usize,
    /// Format steps due on a checked block that didn't run, such as a formatter supdoc couldn't
    /// run safely.
    pub(crate) format_not_run: usize,
    /// Lint steps off: blocks whose linter's table sets `lint = "off"`.
    pub(crate) lint_off: usize,
    /// Lint steps skipped: blocks the author marked `lint(off: ...)`.
    pub(crate) lint_skipped: usize,
    /// Lint steps due on a checked block that didn't run, such as clippy, which needs trust.
    pub(crate) lint_not_run: usize,
    /// How many format steps didn't run for each short reason, for the summary line. They sum to
    /// `format_not_run`.
    #[serde(skip)]
    pub(crate) format_not_run_reasons: BTreeMap<String, usize>,
    /// How many lint steps didn't run for each short reason, for the summary line. They sum to
    /// `lint_not_run`.
    #[serde(skip)]
    pub(crate) lint_not_run_reasons: BTreeMap<String, usize>,
    /// Blocks in a language supdoc has no checker for.
    pub(crate) unknown_language: usize,
    /// Blocks whose selected tool isn't installed.
    pub(crate) tool_missing: usize,
}

/// How bad a count is, for its color in the summary line.
#[derive(Clone, Copy)]
enum Tone {
    /// A failure.
    Bad,
    /// Something left unchecked that the author didn't choose.
    Unchecked,
    /// The author's own choice.
    Plain,
    /// All checks passed.
    Good,
}

impl CodeBlockCounts {
    /// Counts each block's state and its steps that are off or skipped.
    #[must_use]
    pub(crate) fn tally<'block>(
        blocks: impl IntoIterator<Item = (&'block BlockState, &'block Steps)>,
    ) -> Self {
        let mut counts = Self::default();
        for (state, steps) in blocks {
            counts.blocks += 1;
            for (step, off, skipped, not_run, why) in [
                (
                    &steps.format,
                    &mut counts.format_off,
                    &mut counts.format_skipped,
                    &mut counts.format_not_run,
                    &mut counts.format_not_run_reasons,
                ),
                (
                    &steps.lint,
                    &mut counts.lint_off,
                    &mut counts.lint_skipped,
                    &mut counts.lint_not_run,
                    &mut counts.lint_not_run_reasons,
                ),
            ] {
                match step {
                    StepState::Off { .. } => *off += 1,
                    StepState::Skipped { .. } => *skipped += 1,
                    StepState::Unrunnable { reason } => {
                        *not_run += 1;
                        *why.entry(short_reason(reason).to_owned()).or_default() += 1;
                    }
                    StepState::ToolRefused { inside_root, .. } => {
                        *not_run += 1;
                        let reason = if *inside_root {
                            "tool inside the checkout"
                        } else {
                            "tool missing"
                        };
                        *why.entry(reason.to_owned()).or_default() += 1;
                    }
                    StepState::Unsupported { tool, kind } => {
                        *not_run += 1;
                        *why.entry(unsupported_reason(tool, *kind)).or_default() += 1;
                    }
                    StepState::Ran | StepState::NotRun => {}
                }
            }
            *match state {
                BlockState::Passed => &mut counts.passed,
                BlockState::Failed(_) => &mut counts.failed,
                BlockState::Stale => &mut counts.stale,
                BlockState::OptedOut { .. } => &mut counts.opted_out,
                BlockState::NotChecked(_) => &mut counts.not_checked,
                BlockState::UnknownLanguage => &mut counts.unknown_language,
                BlockState::ToolMissing => &mut counts.tool_missing,
            } += 1;
        }
        counts
    }

    /// `Code blocks  1 failed · 2 not checked · 3 passed`: worst first, zero counts left out.
    ///
    /// Steps that didn't run get one count per reason, `57 lint not run (clippy needs trust) ·
    /// 5 lint not run (tool missing)`, most first and then by reason, so the line is the same on
    /// every run.
    ///
    /// No blocks at all says `0 passed`, so the line is never empty. Without `unicode` the counts
    /// are separated by `, ` instead of `·`.
    #[must_use]
    pub(crate) fn line(&self, color: bool, unicode: bool) -> String {
        let worst_first = [
            (self.failed, "failed", Tone::Bad),
            (self.stale, "stale", Tone::Bad),
            (self.tool_missing, "tool missing", Tone::Unchecked),
            (self.unknown_language, "unknown language", Tone::Unchecked),
            (self.not_checked, "not checked", Tone::Unchecked),
            (self.format_off, "format off", Tone::Unchecked),
            (self.lint_off, "lint off", Tone::Unchecked),
            (self.format_not_run, "format not run", Tone::Unchecked),
            (self.lint_not_run, "lint not run", Tone::Unchecked),
            (self.format_skipped, "format skipped", Tone::Plain),
            (self.lint_skipped, "lint skipped", Tone::Plain),
            (self.opted_out, "opted out", Tone::Plain),
            (self.passed, "passed", Tone::Good),
        ];
        let mut parts: Vec<String> = worst_first
            .into_iter()
            .filter(|(count, _, _)| *count > 0)
            .flat_map(|(count, label, tone)| {
                let reasons = match label {
                    "format not run" => by_count(&self.format_not_run_reasons),
                    "lint not run" => by_count(&self.lint_not_run_reasons),
                    _ => Vec::new(),
                };
                if reasons.is_empty() {
                    return vec![paint(color, tone, &format!("{count} {label}"))];
                }
                reasons
                    .into_iter()
                    .map(|(why, count)| paint(color, tone, &format!("{count} {label} ({why})")))
                    .collect()
            })
            .collect();
        if parts.is_empty() {
            parts.push(paint(color, Tone::Good, "0 passed"));
        }
        let separator = if unicode { " · " } else { ", " };
        format!("Code blocks  {}", parts.join(separator))
    }
}

/// `reasons` with their counts, most first, then by reason.
fn by_count(reasons: &BTreeMap<String, usize>) -> Vec<(&str, usize)> {
    let mut sorted: Vec<_> = reasons
        .iter()
        .map(|(why, count)| (why.as_str(), *count))
        .collect();
    sorted.sort_by(|(a_why, a_count), (b_why, b_count)| {
        b_count.cmp(a_count).then_with(|| a_why.cmp(b_why))
    });
    sorted
}

/// `reason`, a step's full reason for not running, as the summary line names it: the held tool's
/// [`short`](tools::Held::short) reason, "clippy needs trust", for a step held for want of trust,
/// "`SUPDOC_TRUST=0` turned trust off" when the variable held it, else the reason as written.
fn short_reason(reason: &'static str) -> &'static str {
    match tools::hold(reason) {
        Some(Hold {
            turned_off: true, ..
        }) => "`SUPDOC_TRUST=0` turned trust off",
        Some(Hold { held, .. }) => held.short,
        None => reason,
    }
}

/// Wraps `text` in its tone's color, when color is on.
fn paint(color: bool, tone: Tone, text: &str) -> String {
    let style = match tone {
        Tone::Bad => Style::new().fg_color(Some(AnsiColor::Red.into())),
        Tone::Unchecked => Style::new().fg_color(Some(AnsiColor::Yellow.into())),
        Tone::Good => Style::new().fg_color(Some(AnsiColor::Green.into())),
        Tone::Plain => return text.to_owned(),
    };
    if !color {
        return text.to_owned();
    }
    format!("{style}{text}{style:#}")
}

#[cfg(test)]
mod tests {
    use super::{
        BlockState, CodeBlockCounts, Excerpt, Failure, NotChecked, StepState, Steps, state_of,
    };
    use crate::code_blocks::block::{CodeBlock, Info};
    use crate::code_blocks::tools::{CARTRIDGES, Held, Hold, ToolCartridge};
    use crate::config::Config;
    use crate::config::code_blocks::{CodeBlocks, Language};
    use crate::finding::Location;
    use crate::path::AbsolutePath;

    /// clippy's trust words, the reasons its lint step gives when trust holds it back.
    fn clippy_held() -> &'static Held {
        CARTRIDGES
            .iter()
            .find(|tool| tool.name == "clippy")
            .and_then(ToolCartridge::held)
            .expect("clippy is a project-tier cartridge")
    }

    #[test]
    fn each_step_held_for_trust_leads_back_to_its_tool_format_first() {
        let held = clippy_held();
        let steps = Steps {
            format: StepState::Unrunnable {
                reason: held.trust_off,
            },
            lint: StepState::Unrunnable {
                reason: held.needs_trust,
            },
        };
        let other = Steps {
            format: StepState::Unrunnable {
                reason: "no private working directory for clippy",
            },
            lint: StepState::Ran,
        };

        assert_eq!(
            (
                steps.held_for_trust().collect::<Vec<_>>(),
                other.held_for_trust().count()
            ),
            (
                vec![
                    Hold {
                        held,
                        turned_off: true
                    },
                    Hold {
                        held,
                        turned_off: false
                    }
                ],
                0
            ),
            "a trust reason should name its tool and whether `SUPDOC_TRUST=0` held it; any other \
             reason isn't a hold"
        );
    }

    /// A block in `file` with `info` as its info string, or none.
    fn block(file: &str, info: Option<&str>) -> CodeBlock {
        let location = Location {
            file: file
                .parse()
                .expect("the fixture path should be a source path"),
            range: 0..1,
            line: 1,
            column: 1,
        };
        CodeBlock {
            fence: location.clone(),
            info: info.map(|text| Info {
                text: text.to_owned(),
                location,
            }),
            body: Vec::new(),
        }
    }

    /// The state of `block` with no excerpt and only Rust's table written.
    fn state(block: &CodeBlock) -> BlockState {
        state_of(block, None, |language| language == Language::Rust)
    }

    #[test]
    fn an_excerpt_takes_its_outcome_before_ignore() {
        let block = block("docs/a.md", Some("rust from(src/a.rs) ignore(why)"));
        let cases = [
            (
                Excerpt::SourceMissing,
                BlockState::Failed(Failure::SourceMissing),
            ),
            (Excerpt::Stale, BlockState::Stale),
            (Excerpt::Current, BlockState::Passed),
        ];
        for (excerpt, expected) in cases {
            assert_eq!(state_of(&block, Some(excerpt), |_| true), expected);
        }
    }

    #[test]
    fn ignore_opts_out_with_its_reason_when_it_has_one() {
        assert_eq!(
            state(&block("docs/a.md", Some("rust ignore(needs a server)"))),
            BlockState::OptedOut {
                reason: Some("needs a server".to_owned())
            }
        );
        assert_eq!(
            state(&block("docs/a.md", Some("zsh ignore"))),
            BlockState::OptedOut { reason: None }
        );
    }

    #[test]
    fn untagged_and_text_blocks_are_not_checked_as_text() {
        for info in [
            None,
            Some(""),
            Some("text"),
            Some("console"),
            Some("output"),
        ] {
            assert_eq!(
                state(&block("docs/a.md", info)),
                BlockState::NotChecked(NotChecked::Text),
                "info {info:?}"
            );
        }
    }

    #[test]
    fn an_untagged_block_in_a_rust_doc_comment_is_rust() {
        assert_eq!(
            state(&block("src/lib.rs", None)),
            BlockState::NotChecked(NotChecked::Pending)
        );
    }

    #[test]
    fn a_rust_doc_fence_leading_with_a_known_attribute_is_rust_with_that_attribute() {
        assert_eq!(
            state(&block("src/lib.rs", Some("no_run"))),
            BlockState::NotChecked(NotChecked::Pending)
        );
        assert_eq!(
            state(&block("src/lib.rs", Some("ignore(slow)"))),
            BlockState::OptedOut {
                reason: Some("slow".to_owned())
            }
        );
    }

    #[test]
    fn markdown_keeps_the_first_word_as_the_language_even_when_it_is_an_attribute() {
        assert_eq!(
            state(&block("docs/a.md", Some("no_run"))),
            BlockState::UnknownLanguage
        );
    }

    #[test]
    fn a_language_without_its_table_is_off_and_with_it_pending() {
        assert_eq!(
            state(&block("docs/a.md", Some("rs"))),
            BlockState::NotChecked(NotChecked::Pending)
        );
        for tag in ["bash", "python3"] {
            assert_eq!(
                state(&block("docs/a.md", Some(tag))),
                BlockState::NotChecked(NotChecked::LanguageOff),
                "tag `{tag}`"
            );
        }
    }

    #[test]
    fn any_other_tag_is_an_unknown_language() {
        for tag in ["zsh", "rust,ignore", "ini"] {
            assert_eq!(
                state(&block("docs/a.md", Some(tag))),
                BlockState::UnknownLanguage,
                "tag `{tag}`"
            );
        }
    }

    #[test]
    fn blocks_is_the_sum_and_the_line_puts_the_worst_first() {
        let states = [
            BlockState::Passed,
            BlockState::Passed,
            BlockState::Failed(Failure::Unformatted),
            BlockState::OptedOut { reason: None },
            BlockState::NotChecked(NotChecked::Text),
            BlockState::UnknownLanguage,
            BlockState::ToolMissing,
        ];

        let steps = Steps::default();
        let counts = CodeBlockCounts::tally(states.iter().map(|state| (state, &steps)));

        assert_eq!(counts.blocks, 7);
        assert_eq!(
            counts.line(false, true),
            "Code blocks  1 failed · 1 tool missing · 1 unknown language · 1 not checked · 1 opted out · 2 passed"
        );
        assert_eq!(
            CodeBlockCounts::default().line(false, false),
            "Code blocks  0 passed"
        );
    }

    /// The `[features.code-blocks]` settings `config`, a whole config file, resolves to.
    fn settings(config: &str) -> CodeBlocks {
        let dir = tempfile::TempDir::new().expect("a temp dir should be creatable");
        std::fs::write(dir.path().join("supdoc.config.toml"), config)
            .expect("the fixture config should be writable");
        let root = AbsolutePath::new(dir.path()).expect("a temp dir path should be absolute");
        Config::load(&root)
            .expect("the fixture config should load")
            .code_blocks()
            .cloned()
            .expect("the fixture writes the table")
    }

    /// The steps due on a Markdown block tagged `info`, under `config`.
    fn steps(config: &str, info: &str) -> Steps {
        let block = block("docs/a.md", Some(info));
        let code_blocks = settings(config);
        let state = state_of(&block, None, |language| code_blocks.checks(language));
        Steps::of(&block, &state, &code_blocks)
    }

    #[test]
    fn lint_off_skips_the_lint_step_with_its_reason_and_a_table_off_turns_it_off() {
        let config =
            "[features.code-blocks.shell]\n[features.code-blocks.python.ruff]\nlint = \"off\"\n";

        let skipped = steps(config, "sh lint(off: needs a server, (and a db))");
        let bare = steps(config, "sh lint(off)");
        let off = steps(config, "python");
        let due = steps(config, "sh");

        assert_eq!(
            skipped,
            Steps {
                format: StepState::NotRun,
                lint: StepState::Skipped {
                    reason: Some("needs a server, (and a db)".to_owned())
                },
            }
        );
        assert_eq!(bare.lint, StepState::Skipped { reason: None });
        assert_eq!(off.lint, step_off("python.ruff", "lint"));
        assert_eq!(off.format, StepState::NotRun, "only the lint step is off");
        assert_eq!(due, Steps::default(), "a lint step left on is due");
    }

    #[test]
    fn a_block_attribute_beats_its_tools_table_for_that_block_only() {
        let config = "[features.code-blocks.shell.shfmt]\nformat = \"off\"\n\
                      [features.code-blocks.python.ruff]\nlint = \"off\"\n";

        let format_full = steps(config, "sh format(full)");
        let lint_full = steps(config, "python lint(full)");
        let format_off = steps(config, "python format(off: generated)");
        let neighbour = steps(config, "sh");

        assert_eq!(
            format_full.format,
            StepState::NotRun,
            "format(full) makes the step due"
        );
        assert_eq!(
            lint_full.lint,
            StepState::NotRun,
            "lint(full) makes the step due"
        );
        assert_eq!(
            format_off.format,
            StepState::Skipped {
                reason: Some("generated".to_owned())
            }
        );
        assert_eq!(
            neighbour.format,
            step_off("shell.shfmt", "format"),
            "another block keeps the table's mode"
        );
    }

    #[test]
    fn a_value_the_cartridge_refuses_leaves_the_tables_mode() {
        let config = "[features.code-blocks.python.ruff]\nlint = \"off\"\n";

        for info in ["python lint(partial)", "python lint(on)", "python lint()"] {
            assert_eq!(
                steps(config, info).lint,
                step_off("python.ruff", "lint"),
                "info `{info}`"
            );
        }
    }

    #[test]
    fn only_a_pending_block_has_steps_off_or_skipped() {
        let config = "[features.code-blocks.shell.shfmt]\nformat = \"off\"\n\
                      [features.code-blocks.shell.shellcheck]\nlint = \"off\"\n";

        for info in [
            "sh ignore(slow) lint(off: why)",
            "text format(off: why)",
            "zsh",
        ] {
            assert_eq!(steps(config, info), Steps::default(), "info `{info}`");
        }
    }

    #[test]
    fn steps_are_counted_apart_from_the_state_they_leave() {
        let format_off = Steps {
            format: step_off("shell.shfmt", "format"),
            lint: StepState::Ran,
        };
        let lint_not_run = Steps {
            format: StepState::Ran,
            lint: StepState::Unrunnable {
                reason: clippy_held().needs_trust,
            },
        };
        let lint_skipped = Steps {
            format: StepState::Ran,
            lint: StepState::Skipped { reason: None },
        };
        let both_off = Steps {
            format: step_off("shell.shfmt", "format"),
            lint: step_off("shell.shellcheck", "lint"),
        };
        let blocks = [
            (BlockState::Passed, &format_off),
            (BlockState::Passed, &lint_not_run),
            (BlockState::Failed(Failure::Unformatted), &lint_skipped),
            (BlockState::NotChecked(NotChecked::NoStepRan), &both_off),
        ];

        let counts = CodeBlockCounts::tally(blocks.iter().map(|(state, steps)| (state, *steps)));

        assert_eq!(
            (
                counts.blocks,
                counts.passed,
                counts.failed,
                counts.not_checked
            ),
            (4, 2, 1, 1)
        );
        assert_eq!((counts.format_not_run, counts.lint_not_run), (0, 1));
        assert_eq!(
            (
                counts.format_off,
                counts.format_skipped,
                counts.lint_off,
                counts.lint_skipped
            ),
            (2, 0, 1, 1)
        );
        assert_eq!(
            counts.line(false, true),
            "Code blocks  1 failed · 1 not checked · 2 format off · 1 lint off · \
             1 lint not run (clippy needs trust) · 1 lint skipped · 2 passed"
        );
    }

    #[test]
    fn format_not_run_gets_one_count_per_reason_most_first_then_by_reason() {
        let missing = Steps {
            format: StepState::ToolRefused {
                binary: "shfmt".to_owned(),
                inside_root: false,
            },
            lint: StepState::Ran,
        };
        let inside = Steps {
            format: StepState::ToolRefused {
                binary: "shfmt".to_owned(),
                inside_root: true,
            },
            lint: StepState::Ran,
        };
        let unrunnable = Steps {
            format: StepState::Unrunnable {
                reason: "no private directory",
            },
            lint: StepState::Ran,
        };
        let blocks = [&missing, &inside, &unrunnable, &missing, &unrunnable];

        let counts =
            CodeBlockCounts::tally(blocks.iter().map(|steps| (&BlockState::Passed, *steps)));

        assert_eq!(counts.format_not_run, 5, "the total stays one number");
        assert_eq!(
            counts.line(false, false),
            "Code blocks  2 format not run (no private directory), \
             2 format not run (tool missing), 1 format not run (tool inside the checkout), \
             5 passed"
        );
    }

    #[test]
    fn a_passed_block_whose_linter_is_missing_names_its_binary() {
        let steps = Steps {
            format: StepState::Ran,
            lint: StepState::ToolRefused {
                binary: "shellcheck".to_owned(),
                inside_root: false,
            },
        };

        let reason = BlockState::Passed.reason(&steps, Some("sh"), Some("shfmt"));

        assert_eq!(reason.as_deref(), Some("`shellcheck` isn't on PATH"));
        assert_eq!(steps.lint.name(), "not_run");
    }

    #[test]
    fn a_passed_block_whose_linter_is_inside_the_checkout_says_so() {
        let steps = Steps {
            format: StepState::Ran,
            lint: StepState::ToolRefused {
                binary: "shellcheck".to_owned(),
                inside_root: true,
            },
        };

        let reason = BlockState::Passed.reason(&steps, Some("sh"), Some("shfmt"));

        assert_eq!(
            reason.as_deref(),
            Some("`shellcheck` is inside the checkout")
        );
    }

    #[test]
    fn a_step_that_was_not_due_gives_no_reason() {
        let steps = Steps {
            format: StepState::Ran,
            lint: StepState::NotRun,
        };

        let reason = BlockState::Passed.reason(&steps, Some("sh"), Some("shfmt"));

        assert_eq!(
            reason, None,
            "a passed block with no lint step is just passed"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a pending block must be settled before its reason is given")]
    fn a_pending_block_never_reaches_output() {
        let _reason =
            BlockState::NotChecked(NotChecked::Pending).reason(&Steps::default(), Some("sh"), None);
    }

    #[test]
    fn every_settled_state_gives_an_output_reason_and_none_is_transitional() {
        let steps = Steps::default();
        let settled = [
            BlockState::Failed(Failure::SourceMissing),
            BlockState::Stale,
            BlockState::NotChecked(NotChecked::Text),
            BlockState::NotChecked(NotChecked::LanguageOff),
            BlockState::NotChecked(NotChecked::NoStepRan),
            BlockState::NotChecked(NotChecked::InputUnreadable),
            BlockState::NotChecked(NotChecked::ToolInsideRoot),
            BlockState::NotChecked(NotChecked::ToolchainUnresolved),
            BlockState::NotChecked(NotChecked::SchemaFile),
            BlockState::UnknownLanguage,
            BlockState::ToolMissing,
        ];

        for state in settled {
            let reason = state.reason(&steps, Some("sh"), Some("shfmt"));

            assert!(
                reason
                    .as_deref()
                    .is_some_and(|reason| !reason.contains(" yet")),
                "{state:?} gave {reason:?}"
            );
        }
    }

    /// The state of a step its tool's table, `[features.code-blocks.<table>]`, turns off.
    fn step_off(table: &str, key: &'static str) -> StepState {
        StepState::Off {
            table: format!("[features.code-blocks.{table}]"),
            key,
        }
    }

    #[test]
    fn a_blocks_reason_keeps_each_steps_reason_when_neither_ran() {
        let steps = Steps {
            format: step_off("rust.rustfmt", "format"),
            lint: StepState::Unrunnable {
                reason: "clippy needs trust",
            },
        };

        let reason =
            BlockState::NotChecked(NotChecked::NoStepRan).reason(&steps, Some("rust"), None);

        assert_eq!(
            reason.as_deref(),
            Some("`format = \"off\"` in `[features.code-blocks.rust.rustfmt]`; clippy needs trust")
        );
    }

    #[test]
    fn a_reason_both_steps_share_is_given_once() {
        let unrun = StepState::Unrunnable {
            reason: "tombi couldn't load the project's config",
        };
        let steps = Steps {
            format: unrun.clone(),
            lint: unrun,
        };

        let reason =
            BlockState::NotChecked(NotChecked::NoStepRan).reason(&steps, Some("toml"), None);

        assert_eq!(
            reason.as_deref(),
            Some("tombi couldn't load the project's config")
        );
    }
}
