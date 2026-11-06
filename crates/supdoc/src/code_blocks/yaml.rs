//! yamlfmt and yamllint: their reports, the configs supdoc gives them, and the check that
//! yamlfmt's output says what the block said.
//!
//! yamlfmt reads the block on stdin and takes no file name for it. Without `-conf` it looks for a
//! config in the directory it runs in, then in each directory above, then in the system config
//! directory. So supdoc always passes `-conf` with a private file of its own and
//! `-no_global_conf`, and yamlfmt never searches. supdoc finds the project's config itself,
//! walking up from the doc to the root, and doesn't pass it as the project wrote it: it reads
//! only the keys it knows (see [`read_config`]), checks each value, drops the keys that select
//! files (`include`, `exclude` and the like, which never apply to text on stdin and could only
//! skip the block), and writes what is left again into a private file named by the hash of its
//! content. With no project config the private file is empty, which is yamlfmt's defaults.
//!
//! yamlfmt picks at random between two config names in one directory, so supdoc refuses a
//! directory that holds more than one. A `# !yamlfmt!:ignore` comment in a block doesn't stop
//! yamlfmt formatting text on stdin, so it switches nothing off.
//!
//! yamlfmt rewrites YAML it reads as something else: `{<n>, ...}` comes back as
//! `{<n>: '', '...': ''}`. So its output is a fix only when it parses to the same YAML structure
//! as the block (see [`same_structure`]).
//!
//! yamllint reads the block on stdin with no file name, so no per-path setting applies to it. With
//! no `-c` or `-d` it looks for a config in the directory it runs in and each one above it, then
//! in `YAMLLINT_CONFIG_FILE` or under `XDG_CONFIG_HOME`; supdoc always passes `-c` with a private
//! file of its own, so it reads none of those. supdoc finds the project's `.yamllint` itself and
//! writes again only what it checked (see [`lint_config`]): no `extends` but yamllint's
//! built-in configs, no `ignore` or `ignore-from-file` anywhere, `yaml-files` dropped, and
//! `document-start` off, since a block is never a whole file. A config holding a character
//! PyYAML reads as a line break where supdoc reads none, or any control but tab, is refused before
//! it is read, and every value that isn't a plain word, number or boolean is written back in
//! double quotes with everything outside printable ASCII escaped, so the copy says exactly what
//! supdoc checked. An empty config, or one that leaves no rule on, is refused: it would switch the
//! step off. yamllint runs in an empty private directory, and must print `yamllint 1.38.0` for
//! `--version` (see [`version_of`]); when it rejects the copy anyway, its words, which can quote
//! the config, are never shown.
//!
//! A block whose own comments switch off every rule its config leaves on (see
//! [`switches_lint_off`]) isn't given to
//! yamllint, and neither is one holding a control character YAML forbids (see
//! [`forbidden_char`]): yamllint reads a NUL as a sign of UTF-16 text and would skip its own
//! checks. The comment scan is conservative: it reads text, not YAML, so a comment inside a
//! multi-line scalar, or one an `enable` comment undoes, still switches the step off. That loses
//! coverage, never gives a pass.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use super::format::{self, ParseError};
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;
use crate::cartridges::yaml as yaml_cartridge;
use crate::config::confine::Boundary;
use crate::hash::Hash;
use crate::limits::TimeLimit;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Deadline, SourceFile};

/// The config names yamlfmt looks for in a directory. When more than one is there, yamlfmt
/// 0.21.0 takes one at random.
pub(crate) const CONFIG_FILES: [&str; 5] = [
    ".yamlfmt",
    "yamlfmt.yml",
    "yamlfmt.yaml",
    ".yamlfmt.yaml",
    ".yamlfmt.yml",
];

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 256 * 1024;

/// The top-level keys that only choose which files yamlfmt formats, or how it reports on them:
/// none applies to text on stdin, and supdoc drops each from the copy so none can skip a block.
/// `gitignore_path` names a file yamlfmt would read: dropped, it reads nothing.
const DROPPED: [&str; 10] = [
    "include",
    "exclude",
    "doublestar",
    "match_type",
    "extensions",
    "regex_exclude",
    "gitignore_excludes",
    "gitignore_path",
    "output_format",
    "continue_on_error",
];

/// The basic formatter's settings that take `true` or `false`.
const BOOLEANS: [&str; 12] = [
    "include_document_start",
    "retain_line_breaks",
    "retain_line_breaks_single",
    "disallow_anchors",
    "scan_folded_as_literal",
    "indentless_arrays",
    "drop_merge_tag",
    "trim_trailing_whitespace",
    "eof_newline",
    "strip_directives",
    "indent_root_array",
    "disable_alias_key_correction",
];

/// The basic formatter's settings that take a count.
const COUNTS: [&str; 4] = [
    "indent",
    "max_line_length",
    "pad_line_comments",
    "array_indent",
];

/// The settings that take one of a few words, with the words each accepts.
const CHOICES: [(&str, &[&str]); 4] = [
    ("type", &["basic", "kyaml"]),
    ("line_ending", &["lf", "crlf"]),
    ("force_array_style", &["flow", "block", ""]),
    ("force_quote_style", &["single", "double", ""]),
];

/// `yamlfmt`'s syntax-error report in `stderr`: a line `yaml: line <n>: <what>`, `<n>` 1-based in
/// the text it read. `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    stderr.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("yaml: line ")?;
        let (number, _) = rest.split_once(':')?;
        let number = number.parse::<usize>().ok()?;
        Some(ParseError {
            line: number.checked_sub(1),
        })
    })
}

/// Why a block isn't given to yamlfmt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's yamlfmt config, at `file` (relative to the root), can't be given to yamlfmt.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: String,
    },
    /// supdoc has no private directory to keep yamlfmt's config in.
    NoPrivateDir,
}

/// The flags yamlfmt is given for the blocks of `doc`: `-conf <private file>`, holding the
/// project's config as supdoc read it, found walking up from `doc`'s directory to `root` (see
/// [`found_config`]), or nothing at all when there is none.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config file in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the nearest directory with a config holds more than one, or the config
/// is a link out of the root, isn't a readable UTF-8 file of at most 256 KiB, or says something
/// [`read_config`] refuses; [`Refusal::NoPrivateDir`] when there is nowhere private to put it.
pub(crate) fn project_args(
    env: &ToolEnv,
    root: &Path,
    doc: &Path,
) -> Result<Vec<OsString>, Refusal> {
    let text = match found_config(root, doc)? {
        Some(found) => {
            let file = relative(root, &found);
            let refuse = |why: String| Refusal::Config {
                file: file.clone(),
                why,
            };
            let written = read_text(root, &found).map_err(|why| refuse(why.to_owned()))?;
            read_config(&written).map_err(refuse)?
        }
        None => String::new(),
    };
    let private = private_config(env, root, &text).ok_or(Refusal::NoPrivateDir)?;
    Ok(vec![OsString::from("-conf"), private.into_os_string()])
}

/// `path` relative to `root`, as shown to the user.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The config yamlfmt would find for `doc` were it run in `doc`'s directory: the one in the
/// nearest directory at or above it, never above `root`, that holds any of [`CONFIG_FILES`].
///
/// Effect: reads file metadata.
///
/// # Errors
///
/// [`Refusal::Config`] when that directory holds more than one, since yamlfmt picks one of them
/// at random.
fn found_config(root: &Path, doc: &Path) -> Result<Option<PathBuf>, Refusal> {
    for dir in doc
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
    {
        let present: Vec<PathBuf> = CONFIG_FILES
            .iter()
            .map(|name| dir.join(name))
            .filter(|candidate| fs::symlink_metadata(candidate).is_ok())
            .collect();
        match present.as_slice() {
            [] => {}
            [one] => return Ok(Some(one.clone())),
            [first, ..] => {
                return Err(Refusal::Config {
                    file: relative(root, first),
                    why: format!(
                        "sits beside another yamlfmt config ({}), and yamlfmt picks one of them at random",
                        present
                            .iter()
                            .skip(1)
                            .map(|other| format!("`{}`", relative(root, other)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        }
    }
    Ok(None)
}

/// The text of the config at `found`, which must resolve inside `root`.
///
/// # Errors
///
/// What is wrong with the file.
fn read_text(root: &Path, found: &Path) -> Result<String, &'static str> {
    let real_root = root.canonicalize().map_err(|_| "couldn't be resolved")?;
    let real = found.canonicalize().map_err(|_| "couldn't be resolved")?;
    if !real.starts_with(&real_root) {
        return Err("is a link that resolves outside the project");
    }
    let meta = fs::metadata(&real).map_err(|_| "couldn't be read")?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err("isn't a regular file of at most 256 KiB");
    }
    fs::read_to_string(&real).map_err(|_| "couldn't be read as UTF-8")
}

/// One setting as written: its key and its value, unquoted.
struct Setting {
    /// The key.
    key: String,
    /// The value, a quoted one decoded.
    value: String,
    /// Whether the value was quoted.
    quoted: bool,
}

/// The yamlfmt config `text` as supdoc gives it to yamlfmt: its `line_ending` and `formatter`
/// settings, each checked and written again one per line, the formatter's indented under it, every
/// value a string in double quotes or a bare `true`, `false` or count. Empty when nothing is left.
///
/// supdoc reads only the YAML a yamlfmt config needs: one document of `key: value` lines and a
/// `formatter:` block of them, with comments and blank lines. A key in [`DROPPED`] is skipped
/// with everything indented under it, whatever it holds.
///
/// # Errors
///
/// Why the config can't be given to yamlfmt: YAML outside that shape (a second document included,
/// which yamlfmt wouldn't read), a key supdoc doesn't know
/// yamlfmt 0.21.0 to read, a key written twice, a value of the wrong kind, or formatter settings
/// beside `type: kyaml`, which takes none.
pub(crate) fn read_config(text: &str) -> Result<String, String> {
    let outside = |line: usize| {
        format!(
            "has YAML on line {line} that supdoc doesn't read in a yamlfmt config, which it reads \
             as `key: value` lines and a `formatter:` block of them"
        )
    };
    let mut top: Vec<Setting> = Vec::new();
    let mut formatter: Option<Vec<Setting>> = None;
    let mut seen: Vec<String> = Vec::new();
    let mut lines = text.split('\n').enumerate().peekable();
    let mut started = false;
    while let Some((at, raw)) = lines.next() {
        let number = at + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if is_blank(line) {
            continue;
        }
        if line == "---" && !started {
            started = true;
            continue;
        }
        started = true;
        if line.starts_with([' ', '\t']) {
            return Err(outside(number));
        }
        let (key, rest) = split_key(line).ok_or_else(|| outside(number))?;
        if seen.iter().any(|known| *known == key) {
            return Err(format!("sets `{key}` twice"));
        }
        seen.push(key.to_owned());
        if DROPPED.contains(&key) {
            while lines
                .peek()
                .is_some_and(|(_, next)| is_blank(next) || continues(next))
            {
                let _skipped = lines.next();
            }
            continue;
        }
        match key {
            "line_ending" => {
                let (value, quoted) = scalar(rest).ok_or_else(|| outside(number))?;
                top.push(Setting {
                    key: key.to_owned(),
                    value,
                    quoted,
                });
            }
            "formatter" => {
                if !is_blank(rest) {
                    return Err(outside(number));
                }
                formatter = Some(formatter_block(&mut lines, &outside)?);
            }
            other => {
                return Err(format!(
                    "sets `{other}`, which supdoc doesn't know yamlfmt 0.21.0 to read"
                ));
            }
        }
    }
    let mut written = String::new();
    for setting in &top {
        let _infallible = writeln!(written, "{}: {}", setting.key, checked(setting)?);
    }
    if let Some(settings) = formatter {
        let kyaml = settings
            .iter()
            .any(|setting| setting.key == "type" && setting.value == "kyaml");
        if kyaml && settings.iter().any(|setting| setting.key != "type") {
            return Err("sets formatter options beside `type: kyaml`, which takes none".to_owned());
        }
        written.push_str("formatter:\n");
        for setting in &settings {
            let _infallible = writeln!(written, "  {}: {}", setting.key, checked(setting)?);
        }
    }
    Ok(written)
}

/// The settings of a `formatter:` block, read from `lines` up to the first line that isn't
/// indented, blank or a comment. Every setting line has the first one's indent.
///
/// # Errors
///
/// `outside` of the first line outside that shape, or a key written twice.
fn formatter_block<'text>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'text str)>>,
    outside: &impl Fn(usize) -> String,
) -> Result<Vec<Setting>, String> {
    let mut settings: Vec<Setting> = Vec::new();
    let mut indent = None;
    while let Some((at, raw)) = lines.next_if(|(_, next)| {
        is_blank(next.strip_suffix('\r').unwrap_or(next)) || next.starts_with([' ', '\t'])
    }) {
        let number = at + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if is_blank(line) {
            continue;
        }
        let body = line.trim_start_matches(' ');
        let width = line.len() - body.len();
        if body.starts_with('\t') || *indent.get_or_insert(width) != width {
            return Err(outside(number));
        }
        let (key, rest) = split_key(body).ok_or_else(|| outside(number))?;
        let (value, quoted) = scalar(rest).ok_or_else(|| outside(number))?;
        if settings.iter().any(|setting| setting.key == key) {
            return Err(format!("sets `formatter.{key}` twice"));
        }
        settings.push(Setting {
            key: key.to_owned(),
            value,
            quoted,
        });
    }
    Ok(settings)
}

/// Whether `line` continues the value of the top-level key above it: it is indented, or it is a
/// sequence item (`-` then a blank or the line's end). A document marker (`---`, `...`) never
/// does, so a second document reaches the refusal of YAML outside the config's shape.
fn continues(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    line.starts_with([' ', '\t'])
        || line == "-"
        || line.starts_with("- ")
        || line.starts_with("-\t")
}

/// Whether `line` says nothing: empty, spaces, or a comment.
fn is_blank(line: &str) -> bool {
    let rest = line.trim_start_matches([' ', '\t']);
    rest.is_empty() || rest.starts_with('#')
}

/// `line`'s key, lowercase letters and `_`, and what follows its `:`.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    let named = !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    (named && (rest.is_empty() || rest.starts_with(' '))).then_some((key, rest))
}

/// The scalar `rest` writes after a key's `:`, and whether it was quoted, a trailing comment
/// dropped: a plain word that starts with none of YAML's indicators, or a quoted string with no
/// escape and no quote inside. `None` for anything else, an empty value included.
fn scalar(rest: &str) -> Option<(String, bool)> {
    let rest = rest.trim_matches([' ', '\t']);
    let after_value = |after: &str| {
        let after = after.trim_start_matches([' ', '\t']);
        after.is_empty() || after.starts_with('#')
    };
    if let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let inner = rest.get(1..)?;
        let end = inner.find(quote)?;
        let (value, after) = (inner.get(..end)?, inner.get(end + 1..)?);
        let plain_text = !value.contains('\\') && !value.chars().any(char::is_control);
        return (plain_text && after_value(after)).then(|| (value.to_owned(), true));
    }
    let value = rest
        .find(" #")
        .or_else(|| rest.find("\t#"))
        .map_or(rest, |comment| rest.get(..comment).unwrap_or(rest))
        .trim_end_matches([' ', '\t']);
    let indicator = value.starts_with([
        '[', ']', '{', '}', ',', '&', '*', '!', '|', '>', '%', '@', '`', '#', '-', '?', ':', '"',
        '\'',
    ]);
    (!value.is_empty() && !indicator && !value.contains(": ")).then(|| (value.to_owned(), false))
}

