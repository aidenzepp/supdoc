//! sqruff: its syntax-error report, its lint JSON, its version, and the configs supdoc gives it.
//!
//! The lint step runs `sqruff lint` on the block on stdin and reads its JSON report (see
//! [`lint_verdict`]), with `--parsing-errors` so a block sqruff can't parse is reported, never
//! passed. sqruff honors `-- noqa` comments, which can hide every finding, a parse error too; a
//! block they may switch off entirely is never given to sqruff, format or lint (see
//! [`switches_lint_off`]), and a block holding one is linted a second time with every `noqa`
//! comment off, so a parse error one hides still fails it (see [`holds_noqa`]).
//!
//! sqruff has no `format` command; supdoc's format step runs `sqruff fix` on the block on stdin
//! and reads the fixed SQL from stdout. `fix` applies every rule its config turns on, so supdoc
//! always gives it a private config whose `rules` are only the layout and capitalisation groups
//! (see [`FORCED`]), so a rule that renames an alias or rewrites a join is off. The lint step gets
//! a second private config, the same but with sqruff's own rule set (see [`LINT_FORCED`]). sqruff
//! also reads
//! config from comments inside the SQL (`-- sqruff:rules:all`), after `--config`, and no flag
//! turns that off, so a block holding one is never given to sqruff (see [`directive_line`]).
//! `fix` exits 0 even when a violation it can't fix remains (a long comment line), and 1 only on
//! a block it can't parse, so the step judges the output against the block, never the exit
//! status (see [`super::format::judge`]).
//!
//! supdoc passes `--ignore-local-config` and `--config` with its private file, so sqruff reads
//! no config in the directory it runs in, its ancestors, the home directory or
//! `~/.config/sqruff`. supdoc finds the project's `.sqruff` itself, walking up from the doc to the
//! root (sqruff's other names, `.sqruff.ini`, `sqruff.toml`, `.sqlfluff` and `pyproject.toml`, are
//! never read), and writes again only what it sorted (see [`sorted`]) into both copies:
//!
//! - `[sqruff]`: `exclude_rules`, when it lists only rule codes, and `max_line_length` are kept
//!   (sqruff reads an `exclude_rules` entry as a pattern, so `core` would exclude every rule);
//!   `templater` must be `raw`
//!   (the release binary has no other, and a pip install's jinja or python templater loads
//!   code); `library_path`, `load_macros_from_path` and `loader_search_path` are refused;
//!   `dialect` and `rules` are replaced by supdoc's own, and the keys that choose files or
//!   change only how sqruff runs are dropped.
//! - `[sqruff:indentation]`, `[sqruff:rules]`, `[sqruff:rules:<rule>]` and
//!   `[sqruff:layout:type:<type>]` hold rule settings and are kept whole.
//! - `[sqruff:templater:raw]` and `[sqruff:templater:placeholder]` are dropped (the private
//!   config forces `raw`); any other templater section is refused.
//!
//! Any other section or key, a key before the first section, an indented line (a continuation
//! of the line before) or a carriage return that doesn't end a line is refused, so nothing
//! reaches sqruff that supdoc didn't sort. The block's dialect is given on the command line
//! (`--dialect`), which beats the config.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::format::{self, ParseError};
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;
use crate::config::code_blocks::SqlDialect;
use crate::config::confine::{Boundary, ReadEscape};
use crate::hash::Hash;
use crate::path::{self, Component, fixed};

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 256 * 1024;

/// What supdoc's copy always opens with: the raw templater, and only the layout and
/// capitalisation rule groups (the set sqlfluff's `format` command fixes), so `fix` changes a
/// block's layout and case and nothing else. sqruff names its groups `layout` and
/// `capitalisation`; `LT` and `CP` are rule-code prefixes it refuses as unknown rules.
const FORCED: &str = "[sqruff]\ntemplater = raw\nrules = layout,capitalisation\n";

/// What supdoc's lint copy always opens with: the raw templater, and no `rules`, so sqruff lints
/// with its own default rule set (its `core` rules) less the project's `exclude_rules`.
const LINT_FORCED: &str = "[sqruff]\ntemplater = raw\n";

/// The `[sqruff]` keys supdoc keeps: they can only turn a rule off or set a rule's limit.
const KEPT: [&str; 2] = ["exclude_rules", "max_line_length"];

/// The `[sqruff]` keys supdoc drops: its own `dialect` and `rules` replace the project's, and
/// the rest choose files or change only how sqruff runs and reports. `disable_noqa` is dropped
/// too: supdoc reads the block's `noqa` comments itself (see [`switches_lint_off`]) and passes
/// [`DISABLE_NOQA`] when it wants them off.
const DROPPED: [&str; 14] = [
    "dialect",
    "rules",
    "sql_file_exts",
    "ignore",
    "ignore_templated_areas",
    "warnings",
    "processes",
    "large_file_skip_char_limit",
    "large_file_skip_byte_limit",
    "encoding",
    "output_line_length",
    "fix_even_unparsable",
    "disable_noqa",
    "nocolor",
];

/// The `[sqruff]` keys that make sqruff read files or load code a project chose, refused.
const READS_FILES: [&str; 3] = [
    "library_path",
    "load_macros_from_path",
    "loader_search_path",
];

/// The templater sections supdoc drops: `raw` is forced, and `placeholder` only matters when
/// chosen as the templater.
const INERT_TEMPLATERS: [&str; 2] = ["sqruff:templater:raw", "sqruff:templater:placeholder"];

/// The names sqruff reads an inline config directive under, lowercase: `-- sqruff:<key>:<value>`
/// and `-- sqlfluff:<key>:<value>`.
const DIRECTIVES: [&str; 2] = ["sqruff", "sqlfluff"];

