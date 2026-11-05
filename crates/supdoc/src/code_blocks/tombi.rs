//! tombi: its JSON diagnostics, its syntax-error report, and the config supdoc gives it.
//!
//! tombi takes no config flag. It looks for a project config walking up from the directory it
//! runs in, all the way to `/`, then for a user config under `$XDG_CONFIG_HOME/tombi`. So supdoc
//! walks up from the doc inside the root for the project's config, refuses one that makes tombi
//! read files the checkout chose or fetch (`schemas`, `schema.catalog`, `extensions`), drops its
//! `files` table and every `enabled` switch (every block is checked), and writes what is left as
//! `.tombi.toml`, the first name tombi looks for, in a private directory tombi runs in, so the
//! nearest config tombi finds is always supdoc's copy. The copy is written even when the project
//! has no config, empty. The directory above it holds an empty `.tombi.toml` of supdoc's too, so
//! no config above supdoc's cache (in the home directory, say) ever applies. When tombi can't
//! load the copy it says so on stderr, naming it, and supdoc reports the block's steps not run.
//! `XDG_CONFIG_HOME` points at the private directory as well, holding the same copy.
//!
//! A block's own `#:schema` comment makes tombi read the file it names, wherever it is, so a
//! block carrying one is never given to tombi (see [`names_schema`]). A block's `#:tombi`
//! directive that switches a step off leaves that step not run (see [`switched_off`]): supdoc's
//! own `lint(off: ...)` and `format(off: ...)` are the way to do that.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::format::{self, ParseError};
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;
use crate::hash::Hash;

/// The config names tombi looks for in a directory, in its own order. `pyproject.toml` counts
/// only when it has a `[tool.tombi]` table.
const CONFIG_FILES: [&str; 4] = [
    ".tombi.toml",
    "tombi.toml",
    ".config/tombi.toml",
    "pyproject.toml",
];

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 256 * 1024;

/// The most bytes of a too-large `pyproject.toml` supdoc scans for `tool.tombi`.
const MAX_SCAN: u64 = 16 * 1024 * 1024;

/// What `tombi lint` writes to stderr after linting the one text it was given without error.
const LINTED_ONE: &str = "1 file linted successfully";

/// What `tombi lint` writes to stderr after finding diagnostics in the one text it was given.
const FAILED_ONE: &str = "1 file failed to be linted";

/// One entry of `tombi lint --diagnostics-format json`'s array.
#[derive(serde::Deserialize)]
struct Entry {
    /// Its code, such as `syntax/duplicate-key`; `syntax/` names a syntax error.
    code: String,
    /// What tombi says.
    message: String,
    /// Where it is.
    range: Range,
}

/// A diagnostic's place in tombi's JSON, 1-based.
#[derive(serde::Deserialize)]
struct Range {
    /// Its first character.
    start: Position,
    /// Just past its last character.
    end: Position,
}

/// A 1-based line and column in tombi's JSON.
#[derive(serde::Deserialize)]
struct Position {
    /// The line.
    line: usize,
    /// The column, in characters.
    column: usize,
}

/// The diagnostics in `tombi lint --diagnostics-format json`'s `stdout`. `None` when it isn't
/// that JSON array.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let entries: Vec<Entry> = serde_json::from_slice(stdout).ok()?;
    Some(
        entries
            .into_iter()
            .map(|entry| Diagnostic {
                line: entry.range.start.line,
                column: entry.range.start.column,
                end_line: entry.range.end.line.max(entry.range.start.line),
                syntax: entry.code.starts_with("syntax/"),
                rule: entry.code,
                message: entry.message,
            })
            .collect(),
    )
}

/// Judges a `tombi lint` run as [`lint::judge`] does, reading its stdout as tombi's JSON array,
/// except that a syntax error (a `syntax/` code) is a parse failure at its first line, as the
/// format step reports one. tombi checks no fragments, so no run is framed.
#[must_use]
pub(crate) fn lint_verdict(
    _input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    let syntax = result
        .as_ref()
        .ok()
        .filter(|ran| !ran.success)
        .and_then(|ran| diagnostics(&ran.stdout))
        .and_then(|found| found.into_iter().find(|found| found.syntax));
    match syntax {
        Some(syntax) => lint::Verdict::Failed(format::Verdict::ParseFailed {
            detail: syntax.message,
            line: syntax.line.checked_sub(1),
        }),
        None => lint::judge(result, diagnostics),
    }
}