/// `setting`'s value as supdoc writes it: `true` or `false`, a count, or one of its words in
/// double quotes.
///
/// # Errors
///
/// What is wrong with it.
fn checked(setting: &Setting) -> Result<String, String> {
    let key = setting.key.as_str();
    let value = setting.value.as_str();
    if BOOLEANS.contains(&key) {
        return match (setting.quoted, value) {
            (false, "true" | "True" | "TRUE") => Ok("true".to_owned()),
            (false, "false" | "False" | "FALSE") => Ok("false".to_owned()),
            _ => Err(format!("sets `{key}` to `{value}`, not `true` or `false`")),
        };
    }
    if COUNTS.contains(&key) {
        return value
            .parse::<u16>()
            .ok()
            .filter(|_| {
                let decimal = value.bytes().all(|b| b.is_ascii_digit());
                // yamlfmt reads `010` as octal 8: only the plain spelling means what supdoc writes.
                let canonical = value == "0" || !value.starts_with('0');
                !setting.quoted && decimal && canonical
            })
            .map(|count| count.to_string())
            .ok_or_else(|| {
                format!(
                    "sets `{key}` to `{value}`, not a count up to 65535 written in plain decimal digits"
                )
            });
    }
    if let Some((_, words)) = CHOICES.iter().find(|(name, _)| *name == key) {
        return if words.contains(&value) {
            Ok(format!("\"{value}\""))
        } else {
            Err(format!(
                "sets `{key}` to `{value}`, not one of {}",
                words
                    .iter()
                    .map(|word| format!("`{word}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
    }
    Err(format!(
        "sets `formatter.{key}`, which supdoc doesn't know yamlfmt 0.21.0 to read"
    ))
}

/// Writes yamlfmt's config `text` into `.yamlfmt` in a private directory (see [`private_file`]).
fn private_config(env: &ToolEnv, root: &Path, text: &str) -> Option<PathBuf> {
    private_file(env, root, ("yamlfmt-config", ".yamlfmt"), text).map(|(_, file)| file)
}

/// Writes `text` into the file `name` in a private directory named by the hash of `text`, under
/// supdoc's directory `kind`, and returns that directory as a boundary with the file's path. A
/// file already there with the same content is kept; anything else at the name is replaced by
/// writing a temporary file in the directory and renaming it into place, so concurrent runs never
/// see half a file. Each directory and the file are admitted beneath the directory they go in.
///
/// Effect: creates the directories mode 0700 and the file mode 0600, and may replace a stale file.
///
/// `None` when there is no private directory or the file can't be written.
fn private_file(
    env: &ToolEnv,
    root: &Path,
    (kind, name): (&str, &str),
    text: &str,
) -> Option<(Boundary, PathBuf)> {
    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let cache = runner::exact(neutral.parent()?)?;
    let parent = AbsolutePath::new(&cache.as_path().join(kind))?;
    let dir = AbsolutePath::new(
        &parent
            .as_path()
            .join(Hash::digest(text.as_bytes()).to_string()),
    )?;
    let file = AbsolutePath::new(&dir.as_path().join(name))?;
    let parent_boundary = runner::create_private(&Boundary::resolve(&cache), &parent)?;
    let own = runner::create_private(&parent_boundary, &dir)?;
    runner::write_private(&own, &file, text)?;
    Some((own, file.as_path().to_path_buf()))
}

/// The most levels of mappings and sequences, block and flow alike, a block's YAML may nest for
/// [`same_structure`] to compare it: past it the fix is refused. The YAML cartridge's guard bounds
/// the column a block structure reaches, not how deep flow collections (`[[[...]]]`) nest, so the
/// comparison counts levels itself.
const MAX_NESTING: usize = 100;

/// Whether `before` and `after` parse, each within `limit`, to the same YAML structure: the same
/// documents, directives, mappings, sequences, keys, anchors, aliases, tags and scalars, comments
/// and layout aside. A scalar keeps whether it was plain or quoted, and a missing value, an empty
/// sequence item included, reads as `null`, so `{a}` and `{a: ''}` differ. False when either
/// doesn't parse, [`yaml_cartridge`]'s guards on the grammar refuse it, it nests more than
/// [`MAX_NESTING`] collections, or comparing it passes `limit`, so a fix it can't check is never
/// applied.
#[must_use]
pub(crate) fn same_structure(before: &str, after: &str, limit: TimeLimit) -> bool {
    match (
        shape(before, limit, MAX_NESTING),
        shape(after, limit, MAX_NESTING),
    ) {
        (Some(before), Some(after)) => before == after,
        _ => false,
    }
}

/// One piece of work in [`shape`]'s walk: a node to write, at its collection nesting depth, or
/// text to append once the nodes pushed after it are written.
enum Work<'tree> {
    /// A node, with how many collections enclose it.
    Node(Node<'tree>, usize),
    /// Text that closes or separates what came before.
    Text(&'static str),
}

/// The structure of `text` as [`same_structure`] compares it. `None` when it doesn't parse
/// cleanly within `limit`, nests more than `max_nesting` collections, or the walk passes the
/// parse's deadline.
///
/// The walk keeps its own stack, so hostile nesting fails closed instead of overflowing the
/// thread's.
fn shape(text: &str, limit: TimeLimit, max_nesting: usize) -> Option<String> {
    // The path only names the text in a parse error, which this check discards.
    let path = AbsolutePath::new(Path::new("/block.yaml"))?;
    let parsed = SourceFile::from_text(
        path,
        text.to_owned(),
        yaml_cartridge::cartridge(),
        Deadline::start(limit),
    )
    .ok()?;
    let deadline = parsed.deadline();
    let root = parsed.tree().root_node();
    if root.has_error() {
        return None;
    }
    let mut out = String::new();
    let mut stack = vec![Work::Node(root, 0)];
    while let Some(work) = stack.pop() {
        if deadline.passed() {
            return None;
        }
        match work {
            Work::Text(close) => out.push_str(close),
            Work::Node(node, depth) => {
                let pushed = write_node(node, depth, max_nesting, text, &mut out)?;
                stack.extend(pushed.into_iter().rev());
            }
        }
    }
    Some(out)
}

/// Writes the part of `node`'s structure that comes before its children to `out`, and returns
/// the work that follows, in order: its children and the text between and after them. `None`
/// when `node` is a collection past `max_nesting`.
fn write_node<'tree>(
    node: Node<'tree>,
    depth: usize,
    max_nesting: usize,
    text: &str,
    out: &mut String,
) -> Option<Vec<Work<'tree>>> {
    let source = node.utf8_text(text.as_bytes()).unwrap_or_default();
    let kind = node.kind();
    let mut cursor = node.walk();
    let children: Vec<Node<'tree>> = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .collect();
    let nests = matches!(
        kind,
        "block_mapping" | "flow_mapping" | "block_sequence" | "flow_sequence"
    );
    let inner = depth + usize::from(nests);
    if inner > max_nesting {
        return None;
    }
    let mut then = Vec::new();
    match kind {
        "comment" => {}
        "plain_scalar" => {
            let folded = fold(source);
            if matches!(folded.as_str(), "" | "~" | "null" | "Null" | "NULL") {
                out.push_str("null;");
            } else {
                let _infallible = write!(out, "p{}:{folded};", folded.len());
            }
        }
        "single_quote_scalar" => {
            let inner = source
                .get(1..source.len().saturating_sub(1))
                .unwrap_or(source);
            let folded = fold(inner).replace("''", "'");
            let _infallible = write!(out, "q{}:{folded};", folded.len());
        }
        "double_quote_scalar" => {
            // Escapes stay as written: two spellings of one value differ, which only refuses a fix.
            let inner = source
                .get(1..source.len().saturating_sub(1))
                .unwrap_or(source);
            let folded = fold(inner);
            let _infallible = write!(out, "q{}:{folded};", folded.len());
        }
        "block_scalar" => {
            let after = text.get(node.end_byte()..).unwrap_or_default();
            let body = block_body(source, after, collection_column(node));
            let _infallible = write!(out, "b{}:{body};", body.len());
        }
        "anchor" | "alias" | "tag" => {
            let _infallible = write!(out, "{kind}{}:{source};", source.len());
        }
        "yaml_directive" | "tag_directive" | "reserved_directive" => {
            let words = source.split_whitespace().collect::<Vec<_>>().join(" ");
            let _infallible = write!(out, "%{}:{words};", words.len());
        }
        "block_mapping_pair" | "flow_pair" => {
            out.push('(');
            let part = |field| {
                node.child_by_field_name(field)
                    .map_or(Work::Text("null;"), |part| Work::Node(part, inner))
            };
            then.extend([part("key"), Work::Text("="), part("value"), Work::Text(")")]);
        }
        "flow_mapping" => {
            out.push('{');
            for child in children {
                if child.kind() == "flow_node" {
                    then.extend([
                        Work::Text("("),
                        Work::Node(child, inner),
                        Work::Text("=null;)"),
                    ]);
                } else {
                    then.push(Work::Node(child, inner));
                }
            }
            then.push(Work::Text("}"));
        }
        _ => {
            let (open, close) = match kind {
                "block_mapping" => ("{", "}"),
                "block_sequence" | "flow_sequence" => ("[", "]"),
                "document" => ("<", ">"),
                _ => ("", ""),
            };
            out.push_str(open);
            if kind == "block_sequence_item" && children.is_empty() {
                out.push_str("null;");
            }
            then.extend(children.into_iter().map(|child| Work::Node(child, inner)));
            then.push(Work::Text(close));
        }
    }
    Some(then)
}

/// A flow scalar's text with its line breaks folded as YAML folds them: the blanks around each
/// break dropped, lines joined by a space, an empty line kept as a line break. Blanks that aren't
/// next to a break, such as inside a one-line quoted string, are kept.
fn fold(source: &str) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    let mut folded = String::new();
    let mut pending_space = false;
    for (index, line) in lines.into_iter().enumerate() {
        let blanks = [' ', '\t', '\r'];
        let line = if index > 0 {
            line.trim_start_matches(blanks)
        } else {
            line
        };
        let line = if index < last {
            line.trim_end_matches(blanks)
        } else {
            line
        };
        if index > 0 && line.is_empty() && index < last {
            folded.push('\n');
            pending_space = false;
            continue;
        }
        if pending_space {
            folded.push(' ');
        }
        folded.push_str(line);
        pending_space = true;
    }
    folded
}

/// The column of the innermost block mapping or sequence holding `node`, the indent a block
/// scalar's indentation indicator counts from. `None` at the top level.
fn collection_column(node: Node<'_>) -> Option<usize> {
    let mut parent = node.parent();
    while let Some(holder) = parent {
        if matches!(holder.kind(), "block_mapping" | "block_sequence") {
            return Some(holder.start_position().column);
        }
        parent = holder.parent();
    }
    None
}

/// A block scalar as [`same_structure`] compares it: its style (`|` or `>`), its content lines
/// with their indentation removed as YAML removes it, then how many line breaks end its value,
/// which is all its chomping (`+`, `-` or neither) decides.
/// With an indentation indicator (`|2`) that is exactly the indicated indent, counted from
/// `holder`, the column of the collection holding the scalar (as go-yaml counts it: the
/// indicator alone at the top level), so the spaces past it stay part of the value and a
/// renumbered indicator that keeps the value compares equal. Without one it is the indent the
/// content lines share. Trailing blank lines are part of a keep (`+`) scalar's value, so for one
/// the blank lines in `after`, the text that follows the node, are counted in too; any other
/// scalar's are dropped.
fn block_body(source: &str, after: &str, holder: Option<usize>) -> String {
    let mut lines = source.split('\n');
    let header = lines.next().unwrap_or_default();
    let indicators: String = header
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '#')
        .collect();
    let style: String = indicators
        .chars()
        .filter(|c| matches!(c, '|' | '>'))
        .collect();
    let keep = indicators.contains('+');
    let strip_chomp = indicators.contains('-');
    let explicit = indicators
        .chars()
        .find_map(|c| c.to_digit(10))
        .and_then(|digit| usize::try_from(digit).ok());
    let body: Vec<&str> = lines.map(|line| line.trim_end_matches('\r')).collect();
    let strip = explicit.map_or_else(
        || {
            body.iter()
                .filter(|line| !line.trim().is_empty())
                .map(|line| line.len() - line.trim_start_matches(' ').len())
                .min()
                .unwrap_or(0)
        },
        |indicator| holder.map_or(indicator, |column| column + indicator),
    );
    let mut out = style.clone();
    for line in body {
        out.push('\n');
        out.push_str(
            line.get(strip..)
                .unwrap_or_else(|| line.trim_start_matches(' ')),
        );
    }
    let mut out = out.trim_end().to_owned();
    let has_content = out.len() > style.len();
    // The line breaks that end the value: none when stripped, one when clipped, and for a keep
    // scalar one plus each trailing blank line, so `|+` with no blank line after it reads as `|`.
    let mut breaks = usize::from(has_content && !strip_chomp);
    if keep {
        breaks += source
            .split('\n')
            .rev()
            .take_while(|line| line.trim().is_empty())
            .count()
            .saturating_sub(1)
            + after
                .split('\n')
                .skip(1)
                .take_while(|line| line.trim().is_empty())
                .count();
    }
    let _infallible = write!(out, "/{breaks}");
    out
}

/// Whether `input`, a YAML block, holds no content yamlfmt keeps: only blank lines, document
/// markers (`---`, `...`) and directives (`%YAML`). yamlfmt prints nothing for such a block.
#[must_use]
pub(crate) fn holds_no_yaml(input: &str) -> bool {
    input.lines().all(|line| {
        let line = line.trim();
        line.is_empty()
            || line.starts_with('%')
            || ["---", "..."]
                .iter()
                .any(|marker| line.strip_prefix(marker).is_some_and(str::is_empty))
    })
}

/// The config names yamllint 1.38.0 looks for in a directory, in its own order: the first one
/// there is the one it reads.
pub(crate) const LINT_CONFIG_FILES: [&str; 3] = [".yamllint", ".yamllint.yaml", ".yamllint.yml"];

/// The rules yamllint 1.38.0 knows.
const LINT_RULES: [&str; 23] = [
    "anchors",
    "braces",
    "brackets",
    "colons",
    "commas",
    "comments",
    "comments-indentation",
    "document-end",
    "document-start",
    "empty-lines",
    "empty-values",
    "float-values",
    "hyphens",
    "indentation",
    "key-duplicates",
    "key-ordering",
    "line-length",
    "new-line-at-end-of-file",
    "new-lines",
    "octal-values",
    "quoted-strings",
    "trailing-spaces",
    "truthy",
];

/// The configs yamllint has built in, the only ones a config may `extends`: yamllint reads any
/// other value as a file's path.
const BUILT_IN: [&str; 2] = ["default", "relaxed"];

/// The rules yamllint 1.38.0's built-in `default` config turns on.
const DEFAULT_ON: [&str; 17] = [
    "anchors",
    "braces",
    "brackets",
    "colons",
    "commas",
    "comments",
    "comments-indentation",
    "document-start",
    "empty-lines",
    "hyphens",
    "indentation",
    "key-duplicates",
    "line-length",
    "new-line-at-end-of-file",
    "new-lines",
    "trailing-spaces",
    "truthy",
];

/// The rules yamllint 1.38.0's built-in `relaxed` config turns off of [`DEFAULT_ON`].
const RELAXED_OFF: [&str; 4] = [
    "comments",
    "comments-indentation",
    "document-start",
    "truthy",
];

/// The rule yamllint leaves off for a block, which a snippet breaks by being part of a file: a
/// block is never a whole file, so it needn't open with `---`.
pub(crate) const LINT_RELAXED: &str = "document-start";

/// The config yamllint is given when the project has none: its defaults.
const LINT_DEFAULT: &str = "extends: default\n";

/// What yamllint is given for the blocks of one doc directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lint {
    /// `-c <private file>`.
    pub(crate) flags: Vec<OsString>,
    /// The empty private directory yamllint runs in, beside its config.
    pub(crate) cwd: PathBuf,
    /// The project's config relative to the root, as named to the user; empty when there is
    /// none and yamllint gets its defaults.
    pub(crate) file: String,
    /// The rules the copy leaves on for a block (see [`switches_lint_off`]).
    pub(crate) enabled: Vec<&'static str>,
    /// The copy's `locale`, as the project wrote it.
    pub(crate) locale: Option<String>,
}

/// What yamllint is given for the blocks of `doc`: `-c <private file>`, holding the project's
/// config as supdoc read it (see [`lint_config`]), found walking up from `doc`'s directory to
/// `root`, or yamllint's defaults when there is none, with [`LINT_RELAXED`] off either way; and an
/// empty private directory to run in. With `-c`, yamllint reads no other config: none in the
/// directory it runs in or above it, and none through `YAMLLINT_CONFIG_FILE` or
/// `XDG_CONFIG_HOME`.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directories and the config file in them.
///
/// # Errors
///
/// [`Refusal::Config`] when the config is a link out of the root, isn't a readable UTF-8 file of
/// at most 256 KiB, or says something [`lint_config`] refuses; [`Refusal::NoPrivateDir`]
/// when there is nowhere private to put it or to run yamllint.
pub(crate) fn lint_args(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Lint, Refusal> {
    let (text, file) = match found_lint_config(root, doc) {
        Some(found) => {
            let file = relative(root, &found);
            let refuse = |why: String| Refusal::Config {
                file: file.clone(),
                why,
            };
            let written = read_text(root, &found).map_err(|why| refuse(why.to_owned()))?;
            (lint_config(&written).map_err(refuse)?, file)
        }
        None => (
            lint_config(LINT_DEFAULT).map_err(|why| Refusal::Config {
                file: String::new(),
                why,
            })?,
            String::new(),
        ),
    };
    let (dir, private) = private_file(env, root, ("yamllint-config", "config.yaml"), &text.text)
        .ok_or(Refusal::NoPrivateDir)?;
    let cwd = AbsolutePath::new(&dir.dir().join("cwd"))
        .and_then(|cwd| empty_private_dir(&dir, &cwd))
        .ok_or(Refusal::NoPrivateDir)?;
    Ok(Lint {
        flags: vec![OsString::from("-c"), private.into_os_string()],
        cwd,
        file,
        enabled: text.enabled,
        locale: text.locale,
    })
}

/// `dir`, created mode 0700 when missing and admitted beneath `parent`, when it is a directory
/// only its owner reaches and holds nothing. `None` otherwise.
///
/// Effect: may create the directory.
fn empty_private_dir(parent: &Boundary, dir: &AbsolutePath) -> Option<PathBuf> {
    let _dir = runner::create_private(parent, dir)?;
    let empty = fs::read_dir(dir.as_path()).ok()?.next().is_none();
    empty.then(|| dir.as_path().to_path_buf())
}

/// The config yamllint would read for `doc` were it run in `doc`'s directory: the first of
/// [`LINT_CONFIG_FILES`] in the nearest directory at or above it, never above `root`, that holds
/// any.
///
/// Effect: reads file metadata.
fn found_lint_config(root: &Path, doc: &Path) -> Option<PathBuf> {
    doc.parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
        .find_map(|dir| {
            LINT_CONFIG_FILES
                .iter()
                .map(|name| dir.join(name))
                .find(|candidate| fs::symlink_metadata(candidate).is_ok())
        })
}

/// One line of a config that says something: its 1-based number, its indent and the rest.
#[derive(Clone, Copy)]
struct Line<'text> {
    /// Its 1-based number.
    number: usize,
    /// How many spaces it starts with.
    indent: usize,
    /// The rest of it.
    body: &'text str,
}

/// An option's value in a yamllint config: a scalar, or a list of them.
enum OptionValue {
    /// One scalar, and whether it was quoted.
    Scalar(String, bool),
    /// A list of scalars, each with whether it was quoted.
    List(Vec<(String, bool)>),
}

/// A rule's setting in a yamllint config.
enum RuleValue {
    /// `enable` or `disable`.
    Switch(&'static str),
    /// Its options, in order.
    Options(Vec<(String, OptionValue)>),
}

/// A yamllint config as supdoc gives it to yamllint.
struct LintConfig {
    /// The copy's text (see [`lint_config`]).
    text: String,
    /// The rules it leaves on for a block, in [`LINT_RULES`] order.
    enabled: Vec<&'static str>,
    /// Its `locale`, as written.
    locale: Option<String>,
}

/// The yamllint config `text` as supdoc gives it to yamllint, with the rules it leaves on and its
/// `locale`. The copy holds its `extends`, `locale` and `rules`, each checked and written again
/// (see [`written`]), with [`LINT_RELAXED`] set to `disable` last. Its `yaml-files`, which only
/// choose the files yamllint walks to and never apply to text on stdin, are dropped.
///
/// supdoc reads only the YAML a yamllint config needs: one document of `key: value` lines and a
/// `rules:` block, each rule `enable`, `disable` or a block of `option: value` lines, a value a
/// scalar, a flow list of scalars, or a block list of them; with comments and blank lines. `\r\n`
/// line ends are read as `\n`, and a leading byte order mark is dropped.
///
/// # Errors
///
/// Why the config can't be given to yamllint: a character yamllint reads other than as supdoc does
/// (see [`unreadable_char`]), YAML outside that shape (a second document included), a key or rule
/// supdoc doesn't know yamllint 1.38.0 to read, a key or rule written twice, an `extends` naming a
/// file rather than a built-in config, an `ignore` or `ignore-from-file`, top-level or a rule's,
/// which skip the files the checkout chooses or read a file it names, no setting at all, or no
/// rule left on.
fn lint_config(text: &str) -> Result<LintConfig, String> {
    let outside = |line: usize| {
        format!(
            "has YAML on line {line} that supdoc doesn't read in a yamllint config, which it reads \
             as `extends`, `locale` and a `rules:` block of rules, each `enable`, `disable` or a \
             block of `option: value` lines"
        )
    };
    let text = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n");
    if let Some((line, c)) = unreadable_char(&text) {
        return Err(format!(
            "holds U+{:04X} on line {line}, a character yamllint reads as a line break or refuses; \
             supdoc reads none in a yamllint config",
            u32::from(c)
        ));
    }
    let lines = lint_lines(&text).map_err(outside)?;
    if lines.is_empty() {
        return Err("is empty, which yamllint refuses as a config that isn't a mapping".to_owned());
    }
    let mut extends: Option<String> = None;
    let mut locale: Option<(String, bool)> = None;
    let mut rules: Vec<(String, RuleValue)> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut at = 0;
    while let Some(line) = lines.get(at) {
        at += 1;
        if line.indent != 0 {
            return Err(outside(line.number));
        }
        let (key, rest) = lint_key(line.body).ok_or_else(|| outside(line.number))?;
        if seen.contains(&key) {
            return Err(format!("sets `{key}` twice"));
        }
        seen.push(key);
        let block_end = |from: usize| {
            from + lines
                .get(from..)
                .unwrap_or_default()
                .iter()
                .take_while(|next| next.indent > 0 || is_item(next.body))
                .count()
        };
        match key {
            "yaml-files" => at = block_end(at),
            "ignore" | "ignore-from-file" => {
                return Err(format!(
                    "sets `{key}`, which skips files the checkout chooses or reads a file it names"
                ));
            }
            "extends" => {
                let (value, _) = scalar(rest).ok_or_else(|| outside(line.number))?;
                if !BUILT_IN.contains(&value.as_str()) {
                    return Err(format!(
                        "extends `{value}`, a file yamllint would read, not one of its built-in \
                         configs `default` and `relaxed`"
                    ));
                }
                extends = Some(value);
            }
            "locale" => locale = Some(scalar(rest).ok_or_else(|| outside(line.number))?),
            "rules" => {
                if !is_blank(rest) {
                    return Err(outside(line.number));
                }
                let end = lines
                    .get(at..)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|next| next.indent > 0)
                    .count()
                    + at;
                rules = lint_rules(lines.get(at..end).unwrap_or_default(), &outside)?;
                at = end;
            }
            other => {
                return Err(format!(
                    "sets `{other}`, which supdoc doesn't know yamllint 1.38.0 to read"
                ));
            }
        }
    }
    let enabled = enabled_rules(extends.as_deref(), &rules);
    if enabled.is_empty() {
        return Err(
            "leaves no yamllint rule on for a block, which switches its lint step off".to_owned(),
        );
    }
    Ok(LintConfig {
        text: write_lint_config(extends.as_deref(), locale.as_ref(), &rules),
        enabled,
        locale: locale.map(|(value, _)| value),
    })
}

/// The first character of `text`, with its 1-based line, that yamllint would read other than as
/// supdoc reads it, or refuse: a C0 or C1 control other than tab and line feed (a carriage return
/// included, once `\r\n` line ends are read as `\n`), the line breaks U+2028 and U+2029, a byte
/// order mark past the start, and the non-characters U+FFFE and U+FFFF.
fn unreadable_char(text: &str) -> Option<(usize, char)> {
    text.split('\n').enumerate().find_map(|(at, line)| {
        line.chars()
            .find(|&c| {
                (c.is_control() && c != '\t')
                    || matches!(
                        c,
                        '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}'
                    )
            })
            .map(|c| (at + 1, c))
    })
}

/// The rules a config that `extends` and sets `rules` leaves on once [`LINT_RELAXED`] is off,
/// in [`LINT_RULES`] order: each rule `extends` turns on, unless `rules` disables it, and each one
/// `rules` enables or gives options, which turns it on.
fn enabled_rules(extends: Option<&str>, rules: &[(String, RuleValue)]) -> Vec<&'static str> {
    let base: Vec<&str> = match extends {
        Some("default") => DEFAULT_ON.to_vec(),
        Some("relaxed") => DEFAULT_ON
            .into_iter()
            .filter(|rule| !RELAXED_OFF.contains(rule))
            .collect(),
        _ => Vec::new(),
    };
    LINT_RULES
        .into_iter()
        .filter(|rule| *rule != LINT_RELAXED)
        .filter(|rule| match rules.iter().find(|(set, _)| set == rule) {
            Some((_, RuleValue::Switch(switch))) => *switch == "enable",
            Some((_, RuleValue::Options(_))) => true,
            None => base.contains(rule),
        })
        .collect()
}