/// The 0-based index of the first line of `block` that may hold an inline config directive, which
/// sqruff applies over supdoc's private config: it can turn every rule on, change the dialect, or
/// pick another templater. `None` when no line may.
///
/// sqruff 0.44 reads a directive from a line that, trimmed, starts with `-- sqruff:` or
/// `-- sqlfluff:` (`Linter::with_inline_config` in `crates/lib/src/core/linter/core.rs` and
/// `FluffConfig::process_raw_file_for_config` in `crates/lib/src/core/config.rs`), and from no
/// `/* */` comment. This scan matches more: a line with `--` anywhere whose text after it names
/// either, in any case, followed by `:` after any whitespace, with lines split at LF alone (so a
/// stray CR joins, never hides, a line). It reads text, not SQL, so a string or a trailing comment
/// counts too: a near miss keeps the block from sqruff, never passes it. Line `n` of `block` is
/// line `n` of the block's body.
#[must_use]
pub(crate) fn directive_line(block: &str) -> Option<usize> {
    block.split('\n').position(holds_directive)
}

/// Whether `line` has a `--` followed, anywhere after it, by a name in [`DIRECTIVES`] and then
/// `:` after any whitespace, in any case.
fn holds_directive(line: &str) -> bool {
    let line = line.to_ascii_lowercase();
    let Some((_, comment)) = line.split_once("--") else {
        return false;
    };
    DIRECTIVES.iter().any(|name| {
        comment.match_indices(name).any(|(at, _)| {
            comment
                .get(at..)
                .and_then(|rest| rest.strip_prefix(name))
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
    })
}

/// What sqruff puts where a rule code goes for a section it can't parse.
const UNPARSABLE: &str = "????";

/// The syntax error in `sqruff fix`'s `stderr`: the first report line under no rule (`L:   1 |
/// P:   1 | ???? | Unparsable section`), placed at its 1-based line. `None` when there is none:
/// any other non-zero exit is a tool failure.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    stderr.lines().find_map(|line| {
        let mut fields = line.split('|').map(str::trim);
        let row = fields
            .next()?
            .strip_prefix("L:")?
            .trim()
            .parse::<usize>()
            .ok()?;
        let _position = fields.next()?.strip_prefix("P:")?;
        (fields.next()? == UNPARSABLE).then_some(ParseError {
            line: row.checked_sub(1),
        })
    })
}

/// The one file name `sqruff lint -f json` reports a block read on stdin under.
const STDIN_NAME: &str = "<string>";

/// One finding of `sqruff lint -f json`.
#[derive(serde::Deserialize)]
struct Entry {
    /// Where it starts and ends.
    range: Range,
    /// What sqruff says.
    message: String,
    /// Its rule, such as `LT01`; `null` for a section sqruff can't parse.
    code: Option<String>,
}

/// Where a finding of `sqruff lint -f json` lies.
#[derive(serde::Deserialize)]
struct Range {
    /// Its start.
    start: Position,
    /// Its end.
    end: Position,
}

/// A place in the block `sqruff lint -f json` names.
#[derive(serde::Deserialize)]
struct Position {
    /// Its 1-based line.
    line: usize,
    /// Its 1-based column.
    character: usize,
}

/// What a finding sqruff can't parse the block at is named by, as sqlfluff names it.
const PARSE_RULE: &str = "PRS";

/// The diagnostics in `sqruff lint -f json`'s `stdout`: an object with one key, the stdin name
/// `<string>`, holding the findings. A finding with no rule code is a syntax error. `None` when
/// `stdout` is anything else.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let report: std::collections::BTreeMap<String, Vec<Entry>> =
        serde_json::from_slice(stdout).ok()?;
    let mut files = report.into_iter();
    let (name, entries) = files.next()?;
    if name != STDIN_NAME || files.next().is_some() {
        return None;
    }
    Some(
        entries
            .into_iter()
            .map(|entry| Diagnostic {
                line: entry.range.start.line,
                column: entry.range.start.character,
                end_line: entry.range.end.line.max(entry.range.start.line),
                syntax: entry.code.is_none(),
                rule: entry.code.unwrap_or_else(|| PARSE_RULE.to_owned()),
                message: entry.message,
            })
            .collect(),
    )
}

/// Judges a `sqruff lint` run by its output alone, never its exit status: anything on stderr is a
/// tool failure (sqruff says there that it refused its config, then prints no report); else a
/// report with no findings is clean, one with findings gives them, and any other stdout, empty
/// or unreadable, is a tool failure. sqruff checks no fragments, so no run is framed.
#[must_use]
pub(crate) fn lint_verdict(
    _input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    let failed = |detail| lint::Verdict::Failed(format::Verdict::ToolFailed(detail));
    let ran = match result {
        Ok(ran) => ran,
        Err(failure) => return lint::Verdict::Failed(format::failed(failure)),
    };
    if !String::from_utf8_lossy(&ran.stderr).trim().is_empty() {
        return failed(format::exit_detail(&ran.stderr));
    }
    match diagnostics(&ran.stdout) {
        Some(found) if found.is_empty() => lint::Verdict::Clean,
        Some(found) => lint::Verdict::Diagnostics(found),
        None => failed(
            "sqruff's lint report wasn't the JSON supdoc reads (an object naming `<string>`)"
                .to_owned(),
        ),
    }
}

/// How a `noqa` comment on one line reads to supdoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Noqa {
    /// It may switch every rule off on its line: no rule list, or one with an entry that isn't a
    /// plain rule code (see [`rule_code`]), such as `all`, `core`, `*` or an empty entry.
    Line,
    /// It may switch every rule off from its line to the block's end: a list naming `disable` or
    /// `enable` with an entry that isn't a plain rule code, such as `disable=all`.
    Range,
    /// It names only plain rule codes, which sqruff alone switches off.
    Rules,
}