/// Whether a `tombi lint` run that exited 0 linted nothing: its stderr doesn't report the one text
/// it was given as linted.
#[must_use]
pub(crate) fn linted_nothing(ran: &Ran) -> bool {
    let stderr = String::from_utf8_lossy(&ran.stderr);
    if ran.success {
        !stderr.contains(LINTED_ONE)
    } else {
        !stderr.contains(FAILED_ONE)
    }
}

/// `tombi format`'s syntax-error report in `stderr`, colour stripped: the line of the first
/// `Error: …` whose place, `<path>:<line>:<column>`, names a block's virtual path. `None` for any
/// other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let plain = strip_ansi(stderr);
    let mut lines = plain.lines();
    let _error = lines.find(|line| line.trim_start().starts_with("Error:"))?;
    let place = lines.next()?.trim().strip_prefix("at ")?;
    let mut parts = place.rsplitn(3, ':');
    let _column = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let path = parts.next()?;
    let (rest, extension) = path.rsplit_once('.')?;
    let (_, index) = rest.rsplit_once('.')?;
    (extension == "toml" && index.parse::<usize>().is_ok()).then_some(ParseError {
        line: line.checked_sub(1),
    })
}

/// `text` without its ANSI escape sequences (`ESC [ … <letter>`), which tombi writes even to a
/// pipe.
fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
}

/// Whether `block` holds a `#:schema` comment directive, which makes tombi read the file it names:
/// a line that, past its indentation, starts with `#:schema`.
#[must_use]
pub(crate) fn names_schema(block: &str) -> bool {
    block
        .lines()
        .any(|line| line.trim_start().starts_with("#:schema"))
}

/// The steps a `#:tombi` document directive in `block` switches off, as `(format, lint)`: a line
/// that, past its indentation, starts with `#:tombi` and sets `disabled` or `enabled` switches off
/// each step it names, `format` or `lint`, or both when it names neither. A `# tombi:` comment
/// that sets a rule's level is no switch: it stays in the block for a reader to see.
#[must_use]
pub(crate) fn switched_off(block: &str) -> (bool, bool) {
    block
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("#:tombi"))
        .filter(|rest| rest.contains("disabled") || rest.contains("enabled"))
        .fold((false, false), |(format, lint), rest| {
            let names_format = rest.contains("format");
            let names_lint = rest.contains("lint");
            let neither = !names_format && !names_lint;
            (
                format || names_format || neither,
                lint || names_lint || neither,
            )
        })
}

/// Why a block isn't given to tombi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's tombi config, at `file` (relative to the root), can't be given to tombi.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// supdoc has no private directory to keep tombi's config in.
    NoPrivateDir,
}

/// What tombi is given for the blocks of one doc directory: the private directory it runs in, and
/// where the doc is told to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// The private directory tombi runs in, holding the copy as `.tombi.toml` and again as
    /// `tombi/config.toml`, which is also its `XDG_CONFIG_HOME`.
    pub(crate) config_home: PathBuf,
    /// The directory the project's config resolves its patterns against (see [`pattern_root`]),
    /// or the root when there is no project config.
    from: PathBuf,
}

impl Project {
    /// Where tombi is told `doc` is: its position under the directory the project's config
    /// resolves patterns against, kept under the private directory, where the copy is, so
    /// `[[overrides]]` patterns match as they do in the project. The file need not exist.
    #[must_use]
    pub(crate) fn doc_as_seen(&self, doc: &Path) -> PathBuf {
        self.config_home.join(
            doc.strip_prefix(&self.from)
                .unwrap_or_else(|_| Path::new(doc.file_name().unwrap_or_default())),
        )
    }

    /// The copy of the project's config tombi reads: `.tombi.toml` in the private directory.
    #[must_use]
    pub(crate) fn copy(&self) -> PathBuf {
        self.config_home.join(".tombi.toml")
    }