/// The lines of `text` that say something, each with its number and indent, after one leading
/// `---`.
///
/// # Errors
///
/// The number of a line indented with a tab, or of a document marker past the first line that
/// says something, which starts a document yamllint wouldn't read.
fn lint_lines(text: &str) -> Result<Vec<Line<'_>>, usize> {
    let mut lines = Vec::new();
    for (at, raw) in text.split('\n').enumerate() {
        let number = at + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if is_blank(line) {
            continue;
        }
        let body = line.trim_start_matches(' ');
        if body.starts_with('\t') {
            return Err(number);
        }
        let marker = body.starts_with("---") || body.starts_with("...");
        if marker && line.len() == body.len() {
            if lines.is_empty() && body.trim_end_matches([' ', '\t']) == "---" {
                continue;
            }
            return Err(number);
        }
        lines.push(Line {
            number,
            indent: line.len() - body.len(),
            body,
        });
    }
    Ok(lines)
}

/// `line`'s key, lowercase letters, digits and `-`, and what follows its `:`.
fn lint_key(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    let named = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    (named && (rest.is_empty() || rest.starts_with(' '))).then_some((key, rest))
}

/// Whether `body` is a block list's item: `-` then a space or nothing.
fn is_item(body: &str) -> bool {
    body == "-" || body.starts_with("- ")
}

