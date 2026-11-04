//! Rust's format and lint steps.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::code_blocks::Edition;
use crate::finding::{IndexedText, Location};

use super::block::CodeBlock;
use super::format::{self, ParseError, first_line};
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

/// The macros that read a file or an environment variable at compile time.
const COMPILE_TIME_READS: [&str; 5] = [
    "include",
    "include_str",
    "include_bytes",
    "env",
    "option_env",
];

/// The `rustfmt.toml` names rustfmt reads, in its own order.
const CONFIG_FILES: [&str; 2] = [".rustfmt.toml", "rustfmt.toml"];

/// The edition a block's own `edition20xx` attribute names; the first one wins.
#[must_use]
pub(crate) fn attribute_edition(attributes: &[&str]) -> Option<Edition> {
    attributes
        .iter()
        .find_map(|word| word.strip_prefix("edition").and_then(Edition::from_year))
}

/// `rustfmt`'s arguments for any block: `--edition <year> --emit stdout`. The block arrives on
/// stdin as written, with no wrapping; rustfmt takes no stdin file name, so the doc and the
/// block's index don't appear.
#[must_use]
pub(crate) fn format_args(edition: Edition) -> Vec<OsString> {
    ["--edition", edition.year(), "--emit", "stdout"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

/// `--config-path <file>` for the first of `root`'s `.rustfmt.toml` and `rustfmt.toml` that
/// exists, by its real path, when that is a regular file inside `root`'s real path. Empty
/// otherwise: rustfmt then searches from its neutral working directory, never the checkout.
///
/// Effect: reads file metadata and resolves symlinks.
#[must_use]
pub(crate) fn config_args(root: &Path) -> Vec<OsString> {
    let Ok(real_root) = root.canonicalize() else {
        return Vec::new();
    };
    let Some(file) = CONFIG_FILES
        .iter()
        .map(|name| root.join(name))
        .find(|path| std::fs::symlink_metadata(path).is_ok())
    else {
        return Vec::new();
    };
    match file.canonicalize() {
        Ok(real)
            if real.starts_with(&real_root)
                && std::fs::metadata(&real).is_ok_and(|meta| meta.is_file()) =>
        {
            vec![OsString::from("--config-path"), real.into_os_string()]
        }
        _ => Vec::new(),
    }
}

/// The lints clippy leaves off for a block, which a snippet breaks for lack of context, as
/// `-A` flags after the project's own so they win.
const RELAXED: [&str; 4] = [
    "unused",
    "dead_code",
    "missing_docs",
    "clippy::missing_docs_in_private_items",
];

/// The lint step's fixed arguments for `tool` on any block: none, since every clippy argument
/// depends on the run (see [`clippy_args`]). `Some` only for clippy, which marks Rust's lint
/// step as built.
#[must_use]
pub(crate) fn lint_args(
    tool: &super::tools::Tool,
    _doc: &Path,
    _index: usize,
) -> Option<Vec<OsString>> {
    (tool.name == "clippy").then(Vec::new)
}

/// `clippy-driver`'s arguments for a block: `--edition <E> --crate-type lib
/// --emit=metadata=<emit> --error-format=json`, the project's `lints` flags, the snippet
/// relaxations, and `-` for the block on stdin. Nothing else from the project: no `--extern`, `-L`
/// or `--cfg`. Without `emit`, the metadata flag has no path, which is how a cache key names
/// every run of the same block alike.
#[must_use]
pub(crate) fn clippy_args(
    edition: Edition,
    emit: Option<&Path>,
    lints: &[OsString],
) -> Vec<OsString> {
    let mut metadata = OsString::from("--emit=metadata");
    if let Some(emit) = emit {
        metadata.push("=");
        metadata.push(emit);
    }
    let mut args: Vec<OsString> = vec![
        "--edition".into(),
        edition.year().into(),
        "--crate-type".into(),
        "lib".into(),
        metadata,
        "--error-format=json".into(),
    ];
    args.extend(lints.iter().cloned());
    for lint in RELAXED {
        args.extend(["-A".into(), lint.into()]);
    }
    args.push("-".into());
    args
}

/// The clippy config names clippy reads from a directory, in its own order.
const CLIPPY_FILES: [&str; 2] = [".clippy.toml", "clippy.toml"];

/// The value for `CLIPPY_CONF_DIR`: `root`'s real path, when it holds a `.clippy.toml` or
/// `clippy.toml` and each of those that exists is a regular file whose real path is inside
/// `root`'s. `None` otherwise, so clippy never follows a link out of the checkout.
///
/// Effect: reads file metadata and resolves symlinks.
#[must_use]
pub(crate) fn clippy_conf_dir(root: &Path) -> Option<OsString> {
    let real_root = root.canonicalize().ok()?;
    let present: Vec<_> = CLIPPY_FILES
        .iter()
        .map(|name| root.join(name))
        .filter(|path| std::fs::symlink_metadata(path).is_ok())
        .collect();
    let inside = |path: &PathBuf| {
        path.canonicalize().is_ok_and(|real| {
            real.starts_with(&real_root) && std::fs::metadata(&real).is_ok_and(|m| m.is_file())
        })
    };
    (!present.is_empty() && present.iter().all(inside)).then(|| real_root.into_os_string())
}

/// One clippy run's directory for its metadata, beside the neutral working directory so that
/// stays empty, made fresh and removed with everything in it when dropped: on success, failure,
/// timeout and unwinding alike.
#[derive(Debug)]
pub(crate) struct EmitDir {
    /// The directory, owned by this run alone.
    path: PathBuf,
}

/// How many [`EmitDir`]s this process has tried to create, which makes each name unique.
static EMITS: AtomicUsize = AtomicUsize::new(0);

impl EmitDir {
    /// A new directory `emit/<pid>-<n>` beside `neutral`, the tool's working directory, created
    /// mode 0700 inside a 0700 `emit` directory. `None` when `neutral` doesn't resolve or the
    /// directory can't be created.
    ///
    /// Effect: creates directories.
    #[must_use]
    pub(crate) fn create(neutral: &Path) -> Option<Self> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        let parent = neutral.canonicalize().ok()?.parent()?.join("emit");
        let _created = std::fs::DirBuilder::new().mode(0o700).create(&parent);
        let meta = std::fs::symlink_metadata(&parent).ok()?;
        if !meta.is_dir() || meta.permissions().mode().trailing_zeros() < 6 {
            return None;
        }
        (0..16).find_map(|_| {
            let count = EMITS.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{}-{count}", std::process::id()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .ok()
                .map(|()| Self { path })
        })
    }

    /// The metadata file clippy writes in this directory.
    #[must_use]
    pub(crate) fn file(&self) -> PathBuf {
        self.path.join("block.rmeta")
    }
}

impl Drop for EmitDir {
    fn drop(&mut self) {
        let _removed = std::fs::remove_dir_all(&self.path);
    }
}

/// One diagnostic of `clippy-driver --error-format=json`.
#[derive(serde::Deserialize)]
struct RustcDiagnostic {
    /// What it says.
    message: String,
    /// Its code: a lint name such as `clippy::ptr_arg`, or an error code such as `E0425`.
    code: Option<RustcCode>,
    /// `error`, `warning`, `note`, `help` or `failure-note`.
    level: String,
    /// Where it points.
    spans: Vec<RustcSpan>,
}

/// A diagnostic's code.
#[derive(serde::Deserialize)]
struct RustcCode {
    /// The code itself.
    code: String,
}

/// One span of a diagnostic, its line and column 1-based in characters.
#[derive(serde::Deserialize)]
struct RustcSpan {
    /// The file it is in: `<anon>` for the block on stdin.
    file_name: String,
    /// Whether it is the diagnostic's main span.
    is_primary: bool,
    /// Its first line.
    line_start: usize,
    /// Its first column.
    column_start: usize,
}

/// The diagnostics in `clippy-driver --error-format=json`'s `stderr`: each JSON line at level
/// `error` or `warning`, at its primary span in the block, or at line 0 (the fence) when it has
/// none there. The summaries rustc closes with, which have neither a code nor a span, are left
/// out, as is any line that isn't a diagnostic.
fn rustc_diagnostics(stderr: &[u8]) -> Vec<Diagnostic> {
    stderr
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<RustcDiagnostic>(line).ok())
        .filter(|found| matches!(found.level.as_str(), "error" | "warning"))
        .filter(|found| found.code.is_some() || !found.spans.is_empty())
        .map(|found| {
            let (line, column) = found
                .spans
                .iter()
                .find(|span| span.is_primary && span.file_name == "<anon>")
                .map_or((0, 0), |span| (span.line_start, span.column_start));
            Diagnostic {
                line,
                column,
                rule: found.code.map_or(found.level, |code| code.code),
                message: found.message,
            }
        })
        .collect()
}