    /// The variables tombi gets beyond the passed environment: `XDG_CONFIG_HOME`, the private
    /// directory, and `NO_COLOR`, so its messages carry no colour codes.
    #[must_use]
    pub(crate) fn vars(&self) -> [(&'static str, OsString); 2] {
        [
            ("XDG_CONFIG_HOME", self.config_home.as_os_str().to_owned()),
            ("NO_COLOR", OsString::from("1")),
        ]
    }

    /// How the private paths tombi prints are named to the user: the private directory, with
    /// and without a trailing `/`, as the block's virtual name relative to the directory its
    /// config resolves patterns against.
    #[must_use]
    pub(crate) fn names(&self) -> [(PathBuf, PathBuf); 2] {
        let mut slashed = self.config_home.clone().into_os_string();
        slashed.push("/");
        [
            (PathBuf::from(slashed), PathBuf::new()),
            (self.config_home.clone(), PathBuf::from(".")),
        ]
    }
}

/// The directory tombi resolves the patterns of the config at `config` against: the directory
/// holding a `.config/tombi.toml`'s `.config`, else the config's own directory.
fn pattern_root(config: &Path) -> Option<PathBuf> {
    let dir = config.parent()?;
    let dot_config = config.file_name().is_some_and(|name| name == "tombi.toml")
        && dir.file_name().is_some_and(|name| name == ".config");
    if dot_config {
        dir.parent().map(Path::to_path_buf)
    } else {
        Some(dir.to_path_buf())
    }
}

/// The [`Project`] tombi is given for `doc`: the project's config, found walking up from `doc`'s
/// directory to `root` (see [`candidates`]), its validated table written again as TOML into a
/// private directory named by the hash of what is written (see [`private_config`]); or an empty
/// copy when there is no config.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config file in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable TOML
/// file of at most 256 KiB, names `schemas`, a `schema.catalog` or `extensions`, or is a
/// `pyproject.toml` whose `[tool.tombi]` isn't a table; [`Refusal::NoPrivateDir`] when there is
/// nowhere private to put it. A `pyproject.toml` supdoc can't read as TOML, or that has no
/// `[tool.tombi]`, is skipped.
pub(crate) fn project(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Project, Refusal> {
    for candidate in candidates(root, doc) {
        if let Some(table) = checked(root, &candidate)? {
            let file = relative(root, &candidate);
            let config_home = private_config(env, root, &table).map_err(|why| match why {
                None => Refusal::NoPrivateDir,
                Some(why) => Refusal::Config { file, why },
            })?;
            let from = pattern_root(&candidate).unwrap_or_else(|| root.to_path_buf());
            return Ok(Project { config_home, from });
        }
    }
    let config_home = private_config(env, root, &toml::Table::new()).map_err(|why| match why {
        None => Refusal::NoPrivateDir,
        Some(why) => Refusal::Config {
            file: String::new(),
            why,
        },
    })?;
    Ok(Project {
        config_home,
        from: root.to_path_buf(),
    })
}

/// `path` relative to `root`, as shown to the user.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The config files in `doc`'s directory and its ancestors, nearest first, never above `root`.
fn candidates(root: &Path, doc: &Path) -> impl Iterator<Item = PathBuf> {
    let dirs: Vec<PathBuf> = doc
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.into_iter()
        .flat_map(|dir| CONFIG_FILES.map(|name| dir.join(name)))
        .filter(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// The validated tombi table of the config at `found`, without its `files` table; `None` for a
/// `pyproject.toml` that supdoc can't read as TOML or that has no `[tool.tombi]` table, which
/// supdoc doesn't count as a config.
///
/// # Errors
///
/// [`Refusal::Config`] naming what is wrong with it.
fn checked(root: &Path, found: &Path) -> Result<Option<toml::Table>, Refusal> {
    let file = relative(root, found);
    let refuse = |why| Refusal::Config {
        file: file.clone(),
        why,
    };
    let is_pyproject = found
        .file_name()
        .is_some_and(|name| name == "pyproject.toml");
    let table = match read_table(root, found) {
        Ok(table) => table,
        Err(ReadError::Oversized(real)) if is_pyproject => {
            return match mentions_tool_tombi(&real) {
                Some(false) => Ok(None),
                Some(true) => Err(refuse(
                    "is over 256 KiB and mentions `tool.tombi`, which supdoc doesn't read in a file that large",
                )),
                None => Err(refuse(
                    "is over 16 MiB, too large to scan for a `[tool.tombi]` table",
                )),
            };
        }
        Err(_) if is_pyproject => return Ok(None),
        Err(ReadError::Oversized(_)) => {
            return Err(refuse("isn't a regular file of at most 256 KiB"));
        }
        Err(ReadError::Other(why)) => return Err(refuse(why)),
    };
    let mut tombi = if is_pyproject {
        match table
            .get("tool")
            .and_then(toml::Value::as_table)
            .and_then(|tool| tool.get("tombi"))
        {
            Some(toml::Value::Table(tombi)) => tombi.clone(),
            Some(_) => return Err(refuse("has a `[tool.tombi]` that isn't a table")),
            None => return Ok(None),
        }
    } else {
        table
    };
    if let Some(why) = unsafe_part(&tombi) {
        return Err(refuse(why));
    }
    let _dropped = tombi.remove("files");
    drop_switches(&mut tombi);
    Ok(Some(tombi))
}

/// What in the config `table` tombi mustn't be given, `None` when nothing.
fn unsafe_part(table: &toml::Table) -> Option<&'static str> {
    if table.contains_key("schemas") {
        return Some("declares `schemas`, which make tombi read or fetch schema files");
    }
    let catalog = table
        .get("schema")
        .and_then(toml::Value::as_table)
        .is_some_and(|schema| schema.contains_key("catalog"));
    if catalog {
        return Some("declares a `schema.catalog`, which makes tombi fetch schemas");
    }
    if table.contains_key("extensions") {
        return Some("declares `extensions`, which make tombi read other files of the project");
    }
    None
}

/// Removes every key of `table` that switches a step off: `format.enabled` and `lint.enabled`, at
/// the top and in each `[[overrides]]` entry, so tombi checks every block (a switched-off step
/// would pass the block, or print nothing for it to format).
fn drop_switches(table: &mut toml::Table) {
    let steps = |table: &mut toml::Table| {
        for step in ["format", "lint"] {
            if let Some(toml::Value::Table(step)) = table.get_mut(step) {
                let _dropped = step.remove("enabled");
            }
        }
    };
    steps(table);
    if let Some(toml::Value::Array(overrides)) = table.get_mut("overrides") {
        for entry in overrides {
            if let toml::Value::Table(entry) = entry {
                steps(entry);
            }
        }
    }
}

/// Why a config file couldn't be read as a table.
enum ReadError {
    /// It is a regular file over 256 KiB, at the real path held.
    Oversized(PathBuf),
    /// Anything else, as a reason.
    Other(&'static str),
}

/// The TOML table in the file at `found`, which must resolve inside `root`.
///
/// # Errors
///
/// What is wrong with the file.
fn read_table(root: &Path, found: &Path) -> Result<toml::Table, ReadError> {
    let other = ReadError::Other;
    let real_root = root
        .canonicalize()
        .map_err(|_| other("couldn't be resolved"))?;
    let real = found
        .canonicalize()
        .map_err(|_| other("couldn't be resolved"))?;
    if !real.starts_with(&real_root) {
        return Err(other("is a link that resolves outside the project"));
    }
    let meta = fs::metadata(&real).map_err(|_| other("couldn't be read"))?;
    if !meta.is_file() {
        return Err(other("isn't a regular file of at most 256 KiB"));
    }
    if meta.len() > MAX_CONFIG {
        return Err(ReadError::Oversized(real));
    }
    let text = fs::read_to_string(&real).map_err(|_| other("couldn't be read as UTF-8"))?;
    text.parse::<toml::Table>()
        .map_err(|_| other("doesn't parse as TOML"))
}

/// Whether the file at `real` mentions `tool.tombi`. `None` when it is over 16 MiB or can't be
/// read, which is as unknown as it gets.
fn mentions_tool_tombi(real: &Path) -> Option<bool> {
    use std::io::Read as _;
    let file = fs::File::open(real).ok()?;
    if file.metadata().ok()?.len() > MAX_SCAN {
        return None;
    }
    let mut bytes = Vec::new();
    let _read = file.take(MAX_SCAN).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).contains("tool.tombi"))
}

/// Writes `table` as TOML into a private directory named by the hash of what is written, under
/// supdoc's `tombi-config` directory, as `.tombi.toml` (what tombi finds from its working
/// directory) and as `tombi/config.toml` (what it finds through `XDG_CONFIG_HOME`), and returns
/// that directory. `tombi-config` itself gets an empty `.tombi.toml`, which tombi finds instead of
/// any config further up when it can't read the copy. A file already holding the right content is
/// kept; anything else at the name is replaced by writing a temporary file beside it and renaming
/// it into place, so concurrent runs never see half a file. The directory is returned by its real
/// path.
///
/// Effect: creates the directories mode 0700 and the files mode 0600, and may replace stale ones.
///
/// # Errors
///
/// `None` when there is no private directory; `Some(why)` when the table can't be written.
fn private_config(
    env: &ToolEnv,
    root: &Path,
    table: &toml::Table,
) -> Result<PathBuf, Option<&'static str>> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return Err(None);
    }
    let text = toml::to_string(table).map_err(|_| Some("couldn't be written again as TOML"))?;
    let parent = neutral.parent().ok_or(None)?.join("tombi-config");
    let home = parent.join(Hash::digest(text.as_bytes()).to_string());
    let private = |path: &Path| {
        fs::symlink_metadata(path)
            .is_ok_and(|meta| meta.is_dir() && meta.permissions().mode().trailing_zeros() >= 6)
    };
    for path in [&parent, &home, &home.join("tombi")] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| None)?;
        if !private(path) {
            return Err(None);
        }
    }
    for (file, content) in [
        (parent.join(".tombi.toml"), ""),
        (home.join(".tombi.toml"), text.as_str()),
        (home.join("tombi/config.toml"), text.as_str()),
    ] {
        write_private(&file, content).ok_or(None)?;
    }
    // Its real path: tombi resolves the working directory's, and matches override patterns
    // against the doc only when the doc's path, which can't be resolved, starts with it.
    home.canonicalize().map_err(|_| None)
}