/// The rules of a `rules:` block, its lines `block`.
///
/// # Errors
///
/// `outside` of the first line outside the block's shape, or what else is wrong.
fn lint_rules(
    block: &[Line<'_>],
    outside: &impl Fn(usize) -> String,
) -> Result<Vec<(String, RuleValue)>, String> {
    let Some(indent) = block.first().map(|line| line.indent) else {
        return Ok(Vec::new());
    };
    let mut rules: Vec<(String, RuleValue)> = Vec::new();
    let mut at = 0;
    while let Some(line) = block.get(at) {
        at += 1;
        if line.indent != indent {
            return Err(outside(line.number));
        }
        let (rule, rest) = lint_key(line.body).ok_or_else(|| outside(line.number))?;
        if !LINT_RULES.contains(&rule) {
            return Err(format!(
                "sets rule `{rule}`, which supdoc doesn't know yamllint 1.38.0 to have"
            ));
        }
        if rules.iter().any(|(known, _)| known == rule) {
            return Err(format!("sets rule `{rule}` twice"));
        }
        let value = if is_blank(rest) {
            let end = at
                + block
                    .get(at..)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|next| next.indent > indent)
                    .count();
            let options = block.get(at..end).unwrap_or_default();
            if options.is_empty() {
                return Err(outside(line.number));
            }
            at = end;
            RuleValue::Options(lint_options(rule, options, outside)?)
        } else {
            match scalar(rest).ok_or_else(|| outside(line.number))?.0.as_str() {
                "enable" => RuleValue::Switch("enable"),
                "disable" => RuleValue::Switch("disable"),
                _ => return Err(outside(line.number)),
            }
        };
        rules.push((rule.to_owned(), value));
    }
    Ok(rules)
}