/// Whether `-- noqa` comments in `block` may switch sqruff's every rule off for all of it, so a
/// lint run would pass whatever the block holds, a syntax error too: every line holding more
/// than whitespace holds a [`Noqa::Line`] comment or comes at or after a [`Noqa::Range`] one.
///
/// sqruff 0.44 reads `-- noqa`, `--noqa` and `/* noqa */` with no rule list as every rule off on
/// that line, its syntax error too, and `-- noqa: disable=all` as every rule off on the lines
/// after it, up to a `-- noqa: enable=all`. It reads each entry of a rule list, `disable=` and
/// `enable=` lists too, as a pattern over its rule codes, names and groups
/// (`IgnoreMask::expand_rule_references` in `crates/lib/src/core/rules/noqa.rs`), so `*`, `????`,
/// `core` or `layout` may name every rule. This scan matches more: `noqa` in any case anywhere
/// after a `--`, `/*` or `#` on the line, followed by anything but `:` and a list of plain rule
/// codes (see [`rule_code`]); and a range from any list naming `disable` or `enable` with any
/// other entry. It reads no `enable=all`, so a range runs to the block's end. It reads text, not
/// SQL, so a string counts too, and lines split at LF alone. A near miss keeps the block from
/// being called linted, never passes it; a block with no `noqa` comment is never switched off.
#[must_use]
pub(crate) fn switches_lint_off(block: &str) -> bool {
    let mut any = false;
    let mut range = false;
    let covered = block.split('\n').all(|line| {
        let found = noqa_comments(line);
        any |= !found.is_empty();
        range |= found.contains(&Noqa::Range);
        range || found.contains(&Noqa::Line) || line.trim().is_empty()
    });
    any && covered
}

/// How each `noqa` in `line` that follows a comment opener (`--`, `/*` or `#`) reads, in order.
fn noqa_comments(line: &str) -> Vec<Noqa> {
    let lower = line.to_ascii_lowercase();
    let Some(opened) = ["--", "/*", "#"]
        .iter()
        .filter_map(|opener| lower.find(opener))
        .min()
    else {
        return Vec::new();
    };
    let comment = lower.get(opened..).unwrap_or_default();
    comment
        .match_indices("noqa")
        .map(|(at, word)| noqa_kind(comment.get(at + word.len()..).unwrap_or_default()))
        .collect()
}

/// How a `noqa` followed by `rest`, lowercase, reads (see [`Noqa`]): a rule list, up to a `*/`,
/// names rules only when each of its comma-separated entries is a plain rule code; a list naming
/// `disable` or `enable` is a range, its entries those after the first `=`.
fn noqa_kind(rest: &str) -> Noqa {
    let Some(list) = rest.trim_start().strip_prefix(':') else {
        return Noqa::Line;
    };
    let list = list.split("*/").next().unwrap_or_default();
    let ranged = list.contains("disable") || list.contains("enable");
    let entries = if ranged {
        list.split_once('=').map_or("", |(_, entries)| entries)
    } else {
        list
    };
    if rule_codes(entries) {
        Noqa::Rules
    } else if ranged {
        Noqa::Range
    } else {
        Noqa::Line
    }
}

/// Whether `entry` is a plain sqruff rule code: two ASCII letters, then two ASCII digits, such
/// as `LT01`, in any case. sqruff reads it as a pattern too, but one matching only that code.
fn rule_code(entry: &str) -> bool {
    matches!(
        entry.as_bytes(),
        [first, second, third, fourth]
            if first.is_ascii_alphabetic()
                && second.is_ascii_alphabetic()
                && third.is_ascii_digit()
                && fourth.is_ascii_digit()
    )
}

/// Whether `list`, comma-separated, holds only plain rule codes (see [`rule_code`]). sqruff reads
/// each entry of an `exclude_rules` setting or a `noqa` list as a pattern over its rule codes,
/// names and groups (`crates/lib/src/core/rules.rs` and `crates/lib/src/core/rules/noqa.rs`), so
/// `core`, `all` or `*` would name every rule.
fn rule_codes(list: &str) -> bool {
    list.split(',').all(|entry| rule_code(entry.trim()))
}

/// The flag that makes sqruff ignore every inline `noqa` comment (`disable_noqa` in
/// `crates/cli-lib/src/commands.rs` and `crates/cli-lib/src/lib.rs` of sqruff 0.44).
pub(crate) const DISABLE_NOQA: &str = "--disable-noqa";

/// Whether `block` may hold a `noqa` comment: it holds `noqa` anywhere, in any case. sqruff lets
/// one with no rule list hide a parse error on its line, and a parse error is never suppressible,
/// so the lint step lints such a block a second time with [`DISABLE_NOQA`] and fails it on any
/// parse error that run reports (see [`parse_errors`]).
#[must_use]
pub(crate) fn holds_noqa(block: &str) -> bool {
    block.to_ascii_lowercase().contains("noqa")
}

/// What `verdict`, a lint run's with every `noqa` comment off (see [`holds_noqa`]), adds to the
/// block's own lint run: its parse errors alone, or its failure. `None` when it reports no parse
/// error, so the block's own run judges it, its `noqa` comments honored.
#[must_use]
pub(crate) fn parse_errors(verdict: lint::Verdict) -> Option<lint::Verdict> {
    match verdict {
        lint::Verdict::Clean => None,
        lint::Verdict::Diagnostics(found) => {
            let syntax: Vec<Diagnostic> = found.into_iter().filter(|found| found.syntax).collect();
            (!syntax.is_empty()).then_some(lint::Verdict::Diagnostics(syntax))
        }
        failed => Some(failed),
    }
}