/// Writes `content` to `file`, mode 0600, unless it already holds exactly that: through a
/// temporary file in the same directory renamed into place. `None` when it can't.
///
/// Effect: may create the temporary file and replace `file`.
fn write_private(file: &Path, content: &str) -> Option<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static WRITERS: AtomicUsize = AtomicUsize::new(0);
    let current = fs::symlink_metadata(file)
        .ok()
        .filter(fs::Metadata::is_file)
        .and_then(|_| fs::read_to_string(file).ok());
    if current.as_deref() == Some(content) {
        return Some(());
    }
    let temp = file.with_file_name(format!(
        ".tombi.{}.{}.tmp",
        std::process::id(),
        WRITERS.fetch_add(1, Ordering::Relaxed)
    ));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut out| out.write_all(content.as_bytes()))
        .and_then(|()| fs::rename(&temp, file));
    if written.is_err() {
        let _own = fs::remove_file(&temp);
        return None;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::{
        Project, Refusal, diagnostics, linted_nothing, names_schema, parse_error, pattern_root,
        project, switched_off,
    };
    use crate::code_blocks::format::ParseError;
    use crate::code_blocks::lint::Diagnostic;
    use crate::code_blocks::runner::Ran;
    use crate::code_blocks::tools::ToolEnv;
    use std::fs;
    use std::path::Path;

    /// A `ToolEnv` whose cache home is `cache`.
    fn env_in(cache: &Path) -> ToolEnv {
        ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| cache.as_os_str().to_owned()))
    }

    /// The private `.tombi.toml` `project` names, checked to be the same as its
    /// `tombi/config.toml`.
    fn config_text(project: &Project) -> String {
        let read = |name: &str| {
            fs::read_to_string(project.config_home.join(name))
                .expect("the private config should be readable")
        };
        let copy = read(".tombi.toml");
        assert_eq!(copy, read("tombi/config.toml"), "the two copies differ");
        copy
    }

    #[test]
    fn tombis_json_gives_each_diagnostics_code_line_and_column_and_marks_syntax_errors() {
        let stdout = br#"[{"path":"/p/a.md.1.toml","level":"error","code":"syntax/duplicate-key","message":"duplicate key: a","range":{"start":{"line":2,"column":1},"end":{"line":2,"column":2}}},{"path":"/p/a.md.1.toml","level":"warning","code":"discouraged/key-empty","message":"an empty key is discouraged","range":{"start":{"line":3,"column":1},"end":{"line":3,"column":3}}}]"#;

        assert_eq!(
            diagnostics(stdout),
            Some(vec![
                Diagnostic {
                    line: 2,
                    column: 1,
                    end_line: 2,
                    syntax: true,
                    rule: "syntax/duplicate-key".to_owned(),
                    message: "duplicate key: a".to_owned(),
                },
                Diagnostic {
                    line: 3,
                    column: 1,
                    end_line: 3,
                    syntax: false,
                    rule: "discouraged/key-empty".to_owned(),
                    message: "an empty key is discouraged".to_owned(),
                },
            ])
        );
        assert_eq!(diagnostics(b"error: boom\n"), None);
    }

    #[test]
    fn a_coloured_format_error_on_the_virtual_path_is_a_parse_error_at_its_line() {
        let stderr = "\u{1b}[1;31m  Error\u{1b}[0m: \u{1b}[1mexpected value\u{1b}[0m\n    \u{1b}[90mat\u{1b}[0m \u{1b}[36m/p/docs/a.md.1.toml:3:5\u{1b}[0m\n";

        assert_eq!(parse_error(stderr), Some(ParseError { line: Some(2) }));
        assert_eq!(parse_error("Error: boom\n    at /p/tombi.toml:3:5\n"), None);
        assert_eq!(parse_error("something else\n"), None);
    }

    #[test]
    fn a_lint_run_that_reports_no_text_linted_linted_nothing() {
        let ran = |success, stderr: &str| Ran {
            success,
            stdout: b"[]".to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };

        assert!(!linted_nothing(&ran(true, "1 file linted successfully\n")));
        assert!(!linted_nothing(&ran(false, "1 file failed to be linted\n")));
        assert!(linted_nothing(&ran(true, "")));
        assert!(linted_nothing(&ran(true, "0 files linted\n")));
    }

    #[test]
    fn a_tombi_directive_switching_a_step_off_is_found_and_a_rule_comment_is_not() {
        assert_eq!(
            switched_off("#:tombi lint.disabled = true\na = 1\n"),
            (false, true)
        );
        assert_eq!(
            switched_off("  #:tombi format.disabled=true\n"),
            (true, false)
        );
        assert_eq!(
            switched_off("#:tombi lint = { enabled = false }\n"),
            (false, true)
        );
        assert_eq!(switched_off("#:tombi disabled = true\n"), (true, true));
        assert_eq!(
            switched_off("# tombi: lint.rules.key-empty.disabled = true\n\"\" = 1\n"),
            (false, false)
        );
        assert_eq!(
            switched_off("#:tombi toml-version = \"v1.0.0\"\n"),
            (false, false)
        );
    }

    #[test]
    fn a_schema_directive_anywhere_in_the_block_is_found() {
        assert!(names_schema("#:schema ./s.json\na = 1\n"));
        assert!(names_schema("a = 1\n  #:schema /etc/hosts\n"));
        assert!(!names_schema("a = \"#:schema\"\n# schema: x\n"));
    }

    #[test]
    fn the_nearest_config_is_passed_as_a_private_copy_without_its_files_table() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(root.path().join("tombi.toml"), "schemas = []\n").expect("root config");
        fs::write(
            root.path().join("docs/.tombi.toml"),
            "# a comment\n[files]\nexclude = [\"**\"]\n[format.rules]\nindent-width = 4\n",
        )
        .expect("docs config");
        let env = env_in(cache.path());

        let passed = project(&env, root.path(), &root.path().join("docs/deep/a.md"))
            .expect("the docs config is safe");
        let refused = project(&env, root.path(), &root.path().join("a.md"));

        assert!(
            !passed.config_home.starts_with(root.path()),
            "{:?} is inside the root",
            passed.config_home
        );
        assert_eq!(config_text(&passed), "[format.rules]\nindent-width = 4\n");
        assert!(
            matches!(refused, Err(Refusal::Config { ref file, .. }) if file == "tombi.toml"),
            "got {refused:?}"
        );
    }

    #[test]
    fn schemas_a_catalog_and_extensions_are_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let env = env_in(cache.path());
        let doc = root.path().join("a.md");
        let config = root.path().join("tombi.toml");
        let refused = |text: &str| {
            fs::write(&config, text).expect("config");
            matches!(
                project(&env, root.path(), &doc),
                Err(Refusal::Config { .. })
            )
        };

        assert!(refused(
            "[[schemas]]\npath = \"s.json\"\ninclude = [\"*.toml\"]\n"
        ));
        assert!(refused("[schema.catalog]\npaths = [\"https://x\"]\n"));
        assert!(refused(
            "[extensions]\n\"tombi-toml/cargo\" = { enabled = true }\n"
        ));
        assert!(!refused(
            "[schema]\nenabled = false\n[lint.rules]\nkey-empty = \"off\"\n"
        ));
    }

    #[test]
    fn every_switch_that_turns_a_step_off_is_dropped_and_other_overrides_kept() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(
            root.path().join("tombi.toml"),
            "[lint]\nenabled = false\n[[overrides]]\nfiles.include = [\"docs/**\"]\n\
             format.enabled = false\nlint.enabled = false\nlint.rules.key-empty = \"error\"\n",
        )
        .expect("config");

        let given = project(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("docs/a.md"),
        )
        .expect("a safe config");

        assert_eq!(
            config_text(&given),
            "[lint]\n\n[[overrides]]\n\n[overrides.files]\ninclude = [\"docs/**\"]\n\n\
             [overrides.format]\n\n[overrides.lint.rules]\nkey-empty = \"error\"\n"
        );
    }

    #[test]
    fn tombis_private_paths_are_named_as_the_blocks_virtual_name() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let doc = root.path().join("docs/a.md");
        let given = project(&env_in(cache.path()), root.path(), &doc).expect("no config is fine");
        let printed = format!(
            "Error: x\n    at {}.1.toml:2:5\n",
            given.doc_as_seen(&doc).display()
        );
        let named = crate::code_blocks::biome::Given {
            flags: Vec::new(),
            config: None,
            mirror: None,
            names: Some(given.names()),
        }
        .named(printed.as_bytes());

        assert_eq!(
            String::from_utf8_lossy(&named),
            "Error: x\n    at docs/a.md.1.toml:2:5\n"
        );
    }

    #[test]
    fn with_no_config_tombi_still_gets_an_empty_private_one() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let doc = root.path().join("docs/a.md");

        let given = project(&env_in(cache.path()), root.path(), &doc).expect("no config is fine");

        assert_eq!(config_text(&given), "");
        assert_eq!(given.doc_as_seen(&doc), given.config_home.join("docs/a.md"));
        let guard = given
            .config_home
            .parent()
            .expect("the private directory has a parent")
            .join(".tombi.toml");
        assert_eq!(
            fs::read_to_string(guard).expect("the guard config should be readable"),
            ""
        );
    }

    #[test]
    fn a_pyproject_counts_only_with_a_tool_tombi_table() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let env = env_in(cache.path());
        let doc = root.path().join("a.md");
        let pyproject = root.path().join("pyproject.toml");

        fs::write(&pyproject, "[tool.ruff]\nx = 1\n").expect("pyproject");
        let without = project(&env, root.path(), &doc).expect("no [tool.tombi] is no config");
        fs::write(&pyproject, "[tool]\ntombi = 1\n").expect("pyproject");
        let not_a_table = project(&env, root.path(), &doc);
        fs::write(&pyproject, "[tool.tombi.format.rules]\nindent-width = 4\n").expect("pyproject");
        let with = project(&env, root.path(), &doc).expect("a [tool.tombi] is a config");

        assert_eq!(config_text(&without), "");
        assert!(
            matches!(not_a_table, Err(Refusal::Config { .. })),
            "got {not_a_table:?}"
        );
        assert_eq!(config_text(&with), "[format.rules]\nindent-width = 4\n");
    }

    #[test]
    fn a_config_that_is_a_link_out_of_the_root_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let outside = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(outside.path().join("t.toml"), "[format]\n").expect("outside config");
        std::os::unix::fs::symlink(
            outside.path().join("t.toml"),
            root.path().join("tombi.toml"),
        )
        .expect("the link");

        let refused = project(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("a.md"),
        );

        assert!(
            matches!(refused, Err(Refusal::Config { .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn the_doc_is_mirrored_under_the_private_copy_from_the_configs_pattern_root() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join(".config")).expect("the fixture dirs");
        fs::write(root.path().join(".config/tombi.toml"), "[format]\n").expect("config");
        let doc = root.path().join("docs/a.md");

        let given = project(&env_in(cache.path()), root.path(), &doc).expect("a safe config");
        let roots = [
            pattern_root(&root.path().join(".config/tombi.toml")),
            pattern_root(&root.path().join("docs/tombi.toml")),
        ];

        assert_eq!(given.doc_as_seen(&doc), given.config_home.join("docs/a.md"));
        assert_eq!(
            roots,
            [
                Some(root.path().to_path_buf()),
                Some(root.path().join("docs"))
            ]
        );
    }

    #[test]
    fn with_no_private_dir_the_block_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");

        let refused = project(&ToolEnv::default(), root.path(), &root.path().join("a.md"));

        assert_eq!(refused, Err(Refusal::NoPrivateDir));
    }
}
