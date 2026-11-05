//! yamlfmt: its syntax-error report, the config supdoc gives it, and the check that its output
//! says what the block said.
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

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use super::format::ParseError;
use super::runner;
use super::tools::ToolEnv;
use crate::cartridges::yaml as yaml_cartridge;
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
/// Why the config can't be given to yamlfmt: YAML outside that shape, a key supdoc doesn't know
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
                .is_some_and(|(_, next)| is_blank(next) || next.starts_with([' ', '\t', '-']))
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
            .filter(|_| !setting.quoted && value.bytes().all(|b| b.is_ascii_digit()))
            .map(|count| count.to_string())
            .ok_or_else(|| format!("sets `{key}` to `{value}`, not a count up to 65535"));
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

/// Writes `text` into `.yamlfmt` in a private directory named by the hash of `text`, under
/// supdoc's `yamlfmt-config` directory, and returns the file's path. A file already there with
/// the same content is kept; anything else at the name is replaced by writing a temporary file in
/// the directory and renaming it into place, so concurrent runs never see half a file.
///
/// Effect: creates the directories mode 0700 and the file mode 0600, and may replace a stale file.
///
/// `None` when there is no private directory or the file can't be written.
fn private_config(env: &ToolEnv, root: &Path, text: &str) -> Option<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let parent = neutral.parent()?.join("yamlfmt-config");
    let dir = parent.join(Hash::digest(text.as_bytes()).to_string());
    let private = |path: &Path| {
        fs::symlink_metadata(path)
            .is_ok_and(|meta| meta.is_dir() && meta.permissions().mode().trailing_zeros() >= 6)
    };
    for path in [&parent, &dir] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .ok()?;
        if !private(path) {
            return None;
        }
    }
    let file = dir.join(".yamlfmt");
    let current = fs::symlink_metadata(&file)
        .ok()
        .filter(fs::Metadata::is_file)
        .and_then(|_| fs::read_to_string(&file).ok());
    if current.as_deref() != Some(text) {
        static WRITERS: AtomicUsize = AtomicUsize::new(0);
        let temp = dir.join(format!(
            ".yamlfmt.{}.{}.tmp",
            std::process::id(),
            WRITERS.fetch_add(1, Ordering::Relaxed)
        ));
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .and_then(|mut out| out.write_all(text.as_bytes()))
            .and_then(|()| fs::rename(&temp, &file));
        if written.is_err() {
            let _own = fs::remove_file(&temp);
            return None;
        }
    }
    Some(file)
}

/// Whether `before` and `after` parse, each within `limit`, to the same YAML structure: the same
/// documents, mappings, sequences, keys, anchors, aliases, tags and scalars, comments and layout
/// aside. A scalar keeps whether it was plain or quoted, and a missing value reads as `null`, so
/// `{a}` and `{a: ''}` differ. False when either doesn't parse, or [`yaml_cartridge`]'s guards on
/// the grammar refuse it, so a fix it can't check is never applied.
#[must_use]
pub(crate) fn same_structure(before: &str, after: &str, limit: TimeLimit) -> bool {
    match (shape(before, limit), shape(after, limit)) {
        (Some(before), Some(after)) => before == after,
        _ => false,
    }
}

/// The structure of `text` as [`same_structure`] compares it, `None` when it doesn't parse
/// cleanly within `limit`.
fn shape(text: &str, limit: TimeLimit) -> Option<String> {
    // The path only names the text in a parse error, which this check discards.
    let path = AbsolutePath::new(Path::new("/block.yaml"))?;
    let parsed = SourceFile::from_text(
        path,
        text.to_owned(),
        yaml_cartridge::cartridge(),
        Deadline::start(limit),
    )
    .ok()?;
    let root = parsed.tree().root_node();
    if root.has_error() {
        return None;
    }
    let mut out = String::new();
    write_shape(root, text, &mut out);
    Some(out)
}

/// Appends the structure of `node`, a node of `text`, to `out`.
fn write_shape(node: Node<'_>, text: &str, out: &mut String) {
    let source = node.utf8_text(text.as_bytes()).unwrap_or_default();
    match node.kind() {
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
            let body = dedent(source);
            let _infallible = write!(out, "b{}:{body};", body.len());
        }
        "anchor" | "alias" | "tag" => {
            let _infallible = write!(out, "{}{}:{source};", node.kind(), source.len());
        }
        "block_mapping_pair" | "flow_pair" => {
            out.push('(');
            match node.child_by_field_name("key") {
                Some(key) => write_shape(key, text, out),
                None => out.push_str("null;"),
            }
            out.push('=');
            match node.child_by_field_name("value") {
                Some(value) => write_shape(value, text, out),
                None => out.push_str("null;"),
            }
            out.push(')');
        }
        "flow_mapping" => {
            out.push('{');
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "flow_node" {
                    out.push('(');
                    write_shape(child, text, out);
                    out.push_str("=null;)");
                } else {
                    write_shape(child, text, out);
                }
            }
            out.push('}');
        }
        kind => {
            let (open, close) = match kind {
                "block_mapping" => ("{", "}"),
                "block_sequence" | "flow_sequence" => ("[", "]"),
                "document" => ("<", ">"),
                _ => ("", ""),
            };
            out.push_str(open);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                write_shape(child, text, out);
            }
            out.push_str(close);
        }
    }
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

/// A block scalar's text with its header line kept and the indent its content lines share
/// removed, so a re-indented block scalar reads the same.
fn dedent(source: &str) -> String {
    let mut lines = source.split('\n');
    let header = lines.next().unwrap_or_default().trim_end();
    let body: Vec<&str> = lines.map(|line| line.trim_end_matches('\r')).collect();
    let shared = body
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    let mut out = header.to_owned();
    for line in body {
        out.push('\n');
        out.push_str(
            line.get(shared..)
                .unwrap_or_else(|| line.trim_start_matches(' ')),
        );
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_FILES, Refusal, parse_error, project_args, read_config, same_structure};
    use crate::code_blocks::format::ParseError;
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
        std::os::unix::fs::symlink(outside.path().join("c.yaml"), root.path().join(".yamlfmt"))
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
}