/// Judges a clippy run: its diagnostics on stderr, whatever its exit (clippy exits 0 on
/// warnings); clean on exit 0 with none; a tool failure on a non-zero exit with none, output
/// past the cap or a failed start; a timeout is a timeout.
#[must_use]
pub(crate) fn lint_verdict(result: Result<Ran, Failure>) -> lint::Verdict {
    match result {
        Ok(ran) => {
            let found = rustc_diagnostics(&ran.stderr);
            if !found.is_empty() {
                lint::Verdict::Diagnostics(found)
            } else if ran.success {
                lint::Verdict::Clean
            } else {
                lint::Verdict::Failed(format::Verdict::ToolFailed(format::exit_detail(
                    &ran.stderr,
                )))
            }
        }
        Err(failure) => lint::Verdict::Failed(format::failed(failure)),
    }
}

/// What a Rust block's tools don't see as written: its hidden lines and its `##`-escaped lines.
/// No line is added or removed, so the checked text's lines are the block's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hidden {
    /// Each hidden line, in order.
    lines: Vec<HiddenLine>,
    /// Each `##`-escaped line: its checked text without leading whitespace, and how many shown
    /// lines with that same text come before it.
    escaped: Vec<(String, usize)>,
    /// Each line the checked text shortened, hidden or escaped: its 0-based line and how many
    /// characters after its indentation the checked text dropped.
    shifts: Vec<(usize, usize)>,
}

/// One hidden line of a Rust block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HiddenLine {
    /// Its 0-based line in the checked text.
    at: usize,
    /// Its checked text: its indentation and the text after `# `, or empty for a blank one.
    checked: String,
    /// The line as the author wrote it, which a fix writes back unchanged.
    written: String,
}

/// `input`, a Rust block with every line LF-ended, as its tools check it, with what it hides.
/// As in rustdoc, a line whose text after its leading whitespace is `#` alone or starts with
/// `# ` is hidden: it is checked as its indentation and the text after `# ` (empty when that is
/// blank). A line whose text after its leading whitespace starts `##` is checked without that
/// first `#`. `#[` and `#!` are ordinary lines.
///
/// The indentation stays so a hidden line inside a body is checked where the formatter wants
/// it; whitespace means nothing else to the compiler.
#[must_use]
pub(crate) fn unhide(input: &str) -> (String, Hidden) {
    let mut checked = String::with_capacity(input.len());
    let mut hidden = Hidden {
        lines: Vec::new(),
        escaped: Vec::new(),
        shifts: Vec::new(),
    };
    let mut seen = Seen::default();
    for (at, line) in input.split_terminator('\n').enumerate() {
        let rest = line.trim_start();
        let indent = line.get(..line.len() - rest.len()).unwrap_or_default();
        if let Some(escaped) = rest.strip_prefix('#').filter(|_| rest.starts_with("##")) {
            let occurrence = seen.count(escaped);
            hidden.escaped.push((escaped.to_owned(), occurrence));
            hidden.shifts.push((at, 1));
            checked.push_str(indent);
            checked.push_str(escaped);
        } else if let Some(text) = (rest == "#").then_some("").or(rest.strip_prefix("# ")) {
            let text = if text.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{text}")
            };
            let dropped = if text.is_empty() {
                rest.chars().count()
            } else {
                2
            };
            hidden.shifts.push((at, dropped));
            checked.push_str(&text);
            hidden.lines.push(HiddenLine {
                at,
                checked: text,
                written: line.to_owned(),
            });
        } else {
            let _occurrence = seen.count(rest);
            checked.push_str(line);
        }
        checked.push('\n');
    }
    (checked, hidden)
}

/// How many shown lines with each text, without leading whitespace, have been read so far.
#[derive(Debug, Default)]
struct Seen(std::collections::HashMap<String, usize>);

impl Seen {
    /// How many shown lines reading `text` came before this one, counting this one from now on.
    fn count(&mut self, text: &str) -> usize {
        let count = self.0.entry(text.to_owned()).or_default();
        *count += 1;
        *count - 1
    }
}

impl Hidden {
    /// How many characters the checked text dropped from its `line`th line (0-based), after its
    /// indentation: 2 for a hidden line's `# `, 1 for an escaped line's first `#`, 0 for any
    /// other line. A diagnostic on that line lands this far right in the doc.
    #[must_use]
    pub(crate) fn removed(&self, line: usize) -> usize {
        self.shifts
            .iter()
            .find(|(at, _)| *at == line)
            .map_or(0, |(_, dropped)| *dropped)
    }

    /// `output`, a formatter's output for the checked text with no final line ending, written
    /// back as the block. Each hidden line goes back exactly as the author wrote it. A shown line
    /// gets its `#` escape back after its indentation when it is the same occurrence of the same
    /// text as an escaped line, or when it would otherwise read as hidden or escaped.
    ///
    /// # Errors
    ///
    /// The checked-text line of the first hidden line that can't be matched one-to-one with a
    /// line of `output`: its text or indentation changed, it was removed, or its place among
    /// `output`'s lines is ambiguous, so the formatter added, removed or reordered hidden lines.
    pub(crate) fn rehide(&self, output: &str) -> Result<String, usize> {
        let lines: Vec<&str> = output.split('\n').collect();
        let earliest = first_matches(lines.iter().copied().enumerate(), self.lines.iter())?;
        let mut latest = first_matches(
            lines.iter().copied().enumerate().rev(),
            self.lines.iter().rev(),
        )?;
        latest.reverse();
        if let Some((line, _)) = self
            .lines
            .iter()
            .zip(earliest.iter().zip(&latest))
            .find(|(_, (early, late))| early != late)
        {
            return Err(line.at);
        }
        let mut hidden = earliest.into_iter().zip(&self.lines).peekable();
        let mut seen = Seen::default();
        let written: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(at, line)| {
                if let Some((_, original)) = hidden.next_if(|(found, _)| *found == at) {
                    return original.written.clone();
                }
                let rest = line.trim_start();
                let indent = line.get(..line.len() - rest.len()).unwrap_or_default();
                let occurrence = seen.count(rest);
                let was_escaped = self
                    .escaped
                    .iter()
                    .any(|(text, nth)| text == rest && *nth == occurrence);
                let must_escape = rest == "#" || rest.starts_with("# ") || rest.starts_with("##");
                if was_escaped || must_escape {
                    format!("{indent}#{rest}")
                } else {
                    (*line).to_owned()
                }
            })
            .collect();
        Ok(written.join("\n"))
    }
}

/// The output line each of `hidden` matches, walking `lines` in the given order and taking the
/// first line equal to its checked text each time.
///
/// # Errors
///
/// The checked-text line of the first hidden line with no match.
fn first_matches<'output, 'hidden>(
    mut lines: impl Iterator<Item = (usize, &'output str)>,
    hidden: impl Iterator<Item = &'hidden HiddenLine>,
) -> Result<Vec<usize>, usize> {
    hidden
        .map(|line| {
            lines
                .find(|(_, output)| *output == line.checked)
                .map(|(found, _)| found)
                .ok_or(line.at)
        })
        .collect()
}

/// Where each line of a block's checked text is in its doc. No line is added or removed, so the
/// checked text's `n`th line is the block body's `n`th line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineMap {
    /// Each body line's text in the doc, located.
    lines: Vec<Location>,
}