/// The options of rule `rule`, its lines `block`.
///
/// # Errors
///
/// `outside` of the first line outside the block's shape, an option written twice, or an
/// `ignore` or `ignore-from-file`.
fn lint_options(
    rule: &str,
    block: &[Line<'_>],
    outside: &impl Fn(usize) -> String,
) -> Result<Vec<(String, OptionValue)>, String> {
    let indent = block.first().map_or(0, |line| line.indent);
    let mut options: Vec<(String, OptionValue)> = Vec::new();
    let mut at = 0;
    while let Some(line) = block.get(at) {
        at += 1;
        if line.indent != indent {
            return Err(outside(line.number));
        }
        let (option, rest) = lint_key(line.body).ok_or_else(|| outside(line.number))?;
        if matches!(option, "ignore" | "ignore-from-file") {
            return Err(format!(
                "sets `rules.{rule}.{option}`, which skips files the checkout chooses or reads a \
                 file it names"
            ));
        }
        if options.iter().any(|(known, _)| known == option) {
            return Err(format!("sets `rules.{rule}.{option}` twice"));
        }
        let value = if is_blank(rest) {
            let items: Vec<&Line<'_>> = block
                .get(at..)
                .unwrap_or_default()
                .iter()
                .take_while(|next| {
                    next.indent > indent || (next.indent == indent && is_item(next.body))
                })
                .collect();
            let Some(first) = items.first() else {
                return Err(outside(line.number));
            };
            let mut list = Vec::new();
            for item in &items {
                let value = item
                    .body
                    .strip_prefix('-')
                    .filter(|_| item.indent == first.indent)
                    .and_then(|rest| scalar(rest).filter(|_| rest.starts_with(' ')))
                    .filter(|(value, quoted)| *quoted || plain_item(value))
                    .ok_or_else(|| outside(item.number))?;
                list.push(value);
            }
            at += items.len();
            OptionValue::List(list)
        } else if rest.trim_start().starts_with('[') {
            OptionValue::List(flow_list(rest).ok_or_else(|| outside(line.number))?)
        } else {
            let (value, quoted) = scalar(rest)
                .filter(|(value, quoted)| *quoted || plain_item(value))
                .ok_or_else(|| outside(line.number))?;
            OptionValue::Scalar(value, quoted)
        };
        options.push((option.to_owned(), value));
    }
    Ok(options)
}

/// Whether `value`, a plain scalar, reads the same written back in a block or a flow: none of
/// the flow indicators or a trailing `:` in it.
fn plain_item(value: &str) -> bool {
    !value.contains(['[', ']', '{', '}', ',']) && !value.ends_with(':')
}

/// The scalars of the flow list `rest` writes after a key's `:`, each with whether it was quoted,
/// a trailing comment dropped: `[a, 'b', "c"]`. `None` for anything else, a nested collection
/// included.
fn flow_list(rest: &str) -> Option<Vec<(String, bool)>> {
    let mut inner = rest.trim_start_matches([' ', '\t']).strip_prefix('[')?;
    let mut items = Vec::new();
    loop {
        inner = inner.trim_start_matches([' ', '\t']);
        if let Some(after) = inner.strip_prefix(']').filter(|_| items.is_empty()) {
            return is_blank(after).then_some(items);
        }
        let end = match inner.chars().next()? {
            quote @ ('"' | '\'') => inner.get(1..)?.find(quote)? + 2,
            _ => inner.find([',', ']'])?,
        };
        let (item, after) = (inner.get(..end)?, inner.get(end..)?);
        let (value, quoted) = scalar(item)?;
        if !quoted && !plain_item(&value) {
            return None;
        }
        items.push((value, quoted));
        let after = after.trim_start_matches([' ', '\t']);
        if let Some(next) = after.strip_prefix(',') {
            inner = next;
        } else {
            return is_blank(after.strip_prefix(']')?).then_some(items);
        }
    }
}

/// `value` as supdoc writes it: a plain one made only of ASCII letters, digits and `_`, `.`, `+`
/// and `-` (a word, a number or a boolean) as written, so it keeps its type; any other in double
/// quotes, every character outside printable ASCII escaped, so the copy holds no line break or
/// key that supdoc didn't read.
fn written((value, quoted): (&str, bool)) -> String {
    let token = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'));
    if token && !quoted {
        return value.to_owned();
    }
    let mut out = String::from('"');
    for c in value.chars() {
        let _infallible = match c {
            '"' => write!(out, "\\\""),
            '\\' => write!(out, "\\\\"),
            ' '..='~' => write!(out, "{c}"),
            c if u32::from(c) <= 0xffff => write!(out, "\\u{:04x}", u32::from(c)),
            c => write!(out, "\\U{:08x}", u32::from(c)),
        };
    }
    out.push('"');
    out
}

/// The config [`lint_config`] read, written again.
fn write_lint_config(
    extends: Option<&str>,
    locale: Option<&(String, bool)>,
    rules: &[(String, RuleValue)],
) -> String {
    let mut out = String::new();
    if let Some(extends) = extends {
        let _infallible = writeln!(out, "extends: {extends}");
    }
    if let Some((value, quoted)) = locale {
        let _infallible = writeln!(out, "locale: {}", written((value, *quoted)));
    }
    out.push_str("rules:\n");
    for (rule, value) in rules.iter().filter(|(rule, _)| rule != LINT_RELAXED) {
        match value {
            RuleValue::Switch(switch) => {
                let _infallible = writeln!(out, "  {rule}: {switch}");
            }
            RuleValue::Options(options) => {
                let _infallible = writeln!(out, "  {rule}:");
                for (option, value) in options {
                    match value {
                        OptionValue::Scalar(value, quoted) => {
                            let _infallible =
                                writeln!(out, "    {option}: {}", written((value, *quoted)));
                        }
                        OptionValue::List(items) if items.is_empty() => {
                            let _infallible = writeln!(out, "    {option}: []");
                        }
                        OptionValue::List(items) => {
                            let _infallible = writeln!(out, "    {option}:");
                            for (value, quoted) in items {
                                let _infallible =
                                    writeln!(out, "      - {}", written((value, *quoted)));
                            }
                        }
                    }
                }
            }
        }
    }
    let _infallible = writeln!(out, "  {LINT_RELAXED}: disable");
    out
}

/// Whether `block` holds yamllint comments that switch every rule off for the rest of the
/// block, a line or the whole block: `# yamllint disable`, `disable-line` or `disable-file` naming
/// no rule, or such comments naming between them every rule in `enabled`, the rules the config
/// leaves on (yamllint's comments only count those). supdoc's own `lint(off: ...)` is the way to
/// switch the step off.
///
/// The scan is conservative, so a near miss counts as switching the step off, never as a pass: it
/// reads text, not YAML, so a comment inside a string or a multi-line scalar counts, as does one
/// an `enable` comment undoes, and `disable-file` counts on any line, not only the first; it splits
/// words on every character Python's `str.isspace` takes as whitespace (U+001C to U+001F
/// included), and finds a comment wherever a `#` starts it.
#[must_use]
pub(crate) fn switches_lint_off(block: &str, enabled: &[&str]) -> bool {
    let mut named: Vec<&str> = Vec::new();
    for line in block.split('\n') {
        for (at, _) in line.match_indices('#') {
            let words: Vec<&str> = line
                .get(at + 1..)
                .unwrap_or_default()
                .split(python_space)
                .filter(|word| !word.is_empty())
                .collect();
            match words.as_slice() {
                ["yamllint", "disable" | "disable-line" | "disable-file"] => return true,
                [
                    "yamllint",
                    "disable" | "disable-line" | "disable-file",
                    rules @ ..,
                ] => {
                    named.extend(rules.iter().filter_map(|rule| rule.strip_prefix("rule:")));
                }
                _ => {}
            }
        }
    }
    enabled.iter().all(|rule| named.contains(rule))
}

/// Whether Python's `str.isspace` takes `c` as whitespace: Unicode's white space, and the
/// separators U+001C to U+001F.
fn python_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// The first control character in `block` that YAML doesn't allow, other than tab, line feed and
/// carriage return, with its 0-based line: a NUL included, which yamllint would read as a sign of
/// UTF-16 or UTF-32 text. `None` when there is none.
#[must_use]
pub(crate) fn forbidden_char(block: &str) -> Option<(usize, char)> {
    block.split('\n').enumerate().find_map(|(at, line)| {
        line.chars()
            .find(|&c| c.is_control() && !matches!(c, '\t' | '\r'))
            .map(|c| (at, c))
    })
}

/// What `yamllint --version` prints for the yamllint supdoc's checks are verified against.
pub(crate) const VERIFIED: &str = "yamllint 1.38.0";

/// How a yamllint whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// What `program`, a yamllint, printed for `--version`, run in `cwd` as any tool is (see
/// [`runner::run`]) under `timeout`, when it is [`VERIFIED`].
///
/// Effect: runs the binary.
///
/// # Errors
///
/// How to name what it printed to the user: its first line, controls dropped and cut at 60
/// characters, in backticks; or that it couldn't be read, when the run failed or printed nothing.
pub(crate) fn version_of(
    program: &Path,
    env: &ToolEnv,
    (root, cwd): (&Path, &Path),
    timeout: Option<std::time::Duration>,
) -> Result<(), String> {
    let call = runner::Call {
        program,
        name: "yamllint",
        args: &[std::ffi::OsStr::new("--version")],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Private(cwd),
        cap: runner::CAP,
    };
    let ran = match runner::run(&call, env, root) {
        Ok(ran) if ran.success => ran,
        _ => return Err(UNREADABLE.to_owned()),
    };
    let text = String::from_utf8_lossy(&ran.stdout);
    let first = text.lines().next().unwrap_or_default().trim();
    if first == VERIFIED {
        return Ok(());
    }
    let shown: String = first.chars().filter(|c| !c.is_control()).take(60).collect();
    if shown.is_empty() {
        Err(UNREADABLE.to_owned())
    } else {
        Err(format!("`{}`", shown.replace('`', "'")))
    }
}

/// Whether a yamllint run that failed with `stderr` rejected the config supdoc gave it: it says
/// `invalid config`, which begins every config error yamllint 1.38.0 reports.
#[must_use]
pub(crate) fn rejected_config(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains("invalid config")
}

/// One diagnostic of `yamllint -f parsable`: `stdin:<line>:<column>: [<level>] <message>
/// (<rule>)`, a syntax error's rule `syntax` and its message without `syntax error: `. `None`
/// for any other line.
fn parsable(line: &str) -> Option<Diagnostic> {
    let rest = line.strip_prefix("stdin:")?;
    let (number, rest) = rest.split_once(':')?;
    let (column, rest) = rest.split_once(": [")?;
    let (_level, said) = rest.split_once("] ")?;
    let (message, rule) = said.strip_suffix(')')?.rsplit_once(" (")?;
    let named = !rule.is_empty() && rule.bytes().all(|b| b.is_ascii_lowercase() || b == b'-');
    let line = number.parse::<usize>().ok()?;
    let column = column.parse::<usize>().ok()?;
    let syntax = rule == "syntax";
    let message = if syntax {
        message.strip_prefix("syntax error: ").unwrap_or(message)
    } else {
        message
    };
    named.then(|| Diagnostic {
        line,
        column,
        end_line: line,
        syntax,
        rule: rule.to_owned(),
        message: message.to_owned(),
    })
}

/// The diagnostics in `yamllint -f parsable`'s `stdout`, one per line. `None` when any line isn't
/// one.
#[must_use]
pub(crate) fn lint_diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    std::str::from_utf8(stdout)
        .ok()?
        .lines()
        .filter(|line| !line.is_empty())
        .map(parsable)
        .collect()
}

/// Judges a `yamllint` run as [`lint::judge`] does, reading its stdout as `-f parsable` lines,
/// except that a syntax error is a parse failure at its line, as the format step reports one, and
/// a failure that is a Python traceback is a tool failure in supdoc's words (see
/// [`internal_error`]), never the traceback. yamllint checks no fragments, so no run is framed.
#[must_use]
pub(crate) fn lint_verdict(
    _input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    let syntax = result
        .as_ref()
        .ok()
        .and_then(|ran| lint_diagnostics(&ran.stdout))
        .and_then(|found| found.into_iter().find(|found| found.syntax));
    if let Some(syntax) = syntax {
        return lint::Verdict::Failed(format::Verdict::ParseFailed {
            detail: syntax.message,
            line: syntax.line.checked_sub(1),
        });
    }
    let crashed = result.as_ref().ok().and_then(|ran| {
        let reported = lint_diagnostics(&ran.stdout).is_some_and(|found| !found.is_empty());
        (!ran.success && !reported)
            .then(|| internal_error(&ran.stderr))
            .flatten()
    });
    match crashed {
        Some(said) => lint::Verdict::Failed(format::Verdict::ToolFailed(said)),
        None => lint::judge(result, lint_diagnostics),
    }
}

