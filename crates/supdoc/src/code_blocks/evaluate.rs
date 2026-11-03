//! The per-block pipeline that `check` runs: judges each code block and returns its findings
//! and the count of blocks in each state.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::config::code_blocks::Language;
use crate::finding::{Finding, Location, Severity};
use crate::hash::Hash;
use crate::lock::model::Lockfile;
use crate::read::cartridge::Languages;
use crate::read::error::CartridgeLoadError;

use super::block::{CodeBlock, Info};
use super::excerpt;
use super::format;
use super::info::{
    KNOWN_ATTRIBUTES, SourceKind, is_known_attribute, parenthesized_spelling, reasoned,
    source_attributes,
};
use super::runner::{self, Call};
use super::source::{self, SourceText, Unresolved};
use super::states::{Excerpt, NotChecked, state_of, tag_of};
use super::tools::{Found, Tool, ToolEnv, find, formatter};
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
    /// What a formatter reads for the block, kept while it is pending.
    input: Option<String>,
}

/// The findings for `blocks`, in block order, reading each `from(...)` and `uses(...)` source
/// with `languages`, and one `code-block-stale` error for each `from(...)` block whose body
/// differs from its parts. Then the `uses(...)` findings against `lock`'s `[code-blocks]`
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
    let mut doc: Option<(&str, Option<String>)> = None;
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
            let doc_name = block.fence.file.as_str();
            block_uses.extend(sources.uses.iter().map(|value| {
                let hash = recorded.get(&(doc_name, value.target.clone()));
                (value.target.clone(), UseState::of(value, hash))
            }));
            block_uses.extend(
                sources
                    .unresolved
                    .into_iter()
                    .map(|value| (value, UseState::Missing)),
            );
            written.extend(sources.uses);
            unresolved_uses.extend(sources.use_missing.then_some(block.fence.file.as_str()));
            outcome = sources.from_missing.then_some(Excerpt::SourceMissing);
            if let Some(parts) = sources.parts {
                let file = block.fence.file.as_str();
                if doc.as_ref().is_none_or(|(read, _)| *read != file) {
                    doc = Some((file, read_doc(config, file)));
                }
                let stale = doc
                    .as_ref()
                    .and_then(|(_, text)| text.as_ref())
                    .and_then(|text| {
                        let line_endings = config.hash_settings().line_endings;
                        let expected = excerpt::expected(&parts, line_endings);
                        excerpt::stale(block, text, &expected, line_endings)
                    });
                outcome = Some(if stale.is_some() {
                    Excerpt::Stale
                } else {
                    Excerpt::Current
                });
                findings.extend(stale);
            }
        }
        let state = state_of(block, outcome, |language| code_blocks.checks(language));
        let language = tag_of(block).map(str::to_owned);
        let input = if state == BlockState::NotChecked(NotChecked::Pending) {
            let file = block.fence.file.as_str();
            if doc.as_ref().is_none_or(|(read, _)| *read != file) {
                doc = Some((file, read_doc(config, file)));
            }
            doc.as_ref()
                .and_then(|(_, text)| text.as_deref())
                .and_then(|text| format::input(block, text))
        } else {
            None
        };
        records.push(BlockRecord {
            at: block.fence.clone(),
            reason: state.reason(language.as_deref()),
            language,
            state: state.clone(),
            uses: block_uses,
            index,
            input,
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
    /// `code-block-tool-timeout` error; a language without a format step stays pending.
    ///
    /// Effect: reads file metadata along `PATH`, resolves symlinks, and runs each found formatter
    /// once per block, killing it at the timeout.
    pub(crate) fn look_up_tools(&mut self, env: &ToolEnv, root: &Path) {
        let timeout = self.timeout;
        let mut looked_up: Vec<(&'static str, Found)> = Vec::new();
        for record in &mut self.records {
            if record.state != BlockState::NotChecked(NotChecked::Pending) {
                continue;
            }
            let Some(tool) = record
                .language
                .as_deref()
                .and_then(Language::from_tag)
                .and_then(formatter)
            else {
                continue;
            };
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
                    let Some(state) = run_formatter(tool, &program, record, env, root, timeout)
                        .map(|(verdict, finding)| {
                            self.findings.extend(finding);
                            verdict.state()
                        })
                    else {
                        continue;
                    };
                    state
                }
                Found::Missing => BlockState::ToolMissing,
                Found::InsideRoot(_) => BlockState::NotChecked(NotChecked::ToolInsideRoot),
            };
            record.reason = record.state.reason(record.language.as_deref());
        }
        if self.counts.is_some() {
            self.counts = Some(CodeBlockCounts::tally(
                self.records.iter().map(|record| &record.state),
            ));
        }
    }
}

/// Runs `tool`, found at `program`, on `record`'s block from `root` with `env`, under `timeout`:
/// its verdict and finding. `None` when the block's language has no format step yet or its input
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
) -> Option<(format::Verdict, Option<Finding>)> {
    let input = record.input.as_deref()?;
    let doc = root.join(record.at.file.as_str());
    let args = format::args(tool.language, &doc, record.index)?;
    let args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsString::as_os_str).collect();
    let call = Call {
        program,
        name: tool.binary,
        args: &args,
        extra: &[],
        stdin: input.as_bytes(),
        timeout,
    };
    let verdict = format::judge(input, runner::run(&call, env, root));
    let finding = format::finding(&verdict, tool, &record.at, timeout);
    Some((verdict, finding))
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