impl LineMap {
    /// The map for `block`, found in `text`. `None` when a body line isn't inside `text`.
    #[must_use]
    pub(crate) fn of(block: &CodeBlock, text: &str) -> Option<Self> {
        let indexed = IndexedText::new(text);
        let lines = block
            .body
            .iter()
            .map(|line| indexed.locate(block.fence.file.clone(), line.content.clone()))
            .collect::<Option<_>>()?;
        Some(Self { lines })
    }

    /// Where the checked text's `line`th line (0-based) is in the doc.
    #[must_use]
    pub(crate) fn doc_line(&self, line: usize) -> Option<&Location> {
        self.lines.get(line)
    }
}

/// The largest block, in bytes, the compile-time-read scan reads. A bigger one is not checked.
pub(crate) const MAX_SCAN_BYTES: usize = 256 * 1024;

/// What the compile-time-read scan found in a Rust block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scan {
    /// The block holds no compile-time read.
    Clean,
    /// The block holds this compile-time read, or can't be scanned past a point that could hide
    /// one.
    Read(Found),
    /// The block is over [`MAX_SCAN_BYTES`], so it wasn't scanned.
    TooLarge,
}

/// What made the scan refuse a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// A call of this macro, named without its `!`: `include_str`.
    Macro(String),
    /// An attribute with `path =` in it, or, before compiling, `path` anywhere in it.
    PathAttribute,
    /// Before compiling: one of the compile-time-read macros' names, not as a path segment
    /// followed by `::`, such as `include_str` in `use std::include_str as inc;`.
    Name(String),
    /// Before compiling: a `$` metavariable inside an attribute.
    MetavariableAttribute,
    /// Before compiling: an attribute whose path's first segment isn't in
    /// [`ALLOWED_ATTRIBUTES`], named by that segment (empty when it doesn't start with a name).
    Attribute(String),
    /// Before compiling: a module not written inline outside any macro, with its name when the
    /// word after `mod` is one: `x` in `mod x;`.
    OutlineModule(Option<String>),
    /// Something the scan can't read past, which could hide a read, described as "an
    /// unterminated block comment".
    Unscannable(&'static str),
}

impl Found {
    /// What the block has, for the warning: "uses `env!`, which reads ...".
    #[must_use]
    pub(crate) fn why(&self) -> String {
        match self {
            Self::Macro(name) => format!(
                "uses `{name}!`, which reads a file or environment variable at compile time"
            ),
            Self::PathAttribute => {
                "uses a `path` attribute, which reads a file at compile time".to_owned()
            }
            Self::Name(name) => format!(
                "names `{name}` other than in a `{name}::` path, which could call `{name}!` under another name and read a file or environment variable at compile time"
            ),
            Self::MetavariableAttribute => {
                "has a `$` metavariable in an attribute, which a macro could make a `path` attribute".to_owned()
            }
            Self::Attribute(name) if name.is_empty() => {
                "has an attribute whose path doesn't start with a name, which supdoc can't check for a compile-time read".to_owned()
            }
            Self::Attribute(name) => format!(
                "uses the `{name}` attribute, which isn't on supdoc's list of attributes that never read a file at compile time"
            ),
            Self::OutlineModule(Some(name)) => format!(
                "declares the outline module `{name}`, which rustc would read from a file outside the block"
            ),
            Self::OutlineModule(None) => {
                "declares a module that a macro could make an outline module, which rustc would read from a file outside the block".to_owned()
            }
            Self::Unscannable(what) => {
                format!("has {what}, which could hide a compile-time read from supdoc's scan")
            }
        }
    }

    /// How the author can fix it, for the warning's help.
    #[must_use]
    pub(crate) fn help(&self) -> String {
        match self {
            Self::Macro(name) => {
                format!("remove the `{name}!` from the block, or mark it `ignore(<why>)`")
            }
            Self::PathAttribute => {
                "remove the `path` attribute from the block, or mark it `ignore(<why>)`".to_owned()
            }
            Self::Name(name) => format!(
                "write `{name}` only as a path such as `std::{name}::..`, or mark the block `ignore(<why>)`"
            ),
            Self::MetavariableAttribute => {
                "build the attribute without a metavariable, or mark the block `ignore(<why>)`".to_owned()
            }
            Self::Attribute(name) if name.is_empty() => {
                "name the attribute plainly, or mark the block `ignore(<why>)`".to_owned()
            }
            Self::Attribute(name) => format!(
                "remove the `{name}` attribute from the block, or mark it `ignore(<why>)`"
            ),
            Self::OutlineModule(_) => {
                "write the module inline as `mod name { .. }` outside any macro, or mark the block `ignore(<why>)`".to_owned()
            }
            Self::Unscannable(_) => {
                "close it so the block is valid Rust, or mark the block `ignore(<why>)`".to_owned()
            }
        }
    }
}

/// Scans `input`, a Rust block, for a compile-time read: a call of `include!`, `include_str!`,
/// `include_bytes!`, `env!` or `option_env!` (comments and whitespace may sit before the `!`,
/// and `r#env` counts), or an attribute with `path =` anywhere in its brackets (`#[path = ..]`,
/// `#![path = ..]`, `#[cfg_attr(.., path = ..)]`). [`Scan::TooLarge`] past [`MAX_SCAN_BYTES`].
///
/// One linear pass: [`Tokens`] skips comments and string and character literals once each, and
/// each token is judged from the two before and after it. The scan fails closed: an unterminated
/// block comment, string or character literal, or a first line starting `#!` but not `#![`, is
/// [`Found::Unscannable`]. Identifiers are ASCII, so any other character ends a word, which can
/// only add matches.
///
/// This is enough for a tool that doesn't expand macros, such as rustfmt. Before a block is
/// compiled, [`before_compiling`] applies stricter rules.
#[must_use]
pub(crate) fn compile_time_read(input: &str) -> Scan {
    scan_capped(input, false)
}

/// [`compile_time_read`] with the rules for a block about to be compiled, which renames and
/// macros can't get past. Besides its findings, a block is refused when:
///
/// - `include`, `include_str`, `include_bytes`, `env` or `option_env` appears as an identifier
///   anywhere except as a path segment followed by `::` (`std::env::var`), so `use
///   std::include_str as inc;` and a `macro_rules!` handed the name are refused;
/// - a `$` metavariable or the word `path` appears anywhere in an attribute;
/// - a module isn't written inline (`mod name { .. }`) outside any macro: rustc reads an outline
///   `mod name;` from a file beside its working directory, and a macro can turn an inline module
///   into one.
#[must_use]
pub(crate) fn before_compiling(input: &str) -> Scan {
    scan_capped(input, true)
}

/// [`scan`] of `input` under [`MAX_SCAN_BYTES`], `compiles` choosing [`before_compiling`]'s rules.
fn scan_capped(input: &str, compiles: bool) -> Scan {
    if input.len() > MAX_SCAN_BYTES {
        return Scan::TooLarge;
    }
    scan(input, compiles).map_or(Scan::Clean, Scan::Read)
}

