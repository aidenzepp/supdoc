//! The per-block pipeline that `check` runs: judges each code block and returns its findings
//! and the count of blocks in each state.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::config::code_blocks::{CodeBlocks, Dialect, Formatter, Language};
use crate::finding::{Finding, Location, Severity};
use crate::hash::Hash;
use crate::lock::model::Lockfile;
use crate::read::cartridge::Languages;
use crate::read::error::CartridgeLoadError;

use super::block::{CodeBlock, Info};
use super::excerpt;
use super::format::{self, FixDoc};
use super::info::{
    KNOWN_ATTRIBUTES, SourceKind, is_known_attribute, parenthesized_spelling, reasoned,
    source_attributes,
};
use super::runner::{self, Call};
use super::rust;
use super::shell;
use super::source::{self, SourceText, Unresolved};
use super::states::{Excerpt, NotChecked, state_of, tag_of};
use super::toolchain::{self, Refusal, Toolchain};
use super::tools::{Found, Step, TOOLS, Tool, ToolEnv, find, formatter};
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
    /// Why it is in that state, when there is a reason.
    pub(crate) reason: Option<String>,
    /// Each `uses(...)` target the block writes, with its state.
    pub(crate) uses: Vec<(String, UseState)>,
    /// The block's 1-based position among the blocks of its doc.
    index: usize,
    /// The formatter the config selects for it, while the block is pending.
    tool: Option<&'static Tool>,
    /// The shell dialect shfmt parses it as.
    dialect: Dialect,
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
    /// What a formatter reads for it.
    input: String,
}

impl Pending {
    /// `block`, found in `doc`, ready for the format step. `None` when its body isn't inside
    /// `doc`.
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
/// [`format::safe_fix`] refuses its fix. Then the `uses(...)` findings against `lock`'s `[code-blocks]`
/// section, judging a doc's recorded targets obsolete only when `in_scope` holds for it and none
/// of its `uses(...)` values fails to resolve. With them, each block's [`BlockState`], counted.
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
) -> Result<Evaluation, CartridgeLoadError> {
    let Some(code_blocks) = config.code_blocks() else {
        return Ok(Evaluation::default());
    };
    let mut findings = Vec::new();
    let mut states: Vec<BlockState> = Vec::with_capacity(blocks.len());
    let mut doc: Option<(&str, Option<Arc<FixDoc>>)> = None;
    let parse_timeout = config.limits_in_effect().parse_timeout.value;
    let read = |file: &str| {
        read_doc(config, file).map(|text| Arc::new(FixDoc::new(Arc::from(text), parse_timeout)))
    };
    let mut written = Vec::new();
    let mut unresolved_uses = BTreeSet::new();
    let recorded: BTreeMap<(&str, String), Hash> = lock
        .recorded_uses()
        .map(|(doc, target, hash)| ((doc, target), hash))
        .collect();
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
        let (state, tool, skip_reason) =
            format_choice(block, state, language.as_deref(), code_blocks);
        let pending = (state == BlockState::NotChecked(NotChecked::Pending))
            .then(|| {
                let file = block.fence.file.as_str();
                if doc.as_ref().is_none_or(|(read, _)| *read != file) {
                    doc = Some((file, read(file)));
                }
                doc.as_ref()?
                    .1
                    .as_ref()
                    .and_then(|text| Pending::of(block, text))
            })
            .flatten();
        records.push(BlockRecord {
            at: block.fence.clone(),
            reason: skip_reason.or_else(|| state.reason(language.as_deref(), None)),
            tool,
            dialect: code_blocks.dialect(),
            language,
            state: state.clone(),
            uses: block_uses,
            index,
            pending,
        });
        states.push(state);
    }
    findings.extend(uses::judge(&written, lock.recorded_uses(), |doc| {
        in_scope(doc) && !unresolved_uses.contains(doc)
    }));
    Ok(Evaluation {
        findings,
        counts: Some(CodeBlockCounts::tally(&states)),
        records,
        timeout: config.limits_in_effect().code_block_timeout.value.get(),
    })
}

