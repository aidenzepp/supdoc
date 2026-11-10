//! sqruff: its syntax-error report, its version, and the config supdoc gives it.
//!
//! sqruff has no `format` command; supdoc's format step runs `sqruff fix` on the block on stdin
//! and reads the fixed SQL from stdout. `fix` applies every rule its config turns on, so supdoc
//! always gives it a private config whose `rules` are only the layout and capitalisation groups
//! (see [`FORCED`]), so a rule that renames an alias or rewrites a join is off. sqruff also reads
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
//! never read), and writes again only what it sorted (see [`kept`]):
//!
//! - `[sqruff]`: `exclude_rules` and `max_line_length` are kept; `templater` must be `raw`
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

use super::format::ParseError;
use super::runner;
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

/// The `[sqruff]` keys supdoc keeps: they can only turn a rule off or set a rule's limit.
const KEPT: [&str; 2] = ["exclude_rules", "max_line_length"];

/// The `[sqruff]` keys supdoc drops: its own `dialect` and `rules` replace the project's, and
/// the rest choose files or change only how sqruff runs and reports.
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

/// What sqruff is given for the blocks of one doc directory: the private config, always, so
/// sqruff never searches for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// The private config file.
    config: PathBuf,
}

impl Project {
    /// The flags a block read in `dialect` gets: `--config <private file> --dialect <name>`.
    #[must_use]
    pub(crate) fn flags(&self, dialect: SqlDialect) -> Vec<OsString> {
        vec![
            OsString::from("--config"),
            self.config.clone().into_os_string(),
            OsString::from("--dialect"),
            OsString::from(dialect.name()),
        ]
    }
}

/// The [`Project`] sqruff is given for `doc`: the project's `.sqruff`, found walking up from
/// `doc`'s directory to `root`, sorted (see [`kept`]) behind [`FORCED`] into a private file named
/// by the hash of what is written; with no config, a private file holding [`FORCED`] alone.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config file in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable UTF-8
/// file of at most 256 KiB, or holds a line supdoc refuses (see [`kept`]);
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
            (Some(file.clone()), kept(&text).map_err(refuse)?)
        }
        None => (None, FORCED.to_owned()),
    };
    let config = private_config(env, root, &text).ok_or(match file {
        Some(file) => Refusal::Config {
            file,
            why: "couldn't be written again into supdoc's private directory",
        },
        None => Refusal::NoPrivateDir,
    })?;
    Ok(Project { config })
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

/// What supdoc writes again of the `.sqruff` `text`: [`FORCED`], then the kept `[sqruff]` keys
/// in order, then each kept section with its keys, with `\n` ending every line. Blank and
/// comment (`#`, `;`) lines are dropped.
///
/// # Errors
///
/// Why supdoc doesn't read the config: a carriage return that doesn't end a line, an indented
/// line (sqruff reads it as part of the value above, which supdoc doesn't sort), a section or a
/// `[sqruff]` key it doesn't know, a templater other than `raw` or a templater section other
/// than `raw`'s or `placeholder`'s, a key that names files to read, a key before the first
/// section, or a line that is none of a section header, a `key = value` or a comment.
fn kept(text: &str) -> Result<String, &'static str> {
    let text = text.replace("\r\n", "\n");
    if text.contains('\r') {
        return Err(
            "holds a carriage return that doesn't end a line, which sqruff may read as a line \
             break: end each line with `\\n` or `\\r\\n`",
        );
    }
    let mut core = String::from(FORCED);
    let mut sections = String::new();
    let mut section = Section::Top;
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
    fn the_flags_name_the_private_config_and_the_dialect() {
        let project = Project {
            config: PathBuf::from("/c/sqruff.cfg"),
        };

        assert_eq!(
            project.flags(SqlDialect::Tsql),
            ["--config", "/c/sqruff.cfg", "--dialect", "tsql"].map(OsString::from)
        );
    }
}