/// The first compile-time read in `input`, by [`compile_time_read`]'s rules, and by
/// [`before_compiling`]'s too when `compiles` holds, without the size cap.
fn scan(input: &str, compiles: bool) -> Option<Found> {
    if input.starts_with("#!") && !input.starts_with("#![") {
        return Some(Found::Unscannable(
            "a `#!` first line that isn't an inner attribute",
        ));
    }
    let mut lexer = Tokens::new(input);
    let tokens: Vec<Token<'_>> = lexer.by_ref().collect();
    let at = |index: Option<usize>| {
        index
            .and_then(|index| tokens.get(index))
            .copied()
            .unwrap_or(Token::Other)
    };
    // The bracket depth inside the attribute being read, or 0 outside one.
    let mut attribute = 0_usize;
    let mut nesting = Nesting::default();
    // The macro whose input or body the next delimiter opens: a `name!` or `macro_rules! name`
    // just ended.
    let mut macro_next: Option<MacroKind> = None;
    for (index, &token) in tokens.iter().enumerate() {
        let before = at(index.checked_sub(1));
        let before_that = at(index.checked_sub(2));
        let next = at(index.checked_add(1));
        let after_next = at(index.checked_add(2));
        let opens_macro = macro_next.take();
        match token {
            Token::Punct('!') => {
                if let Token::Ident(word) = before {
                    if COMPILE_TIME_READS.contains(&word) {
                        return Some(Found::Macro(word.to_owned()));
                    }
                    macro_next = Some(if word == "macro_rules" {
                        MacroKind::Definition
                    } else {
                        MacroKind::Call
                    });
                }
            }
            Token::Punct('#') if compiles && nesting.in_call > 0 => {
                return Some(Found::Unscannable("a `#` inside a macro call's input"));
            }
            Token::Punct('#') if compiles => {
                let starts_attribute = next == Token::Punct('[')
                    || (next == Token::Punct('!') && after_next == Token::Punct('['));
                if !starts_attribute {
                    return Some(Found::Unscannable("a `#` that doesn't start an attribute"));
                }
            }
            Token::Punct('[') if attribute > 0 => attribute += 1,
            Token::Punct('[') => {
                let opens = before == Token::Punct('#')
                    || (before == Token::Punct('!') && before_that == Token::Punct('#'));
                if opens {
                    attribute = 1;
                    if compiles
                        && let Some(found) =
                            unlisted_attribute(tokens.get(index + 1..).unwrap_or_default())
                    {
                        return Some(found);
                    }
                }
            }
            Token::Punct(']') => attribute = attribute.saturating_sub(1),
            Token::Punct('=') if attribute > 0 && before == Token::Ident("path") => {
                return Some(Found::PathAttribute);
            }
            Token::Ident(word) if compiles && COMPILE_TIME_READS.contains(&word) => {
                if next == Token::Punct('!') {
                    return Some(Found::Macro(word.to_owned()));
                }
                if !(next == Token::Punct(':') && after_next == Token::Punct(':')) {
                    return Some(Found::Name(word.to_owned()));
                }
            }
            Token::Ident("path") if compiles && attribute > 0 => {
                return Some(Found::PathAttribute);
            }
            Token::Punct('$') if compiles && attribute > 0 => {
                return Some(Found::MetavariableAttribute);
            }
            Token::Ident("mod") if compiles => {
                let name = match next {
                    Token::Ident(name) => Some(name.to_owned()),
                    _ => None,
                };
                if nesting.in_macro > 0 || name.is_none() || after_next != Token::Punct('{') {
                    return Some(Found::OutlineModule(name));
                }
            }
            Token::Ident(_) if opens_macro.is_some() && before == Token::Punct('!') => {
                macro_next = opens_macro;
            }
            _ => {}
        }
        nesting.track(token, opens_macro);
    }
    lexer.unterminated
}

/// The delimiters open at a point of the scan, and how many of them opened a macro's input.
#[derive(Debug, Default)]
struct Nesting {
    /// Each open delimiter, with the macro whose input or body it opened, if any.
    open: Vec<Option<MacroKind>>,
    /// How many of `open` opened a macro call's input or a `macro_rules!` body.
    in_macro: usize,
    /// How many of `open` opened a macro call's input.
    in_call: usize,
}

impl Nesting {
    /// Follows `token`: an opening delimiter opens `opens_macro`'s input or body when it is
    /// `Some`, and a closing one closes the innermost delimiter.
    fn track(&mut self, token: Token<'_>, opens_macro: Option<MacroKind>) {
        match token {
            Token::Punct('(' | '[' | '{') => {
                self.open.push(opens_macro);
                self.in_macro += usize::from(opens_macro.is_some());
                self.in_call += usize::from(opens_macro == Some(MacroKind::Call));
            }
            Token::Punct(')' | ']' | '}') => {
                let closed = self.open.pop().flatten();
                self.in_macro = self.in_macro.saturating_sub(usize::from(closed.is_some()));
                self.in_call = self
                    .in_call
                    .saturating_sub(usize::from(closed == Some(MacroKind::Call)));
            }
            _ => {}
        }
    }
}

/// What a delimiter after `name!` opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacroKind {
    /// A macro call's input, whose tokens the macro can rearrange: a `#` in it could become an
    /// attribute around any bracket group.
    Call,
    /// A `macro_rules!` body.
    Definition,
}

/// The attributes a block may use before it is compiled, by the first segment of each
/// attribute's path.
///
/// An allowlist, not a denylist: some built-in attributes read a file at compile time even when
/// rustc only emits metadata (`#[debugger_visualizer(natvis_file = "..")]`, `#[path = ".."]`),
/// and a later Rust may add more. Each of these only sets lint levels, conditions, derives,
/// documentation or codegen hints. `doc` is safe: `#[doc = include_str!(..)]` is refused by the
/// read-macro rule, and a `doc` with a literal reads nothing. `unsafe` is left out: its
/// edition-2024 wrapper `#[unsafe(..)]` hides the name of the attribute inside it.
const ALLOWED_ATTRIBUTES: [&str; 30] = [
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "cfg",
    "cfg_attr",
    "derive",
    "doc",
    "must_use",
    "inline",
    "cold",
    "track_caller",
    "repr",
    "non_exhaustive",
    "deprecated",
    "test",
    "ignore",
    "should_panic",
    "automatically_derived",
    "rustfmt",
    "clippy",
    "diagnostic",
    "macro_export",
    "macro_use",
    "no_mangle",
    "export_name",
    "global_allocator",
    "no_std",
    "no_implicit_prelude",
];

/// The first attribute in `tokens`, the tokens after an attribute's `[`, whose path's first
/// segment isn't in [`ALLOWED_ATTRIBUTES`]: the attribute itself, and each attribute a
/// `cfg_attr(<predicate>, <attribute>, ..)` in it applies, however deeply nested. A path that
/// doesn't start with a name is refused too. `None` when every one is allowed. One pass up to
/// the attribute's closing `]`.
fn unlisted_attribute(tokens: &[Token<'_>]) -> Option<Found> {
    // The delimiter depth inside the attribute.
    let mut depth = 0_usize;
    // The depth of each open `cfg_attr(..)`'s parentheses, whose top-level commas each start an
    // attribute.
    let mut cfg_attrs: Vec<usize> = Vec::new();
    let mut path_next = true;
    for (index, &token) in tokens.iter().enumerate() {
        let closes = matches!(token, Token::Punct(')' | ']' | '}'));
        if std::mem::take(&mut path_next) && !closes {
            match token {
                Token::Ident("path") => return Some(Found::PathAttribute),
                Token::Ident(name) if ALLOWED_ATTRIBUTES.contains(&name) => {
                    if name == "cfg_attr" && tokens.get(index + 1) == Some(&Token::Punct('(')) {
                        cfg_attrs.push(depth + 1);
                    }
                }
                Token::Ident(name) => return Some(Found::Attribute(name.to_owned())),
                Token::Punct('$') => return Some(Found::MetavariableAttribute),
                _ => return Some(Found::Attribute(String::new())),
            }
        }
        match token {
            Token::Punct('(' | '[' | '{') => depth += 1,
            Token::Punct(')' | ']' | '}') => {
                if cfg_attrs.last() == Some(&depth) {
                    let _closed = cfg_attrs.pop();
                }
                depth = depth.checked_sub(1)?;
            }
            Token::Punct(',') if cfg_attrs.last() == Some(&depth) => path_next = true,
            _ => {}
        }
    }
    None
}

/// One token of a Rust block, as far as the scan needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'text> {
    /// An identifier or keyword, without a raw `r#` prefix.
    Ident(&'text str),
    /// One punctuation character.
    Punct(char),
    /// A literal, a lifetime, or anything else the scan doesn't judge.
    Other,
}

/// The tokens of a Rust block, skipping whitespace and comments. Each byte is read once.
struct Tokens<'text> {
    /// The block.
    text: &'text str,
    /// The byte offset of the next unread character.
    at: usize,
    /// What the text ended inside, when it ended inside a comment or a literal.
    unterminated: Option<Found>,
}

impl<'text> Tokens<'text> {
    /// The tokens of `text`.
    const fn new(text: &'text str) -> Self {
        Self {
            text,
            at: 0,
            unterminated: None,
        }
    }

    /// Records that the text ended inside `what`.
    fn ended_inside(&mut self, what: &'static str) {
        self.at = self.text.len();
        self.unterminated = Some(Found::Unscannable(what));
    }