/// The sqruff versions, as `<major>.<minor>`, supdoc's checks are verified against: supdoc
/// writes sqruff's config and reads its output, and another version may read or write either
/// differently. A minor is added only after the SQL e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 1] = ["0.44"];

/// What `sqruff --version` prints before the version.
const VERSION_PREFIX: &str = "sqruff ";

/// How a sqruff whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// The version `sqruff --version` printed in `stdout` (`sqruff <major>.<minor>.<patch>`) when it
/// is one supdoc is verified against (see [`VERIFIED`]).
///
/// # Errors
///
/// How to name the version to the user: the version it printed, or, when its output isn't a
/// version, that.
pub(crate) fn verified(stdout: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(stdout);
    let version = text
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end()
        .strip_prefix(VERSION_PREFIX)
        .filter(|version| {
            let parts: Vec<&str> = version.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
        .ok_or_else(|| UNREADABLE.to_owned())?;
    let minor = version.rsplit_once('.').map_or(version, |(minor, _)| minor);
    if VERIFIED.contains(&minor) {
        Ok(version.to_owned())
    } else {
        Err(version.to_owned())
    }
}

/// [`verified`] on what `program`, a sqruff binary, prints for `--version`, run as any tool is
/// (see [`runner::run`]) under `timeout`.
///
/// Effect: runs the binary.
///
/// # Errors
///
/// How to name the version to the user, as [`verified`] gives it; a run that failed or exited
/// unsuccessfully couldn't be read.
pub(crate) fn version_of(
    program: &Path,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<std::time::Duration>,
) -> Result<String, String> {
    let call = runner::Call {
        program,
        name: "sqruff",
        args: &[std::ffi::OsStr::new("--version")],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Neutral,
        cap: runner::CAP,
    };
    match runner::run(&call, env, root) {
        Ok(ran) if ran.success => verified(&ran.stdout),
        _ => Err(UNREADABLE.to_owned()),
    }
}

/// Why a block isn't given to sqruff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's `.sqruff`, at `file` (relative to the root), can't be given to sqruff.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// supdoc has no private directory to keep sqruff's config in.
    NoPrivateDir,
}

/// What sqruff is given for the blocks of one doc directory: a private config for each step,
/// always, so sqruff never searches for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// The format step's private config file.
    config: PathBuf,
    /// The lint step's private config file.
    lint: PathBuf,
}

impl Project {
    /// The flags a block read in `dialect` gets for its format step: `--config <private file>
    /// --dialect <name>`.
    #[must_use]
    pub(crate) fn flags(&self, dialect: SqlDialect) -> Vec<OsString> {
        flags(&self.config, dialect)
    }

    /// The flags a block read in `dialect` gets for its lint step: `--config <private lint file>
    /// --dialect <name>`.
    #[must_use]
    pub(crate) fn lint_flags(&self, dialect: SqlDialect) -> Vec<OsString> {
        flags(&self.lint, dialect)
    }
}

/// `--config <config> --dialect <name of dialect>`.
fn flags(config: &Path, dialect: SqlDialect) -> Vec<OsString> {
    vec![
        OsString::from("--config"),
        config.as_os_str().to_owned(),
        OsString::from("--dialect"),
        OsString::from(dialect.name()),
    ]
}

/// The [`Project`] sqruff is given for `doc`: the project's `.sqruff`, found walking up from
/// `doc`'s directory to `root`, sorted (see [`sorted`]) behind [`FORCED`] for the format step and
/// behind [`LINT_FORCED`] for the lint step, each into a private file named by the hash of what
/// is written; with no config, private files holding those alone.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config files in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable UTF-8
/// file of at most 256 KiB, or holds a line supdoc refuses (see [`sorted`]);
/// [`Refusal::NoPrivateDir`] when there is nowhere private to put it.
pub(crate) fn project_args(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Project, Refusal> {
    let (file, text) = match candidates(root, doc).next() {
        Some(found) => {
            let file = relative(root, &found);
            let refuse = |why| Refusal::Config {
                file: file.clone(),
                why,
            };
            let text = read_text(root, &found).map_err(refuse)?;
            (Some(file.clone()), sorted(&text).map_err(refuse)?)
        }
        None => (None, String::new()),
    };
    let unwritten = || match &file {
        Some(file) => Refusal::Config {
            file: file.clone(),
            why: "couldn't be written again into supdoc's private directory",
        },
        None => Refusal::NoPrivateDir,
    };
    let config = private_config(env, root, &format!("{FORCED}{text}")).ok_or_else(unwritten)?;
    let lint = private_config(env, root, &format!("{LINT_FORCED}{text}")).ok_or_else(unwritten)?;
    Ok(Project { config, lint })
}

/// `path` relative to `root`, as shown to the user.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The `.sqruff` files in `doc`'s directory and its ancestors, nearest first, never above `root`.
fn candidates(root: &Path, doc: &Path) -> impl Iterator<Item = PathBuf> {
    let dirs: Vec<PathBuf> = doc
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.into_iter()
        .map(|dir| path::beneath(&dir, &fixed::DOT_SQRUFF))
        .filter(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// The text of the config at `found`, which must resolve inside `root`.
///
/// # Errors
///
/// What is wrong with the file.
fn read_text(root: &Path, found: &Path) -> Result<String, &'static str> {
    let real = Boundary::resolve(root)
        .real_inside(found)
        .map_err(|escape| match escape {
            ReadEscape::Outside { .. } => "is a link that resolves outside the project",
            ReadEscape::BoundaryUnresolved | ReadEscape::Unresolved => "couldn't be resolved",
        })?;
    let meta = fs::metadata(&real).map_err(|_| "couldn't be read")?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err("isn't a regular file of at most 256 KiB");
    }
    fs::read_to_string(&real).map_err(|_| "couldn't be read as UTF-8")
}

/// The section a line of the config sits in, as supdoc sorts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// Before the first section header.
    Top,
    /// `[sqruff]`: each key is kept, dropped or refused on its own.
    Core,
    /// A rule-settings section, kept whole.
    Kept,
    /// An inert templater section, dropped whole.
    Dropped,
}