/// What Python prints before a traceback.
const TRACEBACK: &str = "Traceback (most recent call last):";

/// The most characters of a traceback's last line supdoc shows.
const MAX_SHOWN: usize = 200;

/// The tool failure supdoc reports for a yamllint whose `stderr` is a Python traceback: "yamllint
/// stopped with an internal error", then the traceback's last line, the exception, with controls
/// dropped, every path replaced by `<path>` and cut at 200 characters. `None` for any other
/// stderr.
#[must_use]
pub(crate) fn internal_error(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    if !text.contains(TRACEBACK) {
        return None;
    }
    let last = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let shown: String = without_paths(last)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_SHOWN)
        .collect();
    Some(format!("yamllint stopped with an internal error: {shown}"))
}

/// `line` with each path in it, a run from a `/` or `~` that starts a word or follows a quote
/// to the next space, quote, comma or parenthesis, replaced by `<path>`.
fn without_paths(line: &str) -> String {
    let ends = |c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ',' | ')' | '(');
    let mut out = String::new();
    let mut in_path = false;
    let mut previous = ' ';
    for c in line.chars() {
        if in_path {
            if !ends(c) {
                continue;
            }
            in_path = false;
        } else if matches!(c, '/' | '~') && (ends(previous) || matches!(previous, ':' | '=')) {
            in_path = true;
            out.push_str("<path>");
            previous = c;
            continue;
        }
        out.push(c);
        previous = c;
    }
    out
}