    /// The text from the next unread character.
    fn rest(&self) -> &'text str {
        self.text.get(self.at..).unwrap_or_default()
    }

    /// The character `skip` characters past the next unread one.
    fn peek(&self, skip: usize) -> Option<char> {
        self.rest().chars().nth(skip)
    }

    /// Moves past the next `count` characters.
    fn bump(&mut self, count: usize) {
        for c in self.rest().chars().take(count) {
            self.at += c.len_utf8();
        }
    }

    /// Moves past characters while `keep` holds.
    fn bump_while(&mut self, keep: impl Fn(char) -> bool) {
        let rest = self.rest();
        self.at += rest.find(|c: char| !keep(c)).unwrap_or(rest.len());
    }

    /// Moves past a `/* */` comment the cursor is on, nesting as Rust does, or to the end.
    fn block_comment(&mut self) {
        let mut depth = 0_usize;
        while let Some(c) = self.peek(0) {
            if self.rest().starts_with("/*") {
                depth += 1;
                self.bump(2);
            } else if self.rest().starts_with("*/") {
                self.bump(2);
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return;
                }
            } else {
                self.at += c.len_utf8();
            }
        }
        self.ended_inside("an unterminated block comment");
    }

    /// Moves past a `"` string the cursor is on, honoring `\` escapes, or to the end.
    fn string(&mut self) {
        self.bump(1);
        while let Some(c) = self.peek(0) {
            self.at += c.len_utf8();
            match c {
                '\\' => self.bump(1),
                '"' => return,
                _ => {}
            }
        }
        self.ended_inside("an unterminated string");
    }

    /// Moves past a raw string whose `r` the cursor is past, `hashes` `#`s before its `"`, or to
    /// the end. Each `#` after a `"` is read once.
    fn raw_string(&mut self, hashes: usize) {
        self.bump(hashes + 1);
        while let Some(c) = self.peek(0) {
            self.at += c.len_utf8();
            if c == '"' {
                let closing = self
                    .rest()
                    .chars()
                    .take(hashes)
                    .take_while(|c| *c == '#')
                    .count();
                self.bump(closing);
                if closing == hashes {
                    return;
                }
            }
        }
        self.ended_inside("an unterminated string");
    }

    /// The `#`s from the cursor, then whether a `"` follows them.
    fn hashes_then_quote(&self) -> (usize, bool) {
        let hashes = self.rest().chars().take_while(|c| *c == '#').count();
        (hashes, self.peek(hashes) == Some('"'))
    }
}

impl<'text> Iterator for Tokens<'text> {
    type Item = Token<'text>;

    fn next(&mut self) -> Option<Token<'text>> {
        loop {
            self.bump_while(is_rust_whitespace);
            let rest = self.rest();
            let c = rest.chars().next()?;
            if rest.starts_with("//") {
                self.bump_while(|c| c != '\n');
            } else if rest.starts_with("/*") {
                self.block_comment();
            } else if c == '"' {
                self.string();
                return Some(Token::Other);
            } else if c == '\'' {
                self.quote();
                return Some(Token::Other);
            } else if is_ident_char(c) {
                return Some(self.word());
            } else {
                self.bump(1);
                return Some(Token::Punct(c));
            }
        }
    }
}

impl<'text> Tokens<'text> {
    /// Reads a character literal or a lifetime the cursor is on: `'\..'` and `'x'` are
    /// literals, and `'` before an ASCII identifier is a lifetime or label. Anything else is an
    /// unterminated character literal.
    fn quote(&mut self) {
        self.bump(1);
        if self.peek(0) == Some('\\') {
            self.bump(2);
            self.bump_while(|c| c != '\'' && c != '\n');
            if self.peek(0) == Some('\'') {
                self.bump(1);
            } else {
                self.ended_inside("an unterminated character literal");
            }
        } else if self.peek(1) == Some('\'') {
            self.bump(2);
        } else if self.peek(0).is_some_and(is_ident_char) {
            self.bump_while(is_ident_char);
        } else {
            self.ended_inside("an unterminated character literal");
        }
    }

    /// Reads an identifier, a raw identifier, a prefixed or raw string, or a number the cursor is
    /// on.
    fn word(&mut self) -> Token<'text> {
        let start = self.at;
        self.bump_while(is_ident_char);
        let word = self.text.get(start..self.at).unwrap_or_default();
        if word.starts_with(|c: char| c.is_ascii_digit()) {
            return Token::Other;
        }
        let raw_prefix = matches!(word, "r" | "br" | "cr");
        let (hashes, quote) = self.hashes_then_quote();
        if raw_prefix && quote {
            self.raw_string(hashes);
            return Token::Other;
        }
        if matches!(word, "b" | "c") && self.peek(0) == Some('"') {
            self.string();
            return Token::Other;
        }
        if matches!(word, "b") && self.peek(0) == Some('\'') {
            self.quote();
            return Token::Other;
        }
        if word == "r" && hashes == 1 && self.peek(1).is_some_and(is_ident_char) {
            self.bump(1);
            let start = self.at;
            self.bump_while(is_ident_char);
            return Token::Ident(self.text.get(start..self.at).unwrap_or_default());
        }
        Token::Ident(word)
    }
}

/// Whether `c` can be part of an identifier or number, by the scan's ASCII-only rule: any other
/// character ends a word, which can only split words and so only add matches.
const fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether rustc skips `c` as whitespace: [`char::is_whitespace`] plus the left-to-right and
/// right-to-left marks, U+200E and U+200F.
fn is_rust_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{200E}' | '\u{200F}')
}

/// rustfmt's syntax-error report: stderr opening with `error:` and, when it can place it, a
/// `--> <stdin>:L:C` line. `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    stderr.starts_with("error:").then(|| ParseError {
        line: stderr
            .lines()
            .find_map(|line| line.split_once("-->").map(|(_, at)| first_line(at)))
            .flatten(),
    })
}

#[cfg(test)]
mod tests {

    use std::time::{Duration, Instant};

    use super::{
        EmitDir, Found, LineMap, MAX_SCAN_BYTES, Scan, attribute_edition, before_compiling,
        clippy_args, clippy_conf_dir, compile_time_read, config_args, format_args, lint_verdict,
        parse_error, scan, unhide,
    };
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::code_blocks::block::blocks_in;
    use crate::code_blocks::format::ParseError;
    use crate::code_blocks::lint::{Diagnostic, Verdict};
    use crate::code_blocks::runner::Ran;
    use crate::code_blocks::toolchain::cargo_lints;
    use crate::config::code_blocks::Edition;
    use crate::finding::Location;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::SourceFile;

    #[test]
    fn a_hidden_line_is_checked_without_its_prefix_and_no_line_is_added() {
        let (checked, _) = unhide("# fn main() {\nlet x = 1;\n#\n# }\n");

        assert_eq!(checked, "fn main() {\nlet x = 1;\n\n}\n");
    }

    #[test]
    fn a_double_hash_is_a_literal_hash_and_is_written_back_escaped() {
        let (checked, hidden) = unhide("## not hidden\n##\n");

        assert_eq!(checked, "# not hidden\n#\n");
        assert_eq!(
            hidden.rehide("# not hidden\n#"),
            Ok("## not hidden\n##".to_owned())
        );
    }

    #[test]
    fn an_indented_hidden_line_keeps_its_indentation_and_is_written_back_as_written() {
        let block = "# fn main() {\n    # let x = 1;\n    #\nlet y=x;\n# }\n";

        let (checked, hidden) = unhide(block);

        assert_eq!(checked, "fn main() {\n    let x = 1;\n\nlet y=x;\n}\n");
        assert_eq!(
            hidden.rehide("fn main() {\n    let x = 1;\n\n    let y = x;\n}"),
            Ok("# fn main() {\n    # let x = 1;\n    #\n    let y = x;\n# }".to_owned())
        );
    }