/// What supdoc writes again of the `.sqruff` `text` for the format step: [`FORCED`], then what
/// [`sorted`] keeps.
///
/// # Errors
///
/// Why supdoc doesn't read the config, as [`sorted`] gives it.
#[cfg(test)]
fn kept(text: &str) -> Result<String, &'static str> {
    sorted(text).map(|kept| format!("{FORCED}{kept}"))
}

/// What supdoc keeps of the `.sqruff` `text`, to write behind [`FORCED`] or [`LINT_FORCED`]: the
/// kept `[sqruff]` keys in order, then each kept section with its keys, with `\n` ending every
/// line. Blank and comment (`#`, `;`) lines are dropped.
///
/// # Errors
///
/// Why supdoc doesn't read the config: a carriage return that doesn't end a line, an indented
/// line (sqruff reads it as part of the value above, which supdoc doesn't sort), a section or a
/// `[sqruff]` key it doesn't know, a templater other than `raw` or a templater section other
/// than `raw`'s or `placeholder`'s, a key that names files to read, a key before the first
/// section, a line that is none of a section header, a `key = value` or a comment, or an
/// `exclude_rules` that lists more than rule codes (see [`rule_codes`]).
fn sorted(text: &str) -> Result<String, &'static str> {
    let text = text.replace("\r\n", "\n");
    if text.contains('\r') {
        return Err(
            "holds a carriage return that doesn't end a line, which sqruff may read as a line \
             break: end each line with `\\n` or `\\r\\n`",
        );
    }
    let mut core = String::new();
    let mut sections = String::new();
    let mut section = Section::Top;
    let mut unplain = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(
                "holds an indented line, which sqruff reads as part of the value above it: start \
                 every key and section at the start of a line",
            );
        }
        if let Some(name) = header(line) {
            section = sorted_section(name)?;
            if section == Section::Kept {
                sections.push_str(line.trim_end());
                sections.push('\n');
            }
            continue;
        }
        let (key, value) = key_value(line).ok_or(
            "holds a line supdoc doesn't read: write each setting plainly as `<key> = <value>` \
             under a `[section]`",
        )?;
        match section {
            Section::Top => {
                return Err("holds a setting before its first `[section]`");
            }
            Section::Core => {
                unplain |= key == "exclude_rules" && !rule_codes(value);
                if core_key(&key, value)? {
                    core.push_str(line.trim_end());
                    core.push('\n');
                }
            }
            Section::Kept => {
                sections.push_str(line.trim_end());
                sections.push('\n');
            }
            Section::Dropped => {}
        }
    }
    if unplain {
        return Err(
            "sets `exclude_rules` to more than rule codes, and sqruff reads each entry as a \
             pattern that may name every rule (`core`, `all`, `*`): list each rule by its code, \
             such as `exclude_rules = LT05, AL03`",
        );
    }
    core.push_str(&sections);
    Ok(core)
}

/// The section name a `[name]` header line names, `None` for any other line.
fn header(line: &str) -> Option<&str> {
    line.trim_end().strip_prefix('[')?.strip_suffix(']')
}

/// How supdoc sorts the section `name`.
///
/// # Errors
///
/// Why the section is refused: a templater other than `raw` or `placeholder`, or a section
/// supdoc doesn't know.
fn sorted_section(name: &str) -> Result<Section, &'static str> {
    if name == "sqruff" {
        return Ok(Section::Core);
    }
    if INERT_TEMPLATERS.contains(&name) {
        return Ok(Section::Dropped);
    }
    if name.starts_with("sqruff:templater") {
        return Err(
            "configures a templater other than `raw`, which can load code or read files: remove \
             its `[sqruff:templater:...]` section",
        );
    }
    let rule_settings = name == "sqruff:indentation"
        || name == "sqruff:rules"
        || name
            .strip_prefix("sqruff:rules:")
            .is_some_and(|rule| plain(rule, b'.'))
        || name
            .strip_prefix("sqruff:layout:type:")
            .is_some_and(|kind| plain(kind, b'_'));
    if rule_settings {
        Ok(Section::Kept)
    } else {
        Err(
            "holds a section supdoc doesn't read: keep only `[sqruff]`, `[sqruff:indentation]`, \
             `[sqruff:rules]`, `[sqruff:rules:<rule>]` and `[sqruff:layout:type:<type>]`",
        )
    }
}

/// Whether `name` is non-empty and only ASCII letters, digits, `_` and `extra`.
fn plain(name: &str, extra: u8) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == extra)
}

/// The key, lowercased as sqruff reads it, and the value of a `key = value` or `key: value`
/// line whose key is a plain name. `None` for any other line.
fn key_value(line: &str) -> Option<(String, &str)> {
    let at = line.find(['=', ':'])?;
    let key = line.get(..at)?.trim_end();
    let value = line.get(at + 1..)?.trim();
    plain(key, b'_').then(|| (key.to_ascii_lowercase(), value))
}