/// Whether a yamllint run that failed with `stderr` stopped on the `locale` its config sets: a
/// Python traceback ending in `locale.Error`, which yamllint 1.38.0 raises loading a config whose
/// locale the host lacks, before it reads the block.
#[must_use]
pub(crate) fn locale_failed(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr);
    text.contains(TRACEBACK)
        && text
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .is_some_and(|last| last.starts_with("locale.Error"))
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIG_FILES, LINT_CONFIG_FILES, LINT_RULES, MAX_NESTING, Refusal, forbidden_char,
        holds_no_yaml, internal_error, lint_args, lint_config, lint_diagnostics, lint_verdict,
        locale_failed, parse_error, project_args, read_config, rejected_config, same_structure,
        shape, switches_lint_off,
    };
    use crate::code_blocks::format::{self, ParseError};
    use crate::code_blocks::lint::{self, Diagnostic};
    use crate::code_blocks::runner::Ran;
    use crate::code_blocks::tools::ToolEnv;
    use crate::limits::TimeLimit;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn limit() -> TimeLimit {
        "10s".parse().expect("10s is a valid limit")
    }

    /// A `ToolEnv` whose cache home is `cache`.
    fn env_in(cache: &Path) -> ToolEnv {
        ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| cache.as_os_str().to_owned()))
    }

    /// The private config path in `-conf <path>`.
    fn conf_of(flags: &[std::ffi::OsString]) -> PathBuf {
        let [flag, path] = flags else {
            panic!("expected `-conf <path>`, got {flags:?}");
        };
        assert_eq!(flag, "-conf", "the first flag should be -conf");
        PathBuf::from(path)
    }

    #[test]
    fn a_yaml_line_error_names_its_line_zero_based() {
        assert_eq!(
            parse_error("yaml: line 2: did not find expected '-' indicator\n"),
            Some(ParseError { line: Some(1) })
        );
        assert_eq!(parse_error("error: something else\n"), None);
        assert_eq!(parse_error("yaml: did not find expected node\n"), None);
    }

    #[test]
    fn a_config_keeps_its_formatter_and_line_ending_and_drops_file_selection() {
        let written = read_config(
            "---\n# style\nline_ending: lf\ninclude:\n  - a/**\nexclude: [b, c]\n\
             regex_exclude:\n- '.*'\ngitignore_excludes: true\ngitignore_path: ../../outside\n\
             formatter:\n  type: basic # the default\n  indent: 4\n  retain_line_breaks: True\n  \
             force_quote_style: ''\n",
        );

        assert_eq!(
            written.as_deref(),
            Ok(
                "line_ending: \"lf\"\nformatter:\n  type: \"basic\"\n  indent: 4\n  \
                retain_line_breaks: true\n  force_quote_style: \"\"\n"
            )
        );
    }

    #[test]
    fn an_empty_config_leaves_yamlfmts_defaults() {
        assert_eq!(read_config("").as_deref(), Ok(""));
        assert_eq!(read_config("# nothing\n---\n").as_deref(), Ok(""));
    }

    #[test]
    fn unknown_keys_wrong_values_and_yaml_outside_the_shape_are_refused() {
        for (config, says) in [
            ("bogus: 1\n", "sets `bogus`"),
            ("formatter:\n  bogus: 1\n", "sets `formatter.bogus`"),
            ("formatter:\n  indent: two\n", "not a count"),
            ("formatter:\n  indent: 010\n", "plain decimal"),
            ("formatter:\n  max_line_length: 0x50\n", "plain decimal"),
            ("formatter:\n  pad_line_comments: +1\n", "plain decimal"),
            ("formatter:\n  array_indent: 2e0\n", "plain decimal"),
            ("formatter:\n  indent: 00\n", "plain decimal"),
            ("include:\n- a\n---\nformatter:\n  indent: 4\n", "line 3"),
            ("exclude: [a]\n...\n", "line 2"),
            ("formatter:\n  eof_newline: yes\n", "not `true` or `false`"),
            ("formatter:\n  type: fancy\n", "not one of"),
            (
                "formatter:\n  type: kyaml\n  indent: 2\n",
                "beside `type: kyaml`",
            ),
            ("line_ending: lf\nline_ending: crlf\n", "twice"),
            ("formatter: {type: basic}\n", "line 1"),
            ("formatter:\n  type: &a basic\n", "line 2"),
            ("formatter:\n  type: basic\n   indent: 2\n", "line 3"),
            ("line_ending: \"l\\u0066\"\n", "line 1"),
            ("---\na\n", "line 2"),
            ("---\nline_ending: lf\n---\n", "line 3"),
        ] {
            let refused = read_config(config).expect_err(config);
            assert!(refused.contains(says), "{config:?} gave {refused:?}");
        }
    }

    #[test]
    fn the_nearest_config_inside_the_root_is_given_as_a_private_copy() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(root.path().join(".yamlfmt"), "bogus: 1\n").expect("root config");
        fs::write(
            root.path().join("docs/yamlfmt.yml"),
            "exclude: ['**']\nformatter:\n  indent: 4\n",
        )
        .expect("nearer config");

        let flags = project_args(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("docs/deep/a.md"),
        )
        .expect("the nearer config should be accepted");
        let conf = conf_of(&flags);

        assert!(conf.starts_with(cache.path()), "{conf:?} should be private");
        assert_eq!(
            fs::read_to_string(&conf).expect("the private copy"),
            "formatter:\n  indent: 4\n"
        );
    }

    #[test]
    fn no_config_gives_an_empty_private_copy_and_the_root_config_applies_to_the_whole_tree() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let env = env_in(cache.path());
        let doc = root.path().join("docs/a.md");

        let none = project_args(&env, root.path(), &doc).expect("no config is fine");
        assert_eq!(fs::read_to_string(conf_of(&none)).expect("the copy"), "");

        fs::write(root.path().join(".yamlfmt"), "bogus: 1\n").expect("root config");
        assert!(matches!(
            project_args(&env, root.path(), &doc),
            Err(Refusal::Config { file, .. }) if file == ".yamlfmt"
        ));
    }

    #[test]
    fn two_configs_in_one_directory_are_refused_since_yamlfmt_picks_one_at_random() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        for name in [CONFIG_FILES[0], CONFIG_FILES[4]] {
            fs::write(root.path().join(name), "").expect("a config");
        }

        let refused = project_args(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("a.md"),
        );

        assert!(
            matches!(&refused, Err(Refusal::Config { why, .. }) if why.contains("at random")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_config_link_out_of_the_root_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let outside = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(outside.path().join("c.yaml"), "").expect("outside config");
        crate::cli::testing::symlink(outside.path().join("c.yaml"), root.path().join(".yamlfmt"))
            .expect("the link");

        let refused = project_args(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("a.md"),
        );

        assert!(
            matches!(&refused, Err(Refusal::Config { why, .. }) if why.contains("outside")),
            "{refused:?}"
        );
    }

    #[test]
    fn layout_changes_keep_the_structure_but_new_values_do_not() {
        assert!(same_structure(
            "a:\n    b: 1 # c\n    c: [x,   y]\n",
            "a:\n  b: 1 # c\n  c: [x, y]\n",
            limit()
        ));
        assert!(same_structure(
            "k: |\n      one\n      two\n",
            "k: |\n  one\n  two\n",
            limit()
        ));
        assert!(same_structure("a: 'x'\n", "a: \"x\"\n", limit()));
        assert!(!same_structure("a: ' x'\n", "a: 'x'\n", limit()));
        assert!(same_structure("a: one\n  two\n", "a: one two\n", limit()));
        assert!(!same_structure(
            "{<n>, ...}\n",
            "{<n>: '', '...': ''}\n",
            limit()
        ));
        assert!(!same_structure("a: 1\n", "a: 2\n", limit()));
        assert!(!same_structure("a: [1\n", "a: [1]\n", limit()));
    }

    #[test]
    fn a_zero_count_is_kept() {
        assert_eq!(
            read_config("formatter:\n  max_line_length: 0\n").as_deref(),
            Ok("formatter:\n  max_line_length: 0\n")
        );
    }

    /// `depth` nested flow sequences (`[[...1...]]`) or mappings (`{a: {a: ...1...}}`).
    fn nested(depth: usize, mapping: bool) -> String {
        if mapping {
            format!("{}1{}\n", "{a: ".repeat(depth), "}".repeat(depth))
        } else {
            format!("a: {}1{}\n", "[ ".repeat(depth), " ]".repeat(depth))
        }
    }

    #[test]
    fn nesting_past_the_bound_is_never_the_same_structure_and_never_recurses() {
        for depth in [MAX_NESTING + 1, 5_000, 100_000] {
            for mapping in [false, true] {
                let deep = nested(depth, mapping);
                assert!(
                    !same_structure(&deep, "a: 1\n", limit()),
                    "{depth} levels (mapping: {mapping}) should be refused"
                );
            }
        }
        let within = nested(MAX_NESTING - 1, false);
        assert!(same_structure(&within, &within, limit()));
    }

    #[test]
    fn a_dropped_directive_changes_the_structure() {
        assert!(!same_structure(
            "%YAML 1.1\n---\na: yes\n",
            "a: yes\n",
            limit()
        ));
        assert!(same_structure(
            "%YAML   1.1\n---\na: yes\n",
            "%YAML 1.1\n---\na:   yes\n",
            limit()
        ));
    }

    #[test]
    fn an_empty_sequence_item_is_null_and_a_keep_scalars_trailing_lines_count() {
        assert!(!same_structure("-\n- a\n", "- a\n", limit()));
        assert!(!same_structure("-\n-\n", "[]\n", limit()));
        assert!(!same_structure(
            "k: |+\n  a\n\n\nj: 1\n",
            "k: |+\n  a\nj: 1\n",
            limit()
        ));
        assert!(same_structure(
            "k: |+\n    a\n\nj: 1\n",
            "k: |+\n  a\n\nj: 1\n",
            limit()
        ));
        assert!(same_structure(
            "k: |\n  a\n\nj: 1\n",
            "k: |\n  a\nj: 1\n",
            limit()
        ));
    }

    #[test]
    fn a_block_of_only_markers_and_directives_holds_no_yaml() {
        assert!(holds_no_yaml("---\n"));
        assert!(holds_no_yaml("%YAML 1.2\n---\n...\n\n"));
        assert!(!holds_no_yaml("--- a\n"));
        assert!(!holds_no_yaml("# c\n---\n"));
        assert!(!holds_no_yaml("a: 1\n"));
    }

    #[test]
    fn the_walk_keeps_its_own_stack_so_unbounded_nesting_never_overflows_a_small_one() {
        // No bound, 100,000 levels, and a thread stack far too small for a recursive walk of
        // them: the walk must finish on the heap.
        let deep = nested(100_000, false);
        let walked = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || shape(&deep, limit(), usize::MAX).is_some())
            .expect("the walking thread should start")
            .join()
            .expect("the walk should finish without overflowing its stack");
        assert!(walked, "the unbounded walk should compare the deep text");
    }

    #[test]
    fn block_and_flow_sequences_of_the_same_items_are_the_same_structure() {
        assert!(same_structure("a: [x, y]\n", "a:\n  - x\n  - y\n", limit()));
        assert!(same_structure("a:\n  - x\n  - y\n", "a: [x, y]\n", limit()));
        assert!(same_structure("- [a, b]\n- c\n", "[[a, b], c]\n", limit()));
        assert!(!same_structure("a: [x, y]\n", "a:\n  - x\n", limit()));
    }

    #[test]
    fn an_indentation_indicator_strips_exactly_its_indent() {
        assert!(!same_structure(
            "a:\n    b: |2\n        x\n",
            "a:\n  b: |2\n    x\n",
            limit()
        ));
        assert!(same_structure(
            "a:\n    b: |2\n        x\n",
            "a:\n  b: |4\n        x\n",
            limit()
        ));
        assert!(same_structure("- |1\n   x\n", "- |2\n    x\n", limit()));
        assert!(!same_structure("- |1\n   x\n", "- |1\n  x\n", limit()));
    }

    #[test]
    fn a_keep_scalar_that_ends_the_text_compares_by_its_value() {
        assert!(same_structure("k:   |+\n    a\n", "k: |+\n  a\n", limit()));
        assert!(same_structure("k:   |+\n    a\n", "k: |\n  a\n", limit()));
        assert!(same_structure(
            "k: |+\n  a\nj: 1\n",
            "k: |\n  a\nj: 1\n",
            limit()
        ));
        assert!(!same_structure("k: |+\n  a\n\n", "k: |\n  a\n", limit()));
        assert!(!same_structure("k: |-\n  a\n", "k: |\n  a\n", limit()));
        assert!(!same_structure("k: >\n  a\n", "k: |\n  a\n", limit()));
        assert!(!same_structure("k: |+\n  a\n\n", "k: |+\n  a\n", limit()));
    }

    /// The copy supdoc writes for the yamllint config `text`.
    fn read_lint_config(text: &str) -> Result<String, String> {
        lint_config(text).map(|config| config.text)
    }

    /// yamllint's defaults as supdoc writes them, the snippet relaxation set.
    const LINT_DEFAULTS: &str = "extends: default\nrules:\n  document-start: disable\n";

    #[test]
    fn each_parsable_line_is_a_diagnostic_and_a_syntax_errors_rule_is_syntax() {
        let stdout = b"stdin:2:4: [error] too many spaces after colon (colons)\n\
            stdin:3:4: [warning] truthy value should be one of [false, true] (truthy)\n\
            stdin:5:1: [error] syntax error: expected ',' or ']', but got '<stream end>' (syntax)\n";

        let found = lint_diagnostics(stdout).expect("parsable lines");

        let rules: Vec<(usize, usize, &str, bool)> = found
            .iter()
            .map(|found| (found.line, found.column, found.rule.as_str(), found.syntax))
            .collect();
        assert_eq!(
            rules,
            [
                (2, 4, "colons", false),
                (3, 4, "truthy", false),
                (5, 1, "syntax", true)
            ]
        );
        assert_eq!(
            found
                .iter()
                .map(|found| found.message.as_str())
                .collect::<Vec<_>>(),
            [
                "too many spaces after colon",
                "truthy value should be one of [false, true]",
                "expected ',' or ']', but got '<stream end>'",
            ]
        );
        assert_eq!(lint_diagnostics(b"a.yaml\n  1:1  error  bad  (x)\n"), None);
    }

    #[test]
    fn a_warning_fails_the_block_and_a_syntax_error_is_a_parse_failure_at_its_line() {
        let ran = |success, stdout: &[u8]| {
            Ok(Ran {
                success,
                stdout: stdout.to_vec(),
                stderr: Vec::new(),
            })
        };

        assert_eq!(
            lint_verdict("", false, ran(true, b"")),
            lint::Verdict::Clean
        );
        assert_eq!(
            lint_verdict(
                "",
                false,
                ran(false, b"stdin:1:1: [warning] comment not indented like content (comments-indentation)\n")
            ),
            lint::Verdict::Diagnostics(vec![Diagnostic {
                line: 1,
                column: 1,
                end_line: 1,
                syntax: false,
                rule: "comments-indentation".to_owned(),
                message: "comment not indented like content".to_owned(),
            }])
        );
        assert_eq!(
            lint_verdict(
                "",
                false,
                ran(
                    false,
                    b"stdin:1:3: [error] too many spaces (colons)\n\
                      stdin:2:1: [error] syntax error: could not find expected ':' (syntax)\n"
                )
            ),
            lint::Verdict::Failed(format::Verdict::ParseFailed {
                detail: "could not find expected ':'".to_owned(),
                line: Some(1),
            })
        );
    }

    #[test]
    fn a_lint_config_keeps_its_rules_and_options_drops_yaml_files_and_relaxes_document_start() {
        let written = read_lint_config(
            "---\n# style\nextends: relaxed\nyaml-files:\n  - '*.yaml'\nlocale: en_US.UTF-8\n\
             rules:\n  line-length:\n    max: 120 # wide\n    level: warning\n  \
             document-start: enable\n  truthy:\n    allowed-values: ['true', \"false\", on]\n  \
             key-ordering: disable\n  quoted-strings:\n    extra-allowed:\n    - '^http://'\n    \
             - \"it's\"\n    extra-required: []\n",
        );

        assert_eq!(
            written.as_deref(),
            Ok(
                "extends: relaxed\nlocale: en_US.UTF-8\nrules:\n  line-length:\n    max: 120\n    \
                level: warning\n  truthy:\n    allowed-values:\n      - \"true\"\n      - \"false\"\n      \
                - on\n  key-ordering: disable\n  quoted-strings:\n    extra-allowed:\n      \
                - \"^http://\"\n      - \"it's\"\n    extra-required: []\n  document-start: disable\n"
            )
        );
    }

    #[test]
    fn a_lint_config_without_extends_stays_without_and_the_defaults_extend_default() {
        assert_eq!(
            read_lint_config("rules:\n  truthy: enable\n").as_deref(),
            Ok("rules:\n  truthy: enable\n  document-start: disable\n")
        );
        assert_eq!(
            read_lint_config("extends: default\n").as_deref(),
            Ok(LINT_DEFAULTS)
        );
    }

    #[test]
    fn an_empty_lint_config_or_one_that_leaves_no_rule_on_is_refused() {
        let every_rule_off = |extends: &str| {
            let rules = LINT_RULES
                .iter()
                .map(|rule| format!("  {rule}: disable\n"))
                .collect::<Vec<_>>()
                .concat();
            format!("{extends}rules:\n{rules}")
        };
        for (config, says) in [
            (String::new(), "is empty"),
            ("# nothing\n---\n".to_owned(), "is empty"),
            ("rules:\n".to_owned(), "no yamllint rule on"),
            (
                "rules:\n  document-start: enable\n".to_owned(),
                "no yamllint rule on",
            ),
            (every_rule_off(""), "no yamllint rule on"),
            (every_rule_off("extends: default\n"), "no yamllint rule on"),
            (every_rule_off("extends: relaxed\n"), "no yamllint rule on"),
        ] {
            let refused = read_lint_config(&config).expect_err(&config);
            assert!(refused.contains(says), "{config:?} gave {refused:?}");
        }
        let one_left = every_rule_off("extends: default\n")
            .replace("  truthy: disable\n", "  truthy:\n    level: warning\n");
        assert!(read_lint_config(&one_left).is_ok(), "{one_left:?}");
    }

    #[test]
    fn a_line_break_pyyaml_reads_where_supdoc_reads_none_is_refused_at_every_value_site() {
        let sites = [
            "rules:\n  line-length:\n    max: 80{c}extends: /etc/passwd\n",
            "rules:\n  truthy:\n    allowed-values:\n      - 'true'{c}ignore-from-file: x\n",
            "rules:\n  truthy:\n    allowed-values: [a{c}ignore-from-file: x]\n",
            "extends: default\nlocale: C{c}extends: ../../x\n",
            "extends: default\nrules:\n  truthy:\n    level: 'warning{c}'\n",
        ];
        for c in ['\r', '\u{85}', '\u{2028}', '\u{2029}'] {
            for site in sites {
                let config = site.replace("{c}", &c.to_string());
                let refused = read_lint_config(&config).expect_err(&config);
                assert!(
                    refused.contains(&format!("U+{:04X}", u32::from(c))),
                    "{config:?} gave {refused:?}"
                );
            }
        }
        for config in [
            "extends: default\nlocale: C\u{1}\n",
            "extends: default\nlocale: C\u{7f}\n",
            "extends: default\n\u{feff}rules:\n  truthy: enable\n",
            "extends: default\nlocale: \"C\u{fffe}\"\n",
        ] {
            assert!(read_lint_config(config).is_err(), "{config:?}");
        }
    }

    #[test]
    fn crlf_line_ends_and_a_leading_byte_order_mark_are_read_as_plain_lines() {
        assert_eq!(
            read_lint_config("\u{feff}extends: default\r\nrules:\r\n  truthy: enable\r\n")
                .as_deref(),
            Ok("extends: default\nrules:\n  truthy: enable\n  document-start: disable\n")
        );
    }

    #[test]
    fn every_value_but_a_plain_word_number_or_boolean_is_written_quoted_and_escaped() {
        let written = read_lint_config(
            "extends: default\nlocale: 'fr_FR'\nrules:\n  quoted-strings:\n    \
             extra-allowed: [\"caf\u{e9}\", 'a\"b', ^x$, \"\u{1f600}\"]\n    required: false\n",
        );

        assert_eq!(
            written.as_deref(),
            Ok(
                "extends: default\nlocale: \"fr_FR\"\nrules:\n  quoted-strings:\n    \
                extra-allowed:\n      - \"caf\\u00e9\"\n      - \"a\\\"b\"\n      - \"^x$\"\n      \
                - \"\\U0001f600\"\n    required: false\n  document-start: disable\n"
            )
        );
    }

    #[test]
    fn a_lint_config_that_reads_files_skips_them_or_leaves_the_shape_is_refused() {
        for (config, says) in [
            ("extends: ../shared.yaml\n", "a file yamllint would read"),
            (
                "extends: ~/.config/yamllint/config\n",
                "a file yamllint would read",
            ),
            ("ignore: |\n  *.md\n", "sets `ignore`"),
            ("ignore-from-file: .gitignore\n", "sets `ignore-from-file`"),
            (
                "rules:\n  truthy:\n    ignore: docs/\n",
                "sets `rules.truthy.ignore`",
            ),
            (
                "rules:\n  truthy:\n    ignore-from-file: [.gitignore]\n",
                "sets `rules.truthy.ignore-from-file`",
            ),
            ("bogus: 1\n", "sets `bogus`"),
            ("rules:\n  bogus: enable\n", "sets rule `bogus`"),
            ("rules:\n  truthy: enable\n  truthy: disable\n", "twice"),
            (
                "rules:\n  truthy:\n    level: error\n    level: warning\n",
                "twice",
            ),
            ("extends: default\nextends: relaxed\n", "twice"),
            ("rules:\n  truthy: on\n", "line 2"),
            ("rules:\n  truthy:\n", "line 2"),
            ("rules:\n  truthy:\n    level: {a: b}\n", "line 3"),
            ("rules:\n  truthy:\n    allowed-values: [[a]]\n", "line 3"),
            (
                "rules:\n  truthy:\n    allowed-values:\n      - a\n     - b\n",
                "line 5",
            ),
            ("rules:\n  truthy:\n    level: &a error\n", "line 3"),
            ("rules:\n  truthy:\n    level: \"err\\u006fr\"\n", "line 3"),
            ("rules:\n\ttruthy: enable\n", "line 2"),
            ("rules:\n  truthy: enable\n   colons: enable\n", "line 3"),
            ("rules: {truthy: enable}\n", "line 1"),
            ("rules:\n  commas: {}\n", "line 2"),
            ("extends: default\n---\nrules: {}\n", "line 2"),
            ("extends: default\n...\n", "line 2"),
        ] {
            let refused = read_lint_config(config).expect_err(config);
            assert!(refused.contains(says), "{config:?} gave {refused:?}");
        }
    }

    #[test]
    fn the_nearest_lint_config_is_given_as_a_private_copy_and_its_first_name_wins() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let env = env_in(cache.path());
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(root.path().join(".yamllint"), "bogus: 1\n").expect("root config");
        fs::write(
            root.path().join("docs").join(LINT_CONFIG_FILES[1]),
            "rules:\n  truthy: enable\n",
        )
        .expect("nearer config");
        fs::write(
            root.path().join("docs").join(LINT_CONFIG_FILES[2]),
            "bogus: 1\n",
        )
        .expect("a later name beside it");

        let lint = lint_args(&env, root.path(), &root.path().join("docs/deep/a.md"))
            .expect("the nearer config should be accepted");
        let [flag, conf] = lint.flags.as_slice() else {
            panic!("expected `-c <path>`, got {:?}", lint.flags);
        };

        assert_eq!(flag, "-c", "the first flag should be -c");
        let conf = PathBuf::from(conf);
        assert!(conf.starts_with(cache.path()), "{conf:?} should be private");
        assert_eq!(
            fs::read_to_string(&conf).expect("the private copy"),
            "rules:\n  truthy: enable\n  document-start: disable\n"
        );
        assert_eq!(lint.file, "docs/.yamllint.yaml");
        assert!(
            lint.cwd.starts_with(cache.path()),
            "{:?} should be private",
            lint.cwd
        );
        assert_eq!(
            fs::read_dir(&lint.cwd)
                .map(|mut dir| dir.next().is_none())
                .ok(),
            Some(true),
            "yamllint's working directory should be empty"
        );
        assert!(matches!(
            lint_args(&env, root.path(), &root.path().join("a.md")),
            Err(Refusal::Config { file, .. }) if file == ".yamllint"
        ));
    }

    #[test]
    fn with_no_lint_config_yamllint_gets_its_defaults_relaxed_in_a_private_copy() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");

        let lint = lint_args(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("docs/a.md"),
        )
        .expect("no config is fine");

        assert_eq!(lint.file, "", "no project config is named");
        let conf = PathBuf::from(lint.flags.get(1).expect("`-c <path>` should name the copy"));
        assert!(conf.starts_with(cache.path()), "{conf:?} should be private");
        assert_eq!(fs::read_to_string(&conf).expect("the copy"), LINT_DEFAULTS);
        assert_eq!(
            lint_args(&ToolEnv::default(), root.path(), &root.path().join("a.md")),
            Err(Refusal::NoPrivateDir)
        );
    }

    #[test]
    fn a_lint_config_link_out_of_the_root_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let elsewhere = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(elsewhere.path().join("config"), "extends: default\n").expect("outside config");
        crate::cli::testing::symlink(
            elsewhere.path().join("config"),
            root.path().join(".yamllint"),
        )
        .expect("the link");

        let refused = lint_args(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("a.md"),
        );

        assert!(
            matches!(&refused, Err(Refusal::Config { why, .. }) if why.contains("outside")),
            "{refused:?}"
        );
    }

    #[test]
    fn only_a_yamllint_comment_naming_no_rule_switches_the_lint_step_off() {
        for block in [
            "# yamllint disable-file\na: 1\n",
            "#yamllint disable-file\n",
            "a: 1\n# yamllint disable\nb:   2\n",
            "a:   1  # yamllint disable-line\n",
            "a: 1 #  yamllint   disable \n",
            "#\u{1c}yamllint disable-file\na:   1\n",
            "# yamllint disable-file\u{1f}\n",
            "#\u{a0}yamllint\u{2003}disable\n",
            "a: |\n  # yamllint disable\n",
            "# yamllint disable\n# yamllint enable\na:   1\n",
        ] {
            assert!(switches_lint_off(block, &LINT_RULES), "{block:?}");
        }
        let named = |rules: &[&str]| -> String {
            rules
                .iter()
                .map(|rule| format!(" rule:{rule}"))
                .collect::<Vec<_>>()
                .concat()
        };
        let (first, rest) = LINT_RULES.split_at(10);
        assert!(switches_lint_off(
            &format!("# yamllint disable{}\n", named(&LINT_RULES)),
            &LINT_RULES
        ));
        assert!(switches_lint_off(
            &format!(
                "# yamllint disable{}\na: 1 # yamllint disable-line{}\n",
                named(first),
                named(rest)
            ),
            &LINT_RULES
        ));
        assert!(!switches_lint_off(
            &format!("# yamllint disable{}\n", named(rest)),
            &LINT_RULES
        ));

        for block in [
            "a: 1\n",
            "a: 1  # yamllint disable-line rule:colons\n",
            "# yamllint disable rule:truthy rule:colons\na: yes\n",
            "# yamllint enable\n",
            "a: '# yamllint'\n",
        ] {
            assert!(!switches_lint_off(block, &LINT_RULES), "{block:?}");
        }
    }

    #[test]
    fn comments_naming_every_rule_the_config_leaves_on_switch_the_step_off() {
        let named = |rules: &[&str]| -> String {
            rules
                .iter()
                .map(|rule| format!(" rule:{rule}"))
                .collect::<Vec<_>>()
                .concat()
        };
        let defaults = lint_config("extends: default\n")
            .expect("the defaults")
            .enabled;
        assert_eq!(
            defaults.len(),
            16,
            "default leaves 16 rules on: {defaults:?}"
        );
        assert!(!defaults.contains(&"document-start"), "{defaults:?}");
        assert!(switches_lint_off(
            &format!("# yamllint disable{}\nb: yes\n", named(&defaults)),
            &defaults
        ));
        let all_but_one = defaults.get(1..).expect("16 rules");
        assert!(!switches_lint_off(
            &format!("# yamllint disable{}\nb: yes\n", named(all_but_one)),
            &defaults
        ));

        let truthy = lint_config("rules:\n  truthy: enable\n")
            .expect("one rule on")
            .enabled;
        assert_eq!(truthy, ["truthy"]);
        assert!(switches_lint_off(
            "# yamllint disable rule:truthy\nb: yes\n",
            &truthy
        ));
        assert!(!switches_lint_off(
            "# yamllint disable rule:colons\nb: yes\n",
            &truthy
        ));
    }

    #[test]
    fn a_traceback_is_named_by_its_last_line_without_paths_and_capped() {
        let traceback =
            b"Traceback (most recent call last):\n  File \"/opt/venv/lib/yamllint/cli.py\", \
            line 209, in run\n    locale.setlocale(locale.LC_ALL, conf.locale)\n\
            FileNotFoundError: [Errno 2] No such file or directory: '/Users/me/.ssh/id'\n";

        assert_eq!(
            internal_error(traceback).as_deref(),
            Some(
                "yamllint stopped with an internal error: FileNotFoundError: [Errno 2] No such \
                 file or directory: '<path>'"
            )
        );
        let long = format!("{}\nValueError: {}\n", super::TRACEBACK, "x".repeat(400));
        let shown = internal_error(long.as_bytes()).expect("a traceback");
        assert_eq!(
            shown.chars().count(),
            "yamllint stopped with an internal error: ".len() + 200,
            "{shown}"
        );
        assert_eq!(internal_error(b"invalid config: bad\n"), None);
        assert!(!locale_failed(traceback));
        assert!(locale_failed(
            b"Traceback (most recent call last):\n  File \"x\"\nlocale.Error: unsupported locale setting\n"
        ));
    }

    #[test]
    fn a_traceback_on_a_failed_run_is_a_tool_failure_in_supdocs_words() {
        let verdict = lint_verdict(
            "",
            false,
            Ok(Ran {
                success: false,
                stdout: Vec::new(),
                stderr:
                    b"Traceback (most recent call last):\n  File \"/x/cli.py\"\nKeyError: 'a'\n"
                        .to_vec(),
            }),
        );

        assert_eq!(
            verdict,
            lint::Verdict::Failed(format::Verdict::ToolFailed(
                "yamllint stopped with an internal error: KeyError: 'a'".to_owned()
            ))
        );
    }

    #[test]
    fn a_control_yaml_forbids_is_found_with_its_line_and_tab_and_carriage_return_are_not() {
        assert_eq!(forbidden_char("a: 1\nb: \u{0}2\n"), Some((1, '\u{0}')));
        assert_eq!(
            forbidden_char("#\u{1c}yamllint disable-file\n"),
            Some((0, '\u{1c}'))
        );
        assert_eq!(forbidden_char("a: \u{85}\n"), Some((0, '\u{85}')));
        assert_eq!(forbidden_char("a:\t1\r\nb: 2\n"), None);
    }

    #[test]
    fn only_yamllints_invalid_config_report_is_a_rejected_config() {
        assert!(rejected_config(
            b"invalid config: rule \"truthy\": should be either \"enable\", \"disable\" or a dict\n"
        ));
        assert!(!rejected_config(b"Traceback (most recent call last):\n"));
    }

    #[cfg(unix)]
    #[test]
    fn a_config_directory_swapped_for_a_link_out_gets_no_private_copy_and_creates_nothing_there() {
        use super::private_config;
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let outside = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::set_permissions(outside.path(), fs::Permissions::from_mode(0o700))
            .expect("the mode should be settable");
        let code_blocks = cache.path().join("supdoc/code-blocks");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&code_blocks)
            .expect("supdoc's cache directory");
        crate::cli::testing::symlink(outside.path(), code_blocks.join("yamlfmt-config"))
            .expect("the planted link");
        let env = env_in(cache.path());

        let written = private_config(&env, root.path(), "formatter:\n  type: basic\n");

        assert!(written.is_none(), "got {written:?}");
        let left: Vec<_> = fs::read_dir(outside.path())
            .expect("the outside dir should be listable")
            .collect();
        assert!(
            left.is_empty(),
            "nothing should be created outside: {left:?}"
        );
    }
}