    #[test]
    fn an_escaped_line_keeps_its_escape_and_an_unescaped_twin_stays_unescaped() {
        let block = "#[derive(Debug)]\nstruct S;\n##[derive(Debug)]\nstruct  T;\n";

        let (checked, hidden) = unhide(block);

        assert_eq!(
            checked,
            "#[derive(Debug)]\nstruct S;\n#[derive(Debug)]\nstruct  T;\n"
        );
        assert_eq!(
            hidden.rehide("#[derive(Debug)]\nstruct S;\n#[derive(Debug)]\nstruct T;"),
            Ok("#[derive(Debug)]\nstruct S;\n##[derive(Debug)]\nstruct T;".to_owned())
        );
    }

    #[test]
    fn attributes_and_inner_attributes_are_not_hidden() {
        let block = "#![allow(unused)]\n#[derive(Debug)]\nstruct S;\n";

        let (checked, hidden) = unhide(block);

        assert_eq!(checked, block);
        assert_eq!(
            hidden.rehide(block.trim_end()),
            Ok(block.trim_end().to_owned())
        );
    }

    #[test]
    fn formatter_output_gets_each_hidden_lines_prefix_back() {
        let (checked, hidden) = unhide("# fn main() {\nlet x=1;\n# }\n");
        assert_eq!(checked, "fn main() {\nlet x=1;\n}\n");

        let written = hidden.rehide("fn main() {\n    let x = 1;\n}");

        assert_eq!(written, Ok("# fn main() {\n    let x = 1;\n# }".to_owned()));
    }

    #[test]
    fn hidden_lines_that_cant_be_matched_one_to_one_are_refused() {
        let (_, hidden) = unhide("# use std::fmt;\n# fn main() {\nlet x=1;\n# }\n");

        for (output, first, why) in [
            (
                "fn main() {\n    let x = 1;\n}",
                0,
                "a hidden line was removed",
            ),
            (
                "use std::fmt;\nfn main() {\n    let x = 1; }",
                3,
                "a hidden line's text changed",
            ),
            (
                "fn main() {\nuse std::fmt;\n    let x = 1;\n}",
                1,
                "two hidden lines swapped",
            ),
            (
                "use std::fmt;\nfn main() {\n    let x = || {};\n}\n}",
                3,
                "a shown `}` makes the hidden `}` ambiguous",
            ),
        ] {
            assert_eq!(hidden.rehide(output), Err(first), "{why}: {output:?}");
        }
    }

    #[test]
    fn the_line_map_points_each_checked_line_at_its_doc_line() {
        let text = "# Doc\n\n- item\n\n  ```rust\n  # fn main() {\n  let x = 1;\n  # }\n  ```\n";
        let path = AbsolutePath::new(std::path::Path::new("/repo/docs/a.md"))
            .expect("the fixture path is absolute");
        let file = SourceFile::parse(path, text.to_owned(), cartridge())
            .expect("tree-sitter should return a tree");
        let blocks = blocks_in(&file, |range| {
            let source = "docs/a.md"
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        });
        let block = blocks.first().expect("the doc holds one block");

        let map = LineMap::of(block, text).expect("the block lies inside its doc");

        let lines: Vec<(u32, u32)> = (0..3)
            .map(|line| {
                let at = map.doc_line(line).expect("each body line is mapped");
                (at.line, at.column)
            })
            .collect();
        assert_eq!(lines, [(6, 3), (7, 3), (8, 3)]);
        assert_eq!(map.doc_line(3), None, "the block has three lines");
    }

    #[test]
    fn rustfmt_reads_the_block_as_2024_and_prints_it() {
        assert_eq!(
            format_args(Edition::E2024),
            ["--edition", "2024", "--emit", "stdout"]
        );
    }

    #[test]
    fn a_block_attribute_names_its_edition() {
        assert_eq!(
            attribute_edition(&["no_run", "edition2018"]),
            Some(Edition::E2018)
        );
        assert_eq!(attribute_edition(&["edition2030", "ignore"]), None);
    }

    /// A refusal for a call of `name!`.
    fn read_of(name: &str) -> Scan {
        Scan::Read(Found::Macro(name.to_owned()))
    }

    #[test]
    fn compile_time_reads_are_found_however_they_are_spaced() {
        for (block, read) in [
            (
                "const S: &str = include_str!(\"/etc/passwd\");\n",
                read_of("include_str"),
            ),
            ("fn f() { let _ = std::env!(\"HOME\"); }\n", read_of("env")),
            (
                "fn f() { option_env /* x */ !(\"A\"); }\n",
                read_of("option_env"),
            ),
            ("include_bytes\n!(\"x\");\n", read_of("include_bytes")),
            ("r#include!(\"x\");\n", read_of("include")),
            (
                "const S: &str = \u{b2}include_str!(\"x\");\n",
                read_of("include_str"),
            ),
            ("const S: &str = \u{e9}env!(\"x\");\n", read_of("env")),
            ("const S: &str = env\u{200E}!(\"x\");\n", read_of("env")),
            ("const S: &str = env\u{200F}!(\"x\");\n", read_of("env")),
            (
                "#[path = \"/outside.rs\"]\nmod m;\n",
                Scan::Read(Found::PathAttribute),
            ),
            (
                "# [ cfg_attr(all(), path=\"/o.rs\") ]\nmod m;\n",
                Scan::Read(Found::PathAttribute),
            ),
            (
                "#\u{200E}[path = \"/o.rs\"]\nmod m;\n",
                Scan::Read(Found::PathAttribute),
            ),
        ] {
            assert_eq!(compile_time_read(block), read, "{block:?}");
        }
    }

    #[test]
    fn what_the_scan_cant_read_past_fails_closed() {
        for (block, what) in [
            (
                "fn f() {}\n/* env!(\"A\")\n",
                "an unterminated block comment",
            ),
            ("const S: &str = \"env!(\"A\");\n", "an unterminated string"),
            ("const S: &str = r#\"x\";\n", "an unterminated string"),
            (
                "const C: char = '\"; env!(\"A\");\n",
                "an unterminated character literal",
            ),
            (
                "const C: char = '\\u{41};\n",
                "an unterminated character literal",
            ),
            ("const C: char = '", "an unterminated character literal"),
            (
                "#!/usr/bin/env rust\nfn main() {}\n",
                "a `#!` first line that isn't an inner attribute",
            ),
            (
                "#! [allow(unused)]\nfn main() {}\n",
                "a `#!` first line that isn't an inner attribute",
            ),
        ] {
            assert_eq!(
                compile_time_read(block),
                Scan::Read(Found::Unscannable(what)),
                "{block:?}"
            );
        }
        assert_eq!(
            compile_time_read("#![allow(unused)]\nfn main() {}\n"),
            Scan::Clean
        );
    }

    #[test]
    fn ordinary_uses_of_the_same_words_are_not_reads() {
        for block in [
            "fn main() { let path = std::env::args(); }\n",
            "use std::env;\nfn f() -> String { env::var(\"A\").unwrap_or_default() }\n",
            "#[serde(rename = \"path\")]\nstruct S;\n",
            "fn environment() {}\nmacro_rules! my_env { () => {} }\nmy_env!();\n",
            "fn f<'a>(x: &'a str) -> &'a str { 'outer: loop { break 'outer x; } }\n",
        ] {
            assert_eq!(compile_time_read(block), Scan::Clean, "{block:?}");
        }
    }

    #[test]
    fn reads_in_strings_and_comments_are_not_code_and_cant_hide_code() {
        for block in [
            "const S: &str = \"env!(x) #[path = y]\";\n",
            "const S: &str = r#\"include!(\"x\")\"#;\n// env!(\"A\")\n/* include_str!(\"b\") */\n",
            "fn f(c: char) -> bool { c == '\"' || c == '\\'' }\n",
        ] {
            assert_eq!(compile_time_read(block), Scan::Clean, "{block:?}");
        }
        let after_a_string = "const S: &str = \"/*\"; const T: &str = env!(\"HOME\"); // */\n";
        assert_eq!(compile_time_read(after_a_string), read_of("env"));
    }

    #[test]
    fn a_block_over_the_cap_is_too_large_to_scan() {
        let block = "x".repeat(MAX_SCAN_BYTES + 1);

        assert_eq!(compile_time_read(&block), Scan::TooLarge);
    }