/// Whether the `[sqruff]` setting `key = value` is kept in supdoc's copy.
///
/// # Errors
///
/// Why it is refused: a templater other than `raw`, a key that names files to read, or a key
/// supdoc doesn't know.
fn core_key(key: &str, value: &str) -> Result<bool, &'static str> {
    if key == "templater" {
        return if value == "raw" {
            Ok(false)
        } else {
            Err(
                "sets a `templater` other than `raw`, which can load code or read files: remove \
                 it or set `templater = raw`",
            )
        };
    }
    if READS_FILES.contains(&key) {
        return Err(
            "sets a key that makes sqruff read files the project chose (`library_path`, \
             `load_macros_from_path` or `loader_search_path`): remove it",
        );
    }
    if KEPT.contains(&key) {
        return Ok(true);
    }
    if DROPPED.contains(&key) {
        return Ok(false);
    }
    Err(
        "sets a `[sqruff]` key supdoc doesn't read: keep only `exclude_rules` and `max_line_length`",
    )
}

/// Writes `text` into `sqruff.cfg` in a private directory named by the hash of `text`, under
/// supdoc's `sqruff-config` directory, and returns the file's path. A file already holding the
/// same content is kept; anything else at the name is replaced by writing a temporary file beside
/// it and renaming it into place, so concurrent runs never see half a file.
///
/// Effect: creates the directories mode 0700 and the file mode 0600, and may replace a stale one.
///
/// `None` when there is no private directory or the file can't be written.
fn private_config(env: &ToolEnv, root: &Path, text: &str) -> Option<PathBuf> {
    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let cache = runner::exact(neutral.parent()?)?;
    let parent = cache.beneath(&fixed::SQRUFF_CONFIG_DIR);
    let digest = Component::new(&Hash::digest(text.as_bytes()).to_string())?;
    let dir = parent.beneath(&digest);
    let file = dir.beneath(&fixed::SQRUFF_CFG);
    let parent_boundary = runner::create_private(&Boundary::resolve(&cache), &parent)?;
    let own = runner::create_private(&parent_boundary, &dir)?;
    runner::write_private(&own, &file, text)?;
    Some(file.as_path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unparsable_section_is_a_parse_error_at_its_line() {
        let stderr = "== [<string>] FAIL\nL:   3 | P:   1 | ???? | Unparsable section\n";

        assert_eq!(parse_error(stderr), Some(ParseError { line: Some(2) }));
    }

    #[test]
    fn a_report_under_a_rule_or_any_other_stderr_is_no_parse_error() {
        for stderr in [
            "== [<string>] FAIL\nL:   1 | P:  10 | LT01 | Expected single whitespace.\n",
            "Unknown rule(s) in configuration: ZZ. Available rules are: AL01\n",
            "thread 'main' panicked at x.rs:1:1\n",
            "",
        ] {
            assert_eq!(parse_error(stderr), None, "{stderr:?}");
        }
    }

    #[test]
    fn only_a_verified_minor_passes_the_version_gate() {
        assert_eq!(verified(b"sqruff 0.44.0\n"), Ok("0.44.0".to_owned()));
        assert_eq!(verified(b"sqruff 0.44.7\n"), Ok("0.44.7".to_owned()));
        assert_eq!(verified(b"sqruff 0.45.0\n"), Err("0.45.0".to_owned()));
        for unreadable in [
            &b"sqruff 0.44\n"[..],
            b"sqlfluff 0.44.0\n",
            b"sqruff 0.44.0-rc1\n",
            b"",
        ] {
            assert_eq!(
                verified(unreadable),
                Err(UNREADABLE.to_owned()),
                "{}",
                String::from_utf8_lossy(unreadable)
            );
        }
    }

    #[test]
    fn the_copy_forces_raw_and_layout_and_capitalisation_and_keeps_rule_settings() {
        let config = "# the project's\n[sqruff]\ndialect = postgres\nrules = all\n\
                      templater = raw\nexclude_rules = LT05\nsql_file_exts = .sql\n\
                      max_line_length = 120\n\n[sqruff:indentation]\ntab_space_size = 2\n\
                      [sqruff:rules:capitalisation.keywords]\ncapitalisation_policy = upper\n\
                      [sqruff:layout:type:comma]\nline_position = leading\n\
                      [sqruff:templater:placeholder]\nparam_style = colon\n; trailing\n";

        let copy = kept(config).expect("a plain config should be read");

        assert_eq!(
            copy,
            "[sqruff]\ntemplater = raw\nrules = layout,capitalisation\nexclude_rules = LT05\n\
             max_line_length = 120\n[sqruff:indentation]\ntab_space_size = 2\n\
             [sqruff:rules:capitalisation.keywords]\ncapitalisation_policy = upper\n\
             [sqruff:layout:type:comma]\nline_position = leading\n"
        );
    }

    #[test]
    fn a_templater_other_than_raw_is_refused_as_a_key_or_a_section() {
        for config in [
            "[sqruff]\ntemplater = jinja\n",
            "[sqruff]\nTemplater: python\n",
            "[sqruff]\ntemplater = raw\ntemplater = dbt\n",
            "[sqruff]\ntemplater = placeholder\n",
            "[sqruff:templater:jinja]\napply_dbt_builtins = true\n",
            "[sqruff:templater:dbt]\nproject_dir = .\n",
        ] {
            assert!(
                kept(config).is_err_and(|why| why.contains("templater")),
                "{config:?} should be refused"
            );
        }
    }

    #[test]
    fn a_key_that_reads_files_is_refused() {
        for key in READS_FILES {
            let config = format!("[sqruff]\n{key} = macros\n");
            assert!(
                kept(&config).is_err_and(|why| why.contains("read files")),
                "{key} should be refused"
            );
        }
    }

    #[test]
    fn a_bare_carriage_return_is_refused_so_it_cant_smuggle_a_line() {
        let refused = kept("[sqruff]\nexclude_rules = LT05\rtemplater = jinja\n");

        assert!(
            refused.is_err_and(|why| why.contains("carriage return")),
            "a lone \\r should be refused"
        );
    }

    #[test]
    fn a_crlf_config_is_read_with_plain_line_ends() {
        assert_eq!(
            kept("[sqruff]\r\nexclude_rules = LT05\r\n"),
            Ok(format!("{FORCED}exclude_rules = LT05\n"))
        );
    }

    #[test]
    fn a_line_supdoc_cant_sort_is_refused_never_dropped() {
        for config in [
            "[sqruff]\nexclude_rules = LT05,\n    LT01\n",
            "  [sqruff]\n",
            "exclude_rules = LT05\n",
            "[sqruff]\nnot a setting\n",
            "[sqruff]\nunknown_key = 1\n",
            "[sqlfluff]\nexclude_rules = LT05\n",
            "[sqruff:rules:]\nx = 1\n",
            "[sqruff:layout:type:a.b]\nx = 1\n",
            "[tool]\n",
        ] {
            assert!(kept(config).is_err(), "{config:?} should be refused");
        }
    }

    #[test]
    fn a_line_sqruff_reads_as_a_directive_is_found() {
        for (block, line) in [
            ("-- sqruff:rules:all\nselect 1\n", 0),
            ("select 1\n-- sqlfluff:dialect:tsql\n", 1),
            ("select 1\n   \t-- sqruff:templater:jinja\n", 1),
            ("select 1\r\n-- sqruff:rules:all\r\n", 1),
        ] {
            assert_eq!(directive_line(block), Some(line), "{block:?}");
        }
    }

    #[test]
    fn a_near_miss_counts_as_a_directive() {
        for block in [
            "--sqruff:rules:all",
            "--   SQRUFF:rules:all",
            "-- SqlFluff : dialect:tsql",
            "---- sqruff:rules:all",
            "select 1 -- sqruff:rules:all",
            "select '-- sqruff:rules:all'",
            "-- note sqlfluff:dialect:tsql",
            "select 1\r-- sqruff:rules:all",
            "-- sqruff\t:rules:all",
        ] {
            assert!(directive_line(block).is_some(), "{block:?} should count");
        }
    }

    #[test]
    fn a_block_without_a_directive_goes_to_sqruff() {
        for block in [
            "select 1\n",
            "-- formatted by sqruff\nselect 1\n",
            "select a - - b from sqruff_table\n",
            "/* sqruff:rules:all */ select 1\n",
            "-\n- sqruff:rules:all\n",
            "",
        ] {
            assert_eq!(directive_line(block), None, "{block:?}");
        }
    }

    #[test]
    fn an_empty_config_is_the_forced_settings_alone() {
        assert_eq!(kept("# nothing\n\n"), Ok(FORCED.to_owned()));
    }

    #[test]
    fn each_step_gets_its_own_private_config_and_the_dialect() {
        let project = Project {
            config: PathBuf::from("/c/sqruff.cfg"),
            lint: PathBuf::from("/l/sqruff.cfg"),
        };

        assert_eq!(
            project.flags(SqlDialect::Tsql),
            ["--config", "/c/sqruff.cfg", "--dialect", "tsql"].map(OsString::from)
        );
        assert_eq!(
            project.lint_flags(SqlDialect::Postgres),
            ["--config", "/l/sqruff.cfg", "--dialect", "postgres"].map(OsString::from)
        );
    }

    #[test]
    fn the_lint_copy_keeps_what_the_format_copy_keeps_without_its_rules() {
        let body =
            sorted("[sqruff]\nrules = all\nexclude_rules = AL03\n[sqruff:indentation]\nx = 1\n")
                .expect("a plain config should be read");

        assert_eq!(body, "exclude_rules = AL03\n[sqruff:indentation]\nx = 1\n");
        assert!(
            !LINT_FORCED.contains("rules"),
            "the lint copy should take sqruff's own rule set"
        );
    }

    /// A finished sqruff run that printed `stdout` and `stderr`.
    fn ran(stdout: &str, stderr: &str) -> Ran {
        Ran {
            success: stdout.contains("\"code\""),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_finding_is_a_diagnostic_at_its_line_and_column_and_a_null_code_a_syntax_error() {
        let stdout = r#"{"<string>":[{"range":{"start":{"line":2,"character":7},"end":{"line":2,"character":7}},"message":"Expected only single space.","severity":"Warning","source":"sqruff","code":"LT01"},{"range":{"start":{"line":4,"character":12},"end":{"line":5,"character":1}},"message":"Unparsable section","severity":"Warning","source":"sqruff","code":null}]}"#;

        let lint::Verdict::Diagnostics(found) = lint_verdict("", false, Ok(ran(stdout, ""))) else {
            panic!("a report with findings should give them");
        };

        assert_eq!(
            found,
            [
                Diagnostic {
                    line: 2,
                    column: 7,
                    end_line: 2,
                    syntax: false,
                    rule: "LT01".to_owned(),
                    message: "Expected only single space.".to_owned(),
                },
                Diagnostic {
                    line: 4,
                    column: 12,
                    end_line: 5,
                    syntax: true,
                    rule: PARSE_RULE.to_owned(),
                    message: "Unparsable section".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn an_empty_report_is_clean() {
        assert_eq!(
            lint_verdict("", false, Ok(ran(r#"{"<string>":[]}"#, ""))),
            lint::Verdict::Clean
        );
    }

    #[test]
    fn unreadable_json_or_anything_on_stderr_is_a_tool_failure() {
        for (stdout, stderr) in [
            ("", ""),
            ("not json", ""),
            ("[]", ""),
            ("{}", ""),
            (r#"{"x.sql":[]}"#, ""),
            (r#"{"<string>":[],"x.sql":[]}"#, ""),
            (r#"{"<string>":[{"message":"m","code":"LT01"}]}"#, ""),
            (
                r#"{"<string>":[]}"#,
                "Unknown rule(s) in configuration: ZZ.",
            ),
            ("", "thread 'main' panicked at config.rs:75:68"),
        ] {
            assert!(
                matches!(
                    lint_verdict("", false, Ok(ran(stdout, stderr))),
                    lint::Verdict::Failed(format::Verdict::ToolFailed(_))
                ),
                "{stdout:?} / {stderr:?} should be a tool failure"
            );
        }
    }

    #[test]
    fn noqa_on_every_line_or_a_disable_all_range_switches_lint_off() {
        for block in [
            "select 1 -- noqa\n",
            "select a, -- noqa\n\n  b from t --noqa\n",
            "SELECT TOP 5 * FROM t /* noqa */\n",
            "select 1 -- NOQA\n",
            "select 1 -- noqa extra words\n",
            "select 1 -- noqa:\n",
            "select 1 -- noqa: all\n",
            "select 1 -- noqa: LT01, ALL\n",
            "select 1 # noqa\n",
            "select '-- noqa'\n",
            "-- noqa: disable=all\nselect  a from t\n",
            "--noqa:disable=all\nselect 1\n",
            "-- NOQA: Disable = All\nselect 1\n",
            "select 1 -- noqa\n-- noqa: disable=all\nselect 2\n-- noqa: enable=all\nselect 3\n",
            "select 1 -- noqa: LT01 -- noqa\n",
            "select 1 -- noqa\r\n",
            "select 1 -- noqa: *\n",
            "select 1 -- noqa: ????\n",
            "select 1 -- noqa: [A-Z]*\n",
            "select 1 -- noqa: core\n",
            "select 1 -- noqa: layout\n",
            "select 1 -- noqa: layout.spacing\n",
            "select 1 -- noqa: PRS\n",
            "select 1 -- noqa: LT01,\n",
            "select 1 -- noqa: LT01,,AL03\n",
            "select 1 -- noqa: LT1\n",
            "select 1 -- noqa: LT001\n",
            "select 1 -- noqa: 01LT\n",
            "-- noqa: disable=*\nselect 1\n",
            "-- noqa: disable=core\nselect 1\n",
            "-- noqa: disable=LT01,*\nselect 1\n",
            "-- noqa: disable\nselect 1\n",
            "-- noqa: enable=*\nselect 1\n",
        ] {
            assert!(switches_lint_off(block), "{block:?} should switch lint off");
        }
    }

    #[test]
    fn noqa_naming_rules_or_missing_from_a_line_leaves_lint_on() {
        for block in [
            "select 1\n",
            "",
            "\n\n",
            "select 1 -- noqa: LT01\n",
            "select 1 -- noqa: LT01,AL03\n",
            "select 1 -- noqa: lt01 , al03\n",
            "select 1 /* noqa: LT01 */\n",
            "-- noqa: disable=LT01\nselect 1\n",
            "-- noqa: disable=LT01,AL03\nselect 1\n-- noqa: enable=LT01\n",
            "select a, -- noqa\nb from t\n",
            "select 1\n-- noqa: disable=all\n",
            "select 1 -- no qa\n",
            "select noqa from t\n",
        ] {
            assert!(!switches_lint_off(block), "{block:?} should leave lint on");
        }
    }

    #[test]
    fn only_two_letters_then_two_digits_are_a_plain_rule_code() {
        for code in ["LT01", "al03", "Cp02"] {
            assert!(rule_code(code), "{code:?} should be a plain rule code");
        }
        for entry in [
            "", "LT1", "LT001", "L01", "LTT01", "01LT", "L?01", "LT0*", "PRS", "all", "core",
        ] {
            assert!(
                !rule_code(entry),
                "{entry:?} shouldn't be a plain rule code"
            );
        }
    }

    #[test]
    fn any_noqa_in_any_case_marks_the_block_for_the_unmasked_run() {
        for block in [
            "select 1 -- noqa: LT01\n",
            "select 1 /* NoQa */\n",
            "select 'noqa'\n",
        ] {
            assert!(holds_noqa(block), "{block:?} should hold noqa");
        }
        assert!(!holds_noqa("select 1 -- no qa\n"));
    }

    #[test]
    fn the_unmasked_run_adds_only_its_parse_errors_or_its_failure() {
        let found = |syntax, rule: &str| Diagnostic {
            line: 1,
            column: 1,
            end_line: 1,
            syntax,
            rule: rule.to_owned(),
            message: String::new(),
        };
        assert_eq!(parse_errors(lint::Verdict::Clean), None);
        assert_eq!(
            parse_errors(lint::Verdict::Diagnostics(vec![found(false, "LT01")])),
            None
        );
        assert_eq!(
            parse_errors(lint::Verdict::Diagnostics(vec![
                found(false, "LT01"),
                found(true, PARSE_RULE),
            ])),
            Some(lint::Verdict::Diagnostics(vec![found(true, PARSE_RULE)]))
        );
        let failed = lint::Verdict::Failed(format::Verdict::ToolFailed("x".to_owned()));
        assert_eq!(parse_errors(failed.clone()), Some(failed));
    }

    #[test]
    fn exclude_rules_naming_more_than_rule_codes_is_refused() {
        for value in [
            "core",
            "all",
            "*",
            "LT*",
            "LT05, core",
            "layout",
            "LT05,",
            "PRS",
        ] {
            let refused = sorted(&format!("[sqruff]\nexclude_rules = {value}\n"));
            assert!(
                refused.is_err_and(|why| why.contains("`exclude_rules`")),
                "exclude_rules = {value} should be refused"
            );
        }
        assert_eq!(
            sorted("[sqruff]\nexclude_rules = LT05, AL03,CP02\n"),
            Ok("exclude_rules = LT05, AL03,CP02\n".to_owned())
        );
    }
}