impl Evaluation {
    /// Looks up the formatter of each pending block's language on `env`'s `PATH`, once per tool.
    /// A tool not found makes its blocks [`BlockState::ToolMissing`] with one
    /// `code-block-tool-missing` warning naming its install command; one whose real path is
    /// inside `root` makes them not checked with one `code-block-not-checked` warning. A tool
    /// found elsewhere runs, hardened, on each block of a language with a format step, which
    /// passes it or fails it with one `code-block-unformatted`, `code-block-tool-failed` or
    /// `code-block-tool-timeout` error; a language without a format step stays pending. An
    /// unformatted block's error carries the fix to the formatter's output, unless
    /// [`format::safe_fix`] finds it could end the block early, which adds a
    /// `code-block-fix-unsafe` warning instead.
    ///
    /// Effect: reads file metadata along `PATH`, resolves symlinks, and runs each found formatter
    /// once per block, killing it at the timeout.
    pub(crate) fn look_up_tools(&mut self, env: &ToolEnv, root: &Path) {
        let timeout = self.timeout;
        let mut looked_up: Vec<(&'static str, Found)> = Vec::new();
        let mut rust_setup: Option<RustSetup> = None;
        let mut toolchain_warned = false;
        for record in &mut self.records {
            if record.state != BlockState::NotChecked(NotChecked::Pending) {
                continue;
            }
            let Some(tool) = record.tool else {
                continue;
            };
            let rust = (tool.language == Language::Rust)
                .then(|| &*rust_setup.get_or_insert_with(|| RustSetup::read(root)));
            if let Some(rust) = rust {
                let refused = if let Toolchain::Refused(refusal) = rust.toolchain {
                    if !toolchain_warned {
                        toolchain_warned = true;
                        self.findings.push(toolchain_warning(refusal, &record.at));
                    }
                    Some(NotChecked::ToolchainPath)
                } else {
                    let read = record
                        .pending
                        .as_ref()
                        .and_then(|pending| rust::compile_time_read(&pending.input));
                    read.map(|read| {
                        self.findings.push(compile_time_warning(&read, &record.at));
                        NotChecked::CompileTimeRead
                    })
                };
                if let Some(refused) = refused {
                    record.state = BlockState::NotChecked(refused);
                    record.reason = record
                        .state
                        .reason(record.language.as_deref(), Some(tool.name));
                    continue;
                }
            }
            let found =
                if let Some((_, found)) = looked_up.iter().find(|(name, _)| *name == tool.name) {
                    found.clone()
                } else {
                    let found = find(tool.binary, env, root);
                    self.findings.extend(tool_warning(tool, &found, &record.at));
                    looked_up.push((tool.name, found.clone()));
                    found
                };
            record.state = match found {
                Found::OnPath(program) => {
                    let ran = run_formatter(tool, &program, record, env, root, timeout, rust);
                    let Some(state) = ran.map(|(verdict, findings)| {
                        self.findings.extend(findings);
                        verdict.state()
                    }) else {
                        continue;
                    };
                    state
                }
                Found::Missing => BlockState::ToolMissing,
                Found::InsideRoot(_) => BlockState::NotChecked(NotChecked::ToolInsideRoot),
            };
            record.reason = record
                .state
                .reason(record.language.as_deref(), Some(tool.name));
        }
        if self.counts.is_some() {
            self.counts = Some(CodeBlockCounts::tally(
                self.records.iter().map(|record| &record.state),
            ));
        }
    }
}

/// What every Rust tool call in a run shares, read once from the root.
#[derive(Debug)]
struct RustSetup {
    /// The toolchain the root's `rust-toolchain(.toml)` pins.
    toolchain: Toolchain,
    /// `--config-path` for an in-root `rustfmt.toml`, or nothing.
    config: Vec<OsString>,
}

impl RustSetup {
    /// Reads `root`'s toolchain file and finds its `rustfmt.toml`, each only inside `root`.
    ///
    /// Effect: reads file metadata, resolves symlinks, and reads the toolchain file.
    fn read(root: &Path) -> Self {
        Self {
            toolchain: toolchain::read(root),
            config: rust::config_args(root),
        }
    }
}

/// Runs `tool`, found at `program`, on `record`'s block from `root` with `env`, under `timeout`,
/// adding `rust`'s config arguments and toolchain variables for a Rust tool:
/// its verdict and findings, an unformatted block's error carrying the fix to the formatter's
/// output, or followed by a `code-block-fix-unsafe` warning when [`format::safe_fix`] finds that
/// output could end the block early. `None` when the block's language has no format step yet or its input
/// couldn't be read, so it stays pending.
///
/// Effect: runs the tool, killing it at the timeout.
fn run_formatter(
    tool: &Tool,
    program: &Path,
    record: &BlockRecord,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<Duration>,
    rust: Option<&RustSetup>,
) -> Option<(format::Verdict, Vec<Finding>)> {
    let pending = record.pending.as_ref()?;
    let input = pending.input.as_str();
    let doc = root.join(record.at.file.as_str());
    let mut args = format::args(tool, &doc, record.index)?;
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
    let call = Call {
        program,
        name: tool.binary,
        args: &args,
        extra: &extra,
        stdin: input.as_bytes(),
        timeout,
    };
    let verdict = format::judge(input, runner::run(&call, env, root));
    let finding = format::finding(&verdict, tool, &record.at, timeout);
    let findings = match (&verdict, finding) {
        (format::Verdict::Unformatted(output), Some(unformatted)) => {
            format::fixed(unformatted, &pending.block, &pending.doc, output, tool)
        }
        (_, finding) => finding.into_iter().collect(),
    };
    Some((verdict, findings))
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

/// What the format step does with `block`, in `state`: a pending block marked `no_format(...)`
/// or in a language with `formatter = false` leaves it, with the author's reason when given;
/// any other pending block keeps its state with the formatter selected for `tag`.
fn format_choice(
    block: &CodeBlock,
    state: BlockState,
    tag: Option<&str>,
    code_blocks: &CodeBlocks,
) -> (BlockState, Option<&'static Tool>, Option<String>) {
    if state != BlockState::NotChecked(NotChecked::Pending) {
        return (state, None, None);
    }
    let language = tag.and_then(Language::from_tag);
    let no_format = block.info.as_ref().and_then(|info| {
        info.parsed()
            .attributes
            .iter()
            .find_map(|attribute| reasoned(attribute).filter(|found| found.name == "no_format"))
    });
    if let Some(found) = no_format {
        let skipped = BlockState::NotChecked(NotChecked::FormatSkipped);
        return (skipped, None, found.reason.map(str::to_owned));
    }
    if language.is_some_and(|language| code_blocks.formatter(language) == Some(&Formatter::Off)) {
        return (BlockState::NotChecked(NotChecked::FormatOff), None, None);
    }
    let tool = language.and_then(|language| selected(code_blocks, language));
    (state, tool, None)
}

/// The formatter `code_blocks` selects for `language`: the named one, or the default. `None` when
/// the language has none.
fn selected(code_blocks: &CodeBlocks, language: Language) -> Option<&'static Tool> {
    match code_blocks.formatter(language) {
        Some(Formatter::Named(name)) => TOOLS.iter().find(|tool| {
            tool.language == language && tool.step == Step::Format && tool.name == name
        }),
        Some(Formatter::Off) | None => formatter(language),
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

/// The one `code-block-not-checked` warning, at `at`, for the root's toolchain file refused for
/// `refusal`, which leaves every Rust block unchecked.
fn toolchain_warning(refusal: Refusal, at: &Location) -> Finding {
    let (why, help) = match refusal {
        Refusal::Path => (
            "names a toolchain `path`, which would run a binary the checkout chose",
            "pin a channel such as `stable` or `1.85.0` instead of a `path`",
        ),
        Refusal::InvalidChannel => (
            "names a channel supdoc doesn't accept, or doesn't parse",
            "name `stable`, `beta`, `nightly` or a version such as `1.85.0`, optionally with `-YYYY-MM-DD`",
        ),
        Refusal::Outside => (
            "resolves outside the project, or isn't a regular file of at most 64 KiB",
            "make `rust-toolchain.toml` a regular file inside the project",
        ),
    };
    Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        format!("the project's `rust-toolchain` file {why}, so its Rust blocks aren't checked"),
    )
    .with_help(help)
    .with_location(at.clone())
}

/// The `code-block-not-checked` warning, at `at`, for a Rust block holding `read`, a
/// compile-time read such as `include_str!`.
fn compile_time_warning(read: &str, at: &Location) -> Finding {
    Finding::new(
        NOT_CHECKED,
        Severity::Warning,
        format!(
            "the block uses `{read}`, which reads a file or environment variable at compile time, so it isn't checked"
        ),
    )
    .with_help(format!(
        "remove the `{read}` from the block, or mark it `ignore(<why>)`"
    ))
    .with_location(at.clone())
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
fn value_location(info: &Info, range: Range<usize>) -> Option<Location> {
    let before = info.text.get(..range.start)?.chars().count();
    let start = info.location.range.start + range.start;
    Some(Location {
        file: info.location.file.clone(),
        range: start..info.location.range.start + range.end,
        line: info.location.line,
        column: info.location.column + u32::try_from(before).ok()?,
    })
}