    #[test]
    fn pathological_blocks_scan_in_one_linear_pass() {
        let limit = Duration::from_secs(1);
        for (name, block) in [
            ("200k `#[`", "#[".repeat(200_000)),
            ("100k `env /*`", "env /*".repeat(100_000)),
            ("100k `include /* */`", "include /* */ ".repeat(100_000)),
            ("100k raw strings", "r#\"\"".repeat(100_000)),
        ] {
            let started = Instant::now();
            let _found = scan(&block, true);
            let took = started.elapsed();

            assert!(took < limit, "{name} took {took:?}, over {limit:?}");
        }
    }

    #[test]
    fn an_in_root_rustfmt_toml_is_passed_by_its_real_path() {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        std::fs::write(root.path().join("rustfmt.toml"), "max_width = 80\n")
            .expect("the config should be writable");
        let real = root
            .path()
            .canonicalize()
            .expect("the root should resolve")
            .join("rustfmt.toml");

        assert_eq!(
            config_args(root.path()),
            [
                std::ffi::OsString::from("--config-path"),
                real.into_os_string()
            ]
        );
    }

    #[test]
    fn a_rustfmt_toml_linked_outside_the_root_is_not_passed() {
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let target = outside.path().join("rustfmt.toml");
        std::fs::write(&target, "max_width = 80\n").expect("the config should be writable");
        std::os::unix::fs::symlink(&target, root.path().join(".rustfmt.toml"))
            .expect("the link should be creatable");

        assert_eq!(config_args(root.path()), Vec::<std::ffi::OsString>::new());
    }

    #[test]
    fn an_error_report_with_a_position_is_a_parse_error_at_that_line() {
        assert_eq!(
            parse_error("error: expected one of `;`\n --> <stdin>:3:9\n  |\n"),
            Some(ParseError { line: Some(2) })
        );
    }

    #[test]
    fn an_error_report_without_a_position_is_a_parse_error_without_a_line() {
        assert_eq!(
            parse_error("error: unexpected closing delimiter\n"),
            Some(ParseError { line: None })
        );
    }

    #[test]
    fn stderr_not_opening_with_error_is_not_a_parse_error() {
        assert_eq!(parse_error("warning: x\n"), None);
    }

    #[test]
    fn clippy_gets_the_block_on_stdin_with_its_edition_metadata_path_and_relaxations_last() {
        let lints = ["-W".into(), "clippy::pedantic".into()];

        let args = clippy_args(
            Edition::E2021,
            Some(std::path::Path::new("/c/emit/1-0/block.rmeta")),
            &lints,
        );

        assert_eq!(
            args,
            [
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--emit=metadata=/c/emit/1-0/block.rmeta",
                "--error-format=json",
                "-W",
                "clippy::pedantic",
                "-A",
                "unused",
                "-A",
                "dead_code",
                "-A",
                "missing_docs",
                "-A",
                "clippy::missing_docs_in_private_items",
                "-",
            ]
        );
        for refused in ["--extern", "-L", "--cfg"] {
            assert!(!args.iter().any(|arg| arg == refused), "{args:?}");
        }
        assert!(
            clippy_args(Edition::E2021, None, &lints).contains(&"--emit=metadata".into()),
            "a cache key names no metadata path"
        );
    }

    /// A temporary root holding `Cargo.toml` with `text`.
    fn root_with_cargo_toml(text: &str) -> tempfile::TempDir {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        std::fs::write(root.path().join("Cargo.toml"), text)
            .expect("the manifest should be writable");
        root
    }

    #[test]
    fn project_lints_go_groups_first_by_priority_without_clippy_cargo() {
        let root = root_with_cargo_toml(
            "[workspace.lints.rust]\nunsafe_code = \"forbid\"\nunused = \"warn\"\n\
             [workspace.lints.clippy]\nptr_arg = \"deny\"\n\
             pedantic = { level = \"warn\", priority = -1 }\n\
             cargo = { level = \"warn\", priority = -1 }\n\
             [workspace.lints.rustdoc]\nbroken_intra_doc_links = \"deny\"\n",
        );

        let flags: Vec<String> = cargo_lints(root.path())
            .iter()
            .map(|flag| flag.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            flags,
            [
                "-W",
                "clippy::pedantic",
                "-W",
                "unused",
                "-D",
                "clippy::ptr_arg",
                "-F",
                "unsafe_code"
            ]
        );
    }

    #[test]
    fn a_cargo_toml_linked_outside_the_root_gives_no_lints() {
        let outside = root_with_cargo_toml("[lints.clippy]\nptr_arg = \"deny\"\n");
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        std::os::unix::fs::symlink(
            outside.path().join("Cargo.toml"),
            root.path().join("Cargo.toml"),
        )
        .expect("the link should be creatable");

        assert_eq!(cargo_lints(root.path()), Vec::<std::ffi::OsString>::new());
    }

    #[test]
    fn clippy_conf_dir_is_the_root_only_for_in_root_clippy_configs() {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        assert_eq!(clippy_conf_dir(root.path()), None, "no clippy config");

        std::fs::write(root.path().join("clippy.toml"), "msrv = \"1.80\"\n")
            .expect("the config should be writable");
        let real = root.path().canonicalize().expect("the root should resolve");
        assert_eq!(clippy_conf_dir(root.path()), Some(real.into_os_string()));

        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        std::fs::write(outside.path().join("clippy.toml"), "")
            .expect("the config should be writable");
        std::os::unix::fs::symlink(
            outside.path().join("clippy.toml"),
            root.path().join(".clippy.toml"),
        )
        .expect("the link should be creatable");
        assert_eq!(
            clippy_conf_dir(root.path()),
            None,
            "a `.clippy.toml` linked outside wins clippy's lookup, so none is passed"
        );
    }

    /// One `clippy-driver --error-format=json` line.
    fn rustc_line(level: &str, code: Option<&str>, spans: &str) -> String {
        let code = code.map_or("null".to_owned(), |code| {
            format!("{{\"code\":\"{code}\",\"explanation\":null}}")
        });
        format!(
            "{{\"$message_type\":\"diagnostic\",\"message\":\"m\",\"code\":{code},\"level\":\"{level}\",\"spans\":[{spans}],\"children\":[],\"rendered\":\"r\"}}\n"
        )
    }

    /// A span in `file` at `line` and `column`.
    fn span(file: &str, primary: bool, line: usize, column: usize) -> String {
        format!(
            "{{\"file_name\":\"{file}\",\"byte_start\":0,\"byte_end\":1,\"line_start\":{line},\"line_end\":{line},\"column_start\":{column},\"column_end\":{column},\"is_primary\":{primary},\"text\":[],\"label\":null,\"suggested_replacement\":null,\"suggestion_applicability\":null,\"expansion\":null}}"
        )
    }

    #[test]
    fn clippy_diagnostics_take_their_primary_block_span_and_summaries_are_left_out() {
        let stderr = [
            rustc_line(
                "warning",
                Some("clippy::ptr_arg"),
                &format!(
                    "{},{}",
                    span("<anon>", false, 9, 9),
                    span("<anon>", true, 2, 13)
                ),
            ),
            rustc_line("error", Some("E0425"), &span("/rustc/lib.rs", true, 7, 1)),
            rustc_line("warning", None, ""),
            rustc_line("error", None, ""),
            rustc_line("note", Some("x"), &span("<anon>", true, 1, 1)),
            "not json\n".to_owned(),
        ]
        .concat();

        let verdict = lint_verdict(Ok(Ran {
            success: true,
            stdout: Vec::new(),
            stderr: stderr.into_bytes(),
        }));

        assert_eq!(
            verdict,
            Verdict::Diagnostics(vec![
                Diagnostic {
                    line: 2,
                    column: 13,
                    rule: "clippy::ptr_arg".to_owned(),
                    message: "m".to_owned(),
                },
                Diagnostic {
                    line: 0,
                    column: 0,
                    rule: "E0425".to_owned(),
                    message: "m".to_owned(),
                },
            ]),
            "a warning counts though clippy exits 0; a span outside the block goes to the fence"
        );
        let summaries_only = rustc_line("warning", None, "");
        assert_eq!(
            lint_verdict(Ok(Ran {
                success: true,
                stdout: Vec::new(),
                stderr: summaries_only.into_bytes(),
            })),
            Verdict::Clean
        );
    }

    #[test]
    fn a_diagnostic_on_a_hidden_line_shifts_past_its_marker() {
        let (_, hidden) = unhide("# use std::fmt;\n    # let x = 1;\n## escaped\nlet y = 2;\n#\n");

        let removed: Vec<usize> = (0..5).map(|line| hidden.removed(line)).collect();

        assert_eq!(removed, [2, 2, 1, 0, 1]);
    }

    #[test]
    fn the_emit_dir_is_beside_the_working_directory_and_gone_after_the_run() {
        let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let neutral = cache.path().join("cwd");
        std::fs::create_dir(&neutral).expect("the cwd should be creatable");

        let (first, second) = (
            EmitDir::create(&neutral).expect("the emit dir should be creatable"),
            EmitDir::create(&neutral).expect("a second run gets its own"),
        );
        let file = first.file();
        std::fs::write(&file, "meta").expect("the metadata should be writable");
        let dir = file.parent().expect("the file is in the dir").to_path_buf();

        assert_ne!(
            first.file(),
            second.file(),
            "parallel runs never share a path"
        );
        assert!(!dir.starts_with(&neutral), "{dir:?} is inside the cwd");
        drop(first);
        assert!(!dir.exists(), "{dir:?} should be removed with its metadata");
        assert_eq!(
            std::fs::read_dir(&neutral)
                .expect("the cwd should be readable")
                .count(),
            0,
            "the cwd stays empty"
        );
    }

    #[test]
    fn before_compiling_renames_macro_names_and_metavariable_attributes_are_refused() {
        let name = |word: &str| Scan::Read(Found::Name(word.to_owned()));
        for (block, refused) in [
            (
                "use std::include_str as inc;\nconst S: &str = inc!(\"/etc/passwd\");\n",
                name("include_str"),
            ),
            ("use std::env;\n", name("env")),
            ("use std::{env as e};\n", name("env")),
            (
                "use core::prelude::v1::option_env as o;\n",
                name("option_env"),
            ),
            (
                "macro_rules! m { ($i:ident) => { $i!(\"/etc/passwd\") }; }\nconst S: &str = m!(include_str);\n",
                name("include_str"),
            ),
            (
                "const S: &str = r#include_bytes!(\"x\");\n",
                Scan::Read(Found::Macro("include_bytes".to_owned())),
            ),
            (
                "macro_rules! m { ($p:literal) => { #[doc = $p] fn f() {} }; }\n",
                Scan::Read(Found::MetavariableAttribute),
            ),
            (
                "macro_rules! m { ($($a:tt)*) => { #[$($a)*] mod x {} }; }\n",
                Scan::Read(Found::MetavariableAttribute),
            ),
            (
                "#[cfg_attr(all(), path)]\nmod x {}\n",
                Scan::Read(Found::PathAttribute),
            ),
        ] {
            assert_eq!(before_compiling(block), refused, "{block:?}");
        }
    }

    #[test]
    fn before_compiling_outline_modules_are_refused_however_they_are_written() {
        let outline =
            |name: Option<&str>| Scan::Read(Found::OutlineModule(name.map(str::to_owned)));
        for (block, refused) in [
            ("mod helpers;\n", outline(Some("helpers"))),
            (
                "#[cfg(test)]\npub(crate) mod tests ;\n",
                outline(Some("tests")),
            ),
            (
                "macro_rules! m { ($n:ident) => { mod $n; }; }\nm!(x);\n",
                outline(None),
            ),
            (
                "macro_rules! m { ($k:tt $n:ident $b:tt) => { $k $n; }; }\nm!(mod x {});\n",
                outline(Some("x")),
            ),
            (
                "macro_rules! m { () => { mod x {} }; }\n",
                outline(Some("x")),
            ),
        ] {
            assert_eq!(before_compiling(block), refused, "{block:?}");
        }
        assert_eq!(
            compile_time_read("mod helpers;\nuse std::env;\n"),
            Scan::Clean,
            "rustfmt expands nothing, so its scan keeps the old rules"
        );
    }

    #[test]
    fn before_compiling_passes_ordinary_blocks() {
        for block in [
            "fn main() { let _ = std::env::args(); }\n",
            "fn f() -> Option<String> { std::env::var(\"A\").ok() }\n",
            "mod inline {\n    pub fn f() {}\n}\nfn main() { inline::f(); println!(\"{}\", 1); }\n",
            "#[derive(Debug)]\nstruct S { path: String }\n",
            "fn environment() -> bool { true != false }\n",
            "#[expect(clippy::ptr_arg, reason = \"x\")]\npub fn f(v: &Vec<i32>) {}\n",
        ] {
            assert_eq!(before_compiling(block), Scan::Clean, "{block:?}");
        }
    }

    #[test]
    fn before_compiling_attributes_off_the_allowlist_are_refused() {
        let unlisted = |name: &str| Scan::Read(Found::Attribute(name.to_owned()));
        for (block, refused) in [
            (
                "#![debugger_visualizer(natvis_file = \"/etc/passwd\")]\n",
                unlisted("debugger_visualizer"),
            ),
            (
                "#![debugger_visualizer(gdb_script_file = \"/etc/passwd\")]\n",
                unlisted("debugger_visualizer"),
            ),
            (
                "#[link_section = \".x\"]\npub fn f() {}\n",
                unlisted("link_section"),
            ),
            (
                "#![cfg_attr(all(), debugger_visualizer(natvis_file = \"/etc/passwd\"))]\n",
                unlisted("debugger_visualizer"),
            ),
            (
                "#[cfg_attr(all(), allow(unused), cfg_attr(all(), link_section = \".x\"))]\nfn f() {}\n",
                unlisted("link_section"),
            ),
            ("#[::core::prelude::v1::test]\nfn f() {}\n", unlisted("")),
        ] {
            assert_eq!(before_compiling(block), refused, "{block:?}");
        }
    }

    #[test]
    fn before_compiling_allowlisted_attributes_pass() {
        let block = "#![allow(unused)]\n#![cfg_attr(all(), warn(clippy::pedantic),)]\n\
                     #[derive(Debug, Clone)]\n#[must_use]\n#[repr(C)]\n#[non_exhaustive]\n\
                     #[doc = \"text\"]\n#[cfg_attr(test, derive(Default), deprecated(note = \"x\"))]\n\
                     pub struct S {\n    #[allow(dead_code)]\n    x: [u8; 2],\n}\n\
                     #[rustfmt::skip]\n#[inline]\n#[track_caller]\npub fn f() {}\n\
                     #[diagnostic::on_unimplemented(message = \"x\")]\npub trait T {}\n";

        assert_eq!(before_compiling(block), Scan::Clean);
    }

    #[test]
    fn before_compiling_a_hash_a_macro_could_pair_with_any_bracket_group_is_refused() {
        for (block, what) in [
            (
                "macro_rules! m { ($h:tt $a:tt $b:tt) => { $h $b fn f() {} }; }\n\
                 m!(#[allow(unused)] [debugger_visualizer(natvis_file = \"/etc/passwd\")]);\n",
                "a `#` inside a macro call's input",
            ),
            (
                "macro_rules! m { ($b:tt) => { # $b fn f() {} }; }\n\
                 m!([debugger_visualizer(natvis_file = \"/etc/passwd\")]);\n",
                "a `#` that doesn't start an attribute",
            ),
        ] {
            assert_eq!(
                before_compiling(block),
                Scan::Read(Found::Unscannable(what)),
                "{block:?}"
            );
        }
    }

    #[test]
    fn before_compiling_macro_export_passes_and_an_unsafe_wrapper_is_refused() {
        assert_eq!(
            before_compiling("#[macro_export]\nmacro_rules! m { () => {}; }\n"),
            Scan::Clean
        );
        assert_eq!(
            before_compiling("#[unsafe(no_mangle)]\npub fn f() {}\n"),
            Scan::Read(Found::Attribute("unsafe".to_owned()))
        );
    }
}
