//! What Rust's tools need beyond their cartridges: the project's config and edition, the scans
//! that refuse a block before it runs, and the readers of rustfmt's and clippy's output.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::code_blocks::Edition;
use crate::config::confine::{self, Boundary};
use crate::finding::{IndexedText, Location};
use crate::path::AbsolutePath;

use super::block::CodeBlock;
use super::format::{self, ParseError, first_line};
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};

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
    /// The shared `emit` directory `path` sits in, which removing `path` is admitted against.
    base: Boundary,
    /// The directory, owned by this run alone.
    path: PathBuf,
}

/// How many [`EmitDir`]s this process has tried to create, which makes each name unique.
static EMITS: AtomicUsize = AtomicUsize::new(0);

impl EmitDir {
    /// A new directory `emit/<pid>-<n>` beside `neutral`, the tool's working directory, created
    /// mode 0700 inside a 0700 `emit` directory, each admitted beneath the directory it goes in.
    /// `None` when `neutral` doesn't resolve, a directory isn't admitted, or it can't be created.
    ///
    /// Effect: creates directories.
    #[must_use]
    pub(crate) fn create(neutral: &Path) -> Option<Self> {
        let cache = runner::exact(neutral.canonicalize().ok()?.parent()?)?;
        let parent = AbsolutePath::new(&cache.as_path().join("emit"))?;
        let base = runner::create_private(&Boundary::resolve(&cache), &parent)?;
        (0..16).find_map(|_| {
            let count = EMITS.fetch_add(1, Ordering::Relaxed);
            let path = parent
                .as_path()
                .join(format!("{}-{count}", std::process::id()));
            confine::create_dir(&base.admit(&path).ok()?, 0o700).ok()?;
            Some(Self {
                base: base.clone(),
                path,
            })
        })
    }

    /// The metadata file clippy writes in this directory.
    #[must_use]
    pub(crate) fn file(&self) -> PathBuf {
        self.path.join("block.rmeta")
    }
}

impl Drop for EmitDir {
    /// Removes the run's directory, once it is admitted beneath the shared `emit` directory.
    fn drop(&mut self) {
        if let Ok(dir) = self.base.admit(&self.path) {
            let _removed = confine::remove_dir_all(&dir);
        }
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
    /// Its last line.
    line_end: usize,
}

/// The diagnostics in `clippy-driver --error-format=json`'s `stderr`: each JSON line at level
/// `error` or `warning`, at its primary span in the block, or at line 0 (the fence) when it has
/// none there. The summaries rustc closes with, which have neither a code nor a span, are left
/// out, as is any line that isn't a diagnostic. An error with no code is a syntax error: rustc
/// gives every other error and every clippy lint a code. When any is a missing name
/// ([`lint::missing_name`]), only the missing names remain, one per distinct name at its first
/// span: the rest are knock-ons of an error that stops clippy's lints. In a run over a framed
/// block (`framed`), a syntax error wins over both: when any is reported, only the syntax errors
/// remain, since rustc recovers past many of them and reports names the broken code then misses.
fn rustc_diagnostics(stderr: &[u8], framed: bool) -> Vec<Diagnostic> {
    let found = all_rustc_diagnostics(stderr);
    if framed && found.iter().any(|found| found.syntax) {
        return found.into_iter().filter(|found| found.syntax).collect();
    }
    if found
        .iter()
        .all(|found| lint::missing_name(found).is_none())
    {
        return found;
    }
    let mut seen = std::collections::HashSet::new();
    found
        .into_iter()
        .filter(|found| lint::missing_name(found).is_some_and(|name| seen.insert(name.to_owned())))
        .collect()
}

/// Every diagnostic [`rustc_diagnostics`] reads, before it drops knock-ons.
fn all_rustc_diagnostics(stderr: &[u8]) -> Vec<Diagnostic> {
    stderr
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<RustcDiagnostic>(line).ok())
        .filter(|found| matches!(found.level.as_str(), "error" | "warning"))
        .filter(|found| found.code.is_some() || !found.spans.is_empty())
        .map(|found| {
            let (line, column, end_line) = found
                .spans
                .iter()
                .find(|span| span.is_primary && span.file_name == "<anon>")
                .map_or((0, 0, 0), |span| {
                    (span.line_start, span.column_start, span.line_end)
                });
            Diagnostic {
                line,
                column,
                end_line,
                syntax: found.code.is_none() && found.level == "error",
                rule: found.code.map_or(found.level, |code| code.code),
                message: found.message,
            }
        })
        .collect()
}

/// The name each of clippy's frames declares that a block doesn't (see `CLIPPY_FRAGMENTS` in
/// [`super::tools`]), by the start of the line the frame opens with, each with what a message
/// about a block in that frame says in its place: the members frame's struct, which the block's
/// methods know as `Self`, and the statements and expression frames' function.
const FRAME_NAMES: [(&str, &str, &str); 2] = [
    (
        "struct SupdocFragment; impl SupdocFragment {",
        "SupdocFragment",
        "Self",
    ),
    (
        "fn supdoc_fragment(",
        "supdoc_fragment",
        "<the function supdoc adds>",
    ),
];

/// The name the frame `checked` opens with declares, and what a message says in its place.
/// `None` when `checked`, the text clippy read, opens with none of clippy's frames.
fn frame_name(checked: &str) -> Option<(&'static str, &'static str)> {
    let first = checked.lines().next()?;
    FRAME_NAMES
        .iter()
        .find(|(opens, ..)| first.starts_with(opens))
        .map(|(_, name, shown)| (*name, *shown))
}

/// `text` with each whole identifier `name` replaced by `shown`. An occurrence led or followed
/// by an identifier character is part of another name, such as `supdoc_fragment_total`, and
/// stays.
fn replace_identifier(text: &str, name: &str, shown: &str) -> String {
    let in_identifier = |c: char| c.is_alphanumeric() || c == '_';
    let mut replaced = String::with_capacity(text.len());
    let mut copied = 0;
    for (at, _) in text.match_indices(name) {
        let end = at + name.len();
        let before = text.get(..at).and_then(|head| head.chars().next_back());
        let after = text.get(end..).and_then(|tail| tail.chars().next());
        if before.is_some_and(in_identifier) || after.is_some_and(in_identifier) {
            continue;
        }
        replaced.push_str(text.get(copied..at).unwrap_or_default());
        replaced.push_str(shown);
        copied = end;
    }
    replaced.push_str(text.get(copied..).unwrap_or_default());
    replaced
}

/// `text`, a message clippy wrote about a block in a frame that declares `name` (see
/// [`frame_name`]), with that name, as a whole identifier, replaced by what it stands for to the
/// block. `text` as it is when the frame declares no name.
fn without_frame_name(text: &str, name: Option<(&str, &str)>) -> String {
    name.map_or_else(
        || text.to_owned(),
        |(name, shown)| replace_identifier(text, name, shown),
    )
}

/// Whether `line`, from a rendered diagnostic, is one of rustc's suggestion lines: a line number
/// then `~`, `+` or `-` in place of the gutter's `|`, quoting the source as a fix would change
/// it.
fn suggestion_line(line: &str) -> bool {
    let line = line.trim_start();
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    digits > 0
        && line.get(digits..).is_some_and(|rest| {
            let rest = rest.trim_start_matches(' ');
            rest.starts_with(['~', '+', '-'])
                && rest
                    .get(1..)
                    .is_none_or(|tail| tail.is_empty() || tail.starts_with(' '))
        })
}

/// `stderr` of `clippy-driver --error-format=json` as a person reads it: each diagnostic's
/// `rendered` text in place of its JSON line, any other line as it is. A frame line shows in a
/// rendered source listing as a line of its own, where [`super::fragment::Frame::hidden`] finds
/// it, never inside an escaped JSON string. rustc's suggestion lines ([`suggestion_line`]) are
/// left out: they quote frame lines changed, which no frame line matches.
fn rendered(stderr: &[u8]) -> Vec<u8> {
    /// The one field of a diagnostic this reads.
    #[derive(serde::Deserialize)]
    struct Rendered {
        /// The diagnostic as rustc prints it without `--error-format=json`.
        rendered: String,
    }
    stderr
        .split(|byte| *byte == b'\n')
        .map(|line| {
            serde_json::from_slice::<Rendered>(line).map_or_else(
                |_| line.to_vec(),
                |found| {
                    found
                        .rendered
                        .trim_end()
                        .lines()
                        .filter(|line| !suggestion_line(line))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into_bytes()
                },
            )
        })
        .collect::<Vec<_>>()
        .join(&b'\n')
}

/// Judges a clippy run: its diagnostics on stderr, whatever its exit (clippy exits 0 on
/// warnings); clean on exit 0 with none; a tool failure on a non-zero exit with none, output
/// past the cap or a failed start; a timeout is a timeout.
///
/// When `input`, the checked text, frames the block (`framed`), a syntax error wins over every
/// other diagnostic (see [`rustc_diagnostics`]), no diagnostic message names the name the frame
/// declares ([`frame_name`]), and a tool failure's detail is the rendered text of stderr's
/// diagnostics, that name replaced, so the lint step can hide its frame lines.
#[must_use]
pub(crate) fn lint_verdict(
    input: &str,
    framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    let name = frame_name(input).filter(|_| framed);
    match result {
        Ok(ran) => {
            let mut found = rustc_diagnostics(&ran.stderr, framed);
            for diagnostic in &mut found {
                diagnostic.message = without_frame_name(&diagnostic.message, name);
            }
            if !found.is_empty() {
                lint::Verdict::Diagnostics(found)
            } else if ran.success {
                lint::Verdict::Clean
            } else if framed {
                let detail = format::exit_detail(&rendered(&ran.stderr));
                lint::Verdict::Failed(format::Verdict::ToolFailed(without_frame_name(
                    &detail, name,
                )))
            } else {
                lint::Verdict::Failed(format::Verdict::ToolFailed(format::exit_detail(
                    &ran.stderr,
                )))
            }
        }
        Err(failure) => lint::Verdict::Failed(format::failed(failure)),
    }
}

/// Where each line of a block's body is in its doc.
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

    /// Where the body's `line`th line (0-based) is in the doc.
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
    /// Before compiling: a character outside a literal or comment that isn't Rust syntax: any
    /// non-ASCII character, an ASCII control character or DEL other than whitespace, a backtick
    /// or a backslash.
    Character(char),
    /// Before compiling: a macro called through a path (`::core::println!`, `x::println!`),
    /// named by the macro.
    QualifiedMacro(String),
    /// Before compiling: a macro call or definition off [`ALLOWED_MACROS`], named by the macro
    /// (`macro_rules` for a definition, `as println` for a rename to an allowed name).
    UnlistedMacro(String),
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

/// `c` as `U+XXXX`.
#[must_use]
pub(crate) fn code_point(c: char) -> String {
    format!("U+{:04X}", u32::from(c))
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
                "names `{name}` other than as a path segment before `::`, which could call `{name}!` under another name and read a file or environment variable at compile time"
            ),
            Self::MetavariableAttribute => {
                "has a `$` metavariable in an attribute, which a macro could make a `path` attribute".to_owned()
            }
            Self::UnlistedMacro(name) if name == "macro_rules" => {
                "defines a macro with `macro_rules!`, which could build an attribute supdoc's scan never sees".to_owned()
            }
            Self::UnlistedMacro(name) if name.starts_with("as ") => format!(
                "renames something `{}`, which could make an unlisted macro look like one supdoc allows",
                name.trim_start_matches("as ")
            ),
            Self::UnlistedMacro(name) => format!(
                "calls `{name}!`, which isn't on supdoc's list of macros that never read a file or build an attribute"
            ),
            Self::Character(c) => format!(
                "has the character {} outside a literal or comment, which rustc may read as syntax supdoc's scan doesn't see",
                code_point(*c)
            ),
            Self::QualifiedMacro(name) => format!(
                "calls `{name}!` through a path, and only an unqualified call is known to reach the standard macro"
            ),
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
            Self::UnlistedMacro(name) => format!(
                "remove `{name}` from the block, or mark it `ignore(<why>)`"
            ),
            Self::Character(c) => format!(
                "remove {} from the code, or keep it inside a string or comment, or mark the block `ignore(<why>)`",
                code_point(*c)
            ),
            Self::QualifiedMacro(name) => format!(
                "call it as plain `{name}!`, or mark the block `ignore(<why>)`"
            ),
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
    scan_capped(input, None)
}

/// [`compile_time_read`] with the rules for a block about to be compiled as `edition`, which
/// renames and macros can't get past. The block is read with the token boundaries and keywords
/// rustc gives `edition`: before 2021, `c"` and `cr#"` start no literal, so `cr#".."#` is `cr`,
/// `#` and an ordinary string; `async`, `await` and `dyn` are identifiers in 2015, and `gen`
/// before 2024. Besides its findings, a block is refused when:
///
/// - `include`, `include_str`, `include_bytes`, `env` or `option_env` appears as an identifier
///   anywhere except as a path segment followed by `::` (`std::env::var`), so `use
///   std::include_str as inc;` and a `macro_rules!` handed the name are refused;
/// - a `$` metavariable or the word `path` appears anywhere in an attribute;
/// - a module isn't written inline (`mod name { .. }`) outside any macro: rustc reads an outline
///   `mod name;` from a file beside its working directory, and a macro can turn an inline module
///   into one.
#[must_use]
pub(crate) fn before_compiling(input: &str, edition: Edition) -> Scan {
    scan_capped(input, Some(edition))
}

/// [`scan`] of `input` under [`MAX_SCAN_BYTES`], a `compiles` edition choosing
/// [`before_compiling`]'s rules.
fn scan_capped(input: &str, compiles: Option<Edition>) -> Scan {
    if input.len() > MAX_SCAN_BYTES {
        return Scan::TooLarge;
    }
    scan(input, compiles).map_or(Scan::Clean, Scan::Read)
}

/// The first compile-time read in `input`, by [`compile_time_read`]'s rules, and by
/// [`before_compiling`]'s for the edition `compiles` names, when it names one, without the size
/// cap. [`compile_time_read`] lexes as the newest edition does.
fn scan(input: &str, compiles: Option<Edition>) -> Option<Found> {
    let edition = compiles.unwrap_or(Edition::E2024);
    let compiles = compiles.is_some();
    if input.starts_with("#!") && !input.starts_with("#![") {
        return Some(Found::Unscannable(
            "a `#!` first line that isn't an inner attribute",
        ));
    }
    if compiles && starts_with_frontmatter(input) {
        return Some(Found::Unscannable(FRONTMATTER));
    }
    let mut lexer = Tokens::new(input, compiles, edition);
    let tokens: Vec<Token<'_>> = lexer.by_ref().collect();
    // Each token, a raw identifier read as the identifier it names.
    let at = |index: Option<usize>| {
        index
            .and_then(|index| tokens.get(index))
            .map_or(Token::Other, |token| token.plain())
    };
    // The bracket depth inside the attribute being read, or 0 outside one.
    let mut attribute = 0_usize;
    let mut nesting = Nesting::default();
    // The macro whose input or body the next delimiter opens: a `name!` or `macro_rules! name`
    // just ended.
    let mut macro_next: Option<MacroKind> = None;
    for index in 0..tokens.len() {
        let token = at(Some(index));
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
                    if compiles {
                        let raw = index
                            .checked_sub(1)
                            .and_then(|before| tokens.get(before))
                            .is_some_and(|token| matches!(token, Token::RawIdent(_)));
                        let qualified = before_that == Token::Punct(':')
                            && at(index.checked_sub(3)) == Token::Punct(':');
                        if let Some(found) = unlisted_macro(word, raw, qualified, next, edition) {
                            return Some(found);
                        }
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
            Token::Ident(word) if compiles => {
                let in_macro = nesting.in_macro > 0;
                if let Some(found) = strict_word(word, next, after_next, attribute > 0, in_macro) {
                    return Some(found);
                }
                if opens_macro.is_some() && before == Token::Punct('!') {
                    macro_next = opens_macro;
                }
            }
            Token::Punct('$') if compiles && attribute > 0 => {
                return Some(Found::MetavariableAttribute);
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

/// The macros a block may call before it is compiled, by the last segment of the called path.
///
/// An allowlist, not a denylist: a macro can rearrange the tokens it is given, so a
/// `macro_rules!` of the block's own could pair a `#` with a bracket group into an attribute the
/// scan never saw as one, and a later Rust may add macros that read files. These standard macros
/// format, assert, panic, build vectors or expand to constants, and never turn their input into
/// an attribute or item. `concat` is safe because the read macros are refused by name wherever
/// they appear. Any `macro_rules!` definition, and any other call, refuses the block.
const ALLOWED_MACROS: [&str; 28] = [
    "println",
    "print",
    "eprintln",
    "eprint",
    "format",
    "format_args",
    "write",
    "writeln",
    "vec",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
    "matches",
    "dbg",
    "stringify",
    "line",
    "column",
    "file",
    "module_path",
    "cfg",
    "concat",
];

/// The keywords that can stand before a `!` that negates: `if !(..)`, `return !x`. Some are
/// keywords only from an edition on; [`is_keyword`] says which.
const KEYWORDS: [&str; 39] = [
    "as", "async", "await", "box", "break", "const", "continue", "crate", "do", "dyn", "else",
    "enum", "extern", "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match",
    "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
    "trait", "true", "type", "unsafe", "use",
];

/// Whether `name` is one of the [`KEYWORDS`] in `edition`: `async`, `await` and `dyn` are
/// identifiers in 2015, and `gen` is one before 2024.
fn is_keyword(name: &str, edition: Edition) -> bool {
    let identifier = match name {
        "async" | "await" | "dyn" => edition == Edition::E2015,
        "gen" => edition != Edition::E2024,
        _ => false,
    };
    !identifier && KEYWORDS.contains(&name)
}

/// The refusal for `name!` followed by `next`, before compiling as `edition`: any
/// `macro_rules!`; a call (`name!` then a delimiter) of a macro not in [`ALLOWED_MACROS`]; and a
/// call through a path (`qualified`: `::` before `name`). `None` for an unqualified allowed call
/// and for a keyword's negating `!`. A `raw` identifier (`r#if`) is never a keyword, nor is a
/// word `edition` doesn't reserve.
fn unlisted_macro(
    name: &str,
    raw: bool,
    qualified: bool,
    next: Token<'_>,
    edition: Edition,
) -> Option<Found> {
    let call = matches!(next, Token::Punct('(' | '[' | '{'));
    let negation = !raw && is_keyword(name, edition);
    if name == "macro_rules" || (call && !negation && !ALLOWED_MACROS.contains(&name)) {
        return Some(Found::UnlistedMacro(name.to_owned()));
    }
    (call && !negation && qualified).then(|| Found::QualifiedMacro(name.to_owned()))
}

/// The refusal [`before_compiling`]'s rules give the identifier `word`, followed by `next` and
/// `after_next`, inside an attribute when `in_attribute` and inside a macro's input or body when
/// `in_macro`: a read macro's name not before `::`, the reserved `macro`, a rename to an allowed
/// macro's name, `path` in an attribute, or a module not written inline outside any macro.
fn strict_word(
    word: &str,
    next: Token<'_>,
    after_next: Token<'_>,
    in_attribute: bool,
    in_macro: bool,
) -> Option<Found> {
    match word {
        word if COMPILE_TIME_READS.contains(&word) => {
            if next == Token::Punct('!') {
                return Some(Found::Macro(word.to_owned()));
            }
            let path_segment = next == Token::Punct(':') && after_next == Token::Punct(':');
            (!path_segment).then(|| Found::Name(word.to_owned()))
        }
        "macro" => Some(Found::UnlistedMacro("macro".to_owned())),
        "as" => match next {
            Token::Ident(name) if ALLOWED_MACROS.contains(&name) => {
                Some(Found::UnlistedMacro(format!("as {name}")))
            }
            _ => None,
        },
        "path" if in_attribute => Some(Found::PathAttribute),
        "mod" => {
            let name = match next {
                Token::Ident(name) => Some(name.to_owned()),
                _ => None,
            };
            (in_macro || name.is_none() || after_next != Token::Punct('{'))
                .then_some(Found::OutlineModule(name))
        }
        _ => None,
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

/// What [`before_compiling`] calls a block that may open with frontmatter.
pub(crate) const FRONTMATTER: &str = "frontmatter";

/// Whether `input`, past the whitespace rustc skips ([`is_rust_whitespace`]), starts with `-`:
/// rustc may read a leading `---` as frontmatter, whose text the scan doesn't follow, so before
/// compiling such a block fails closed rather than modeling rustc's frontmatter rules.
fn starts_with_frontmatter(input: &str) -> bool {
    input
        .trim_start_matches(is_rust_whitespace)
        .starts_with('-')
}

/// One token of a Rust block, as far as the scan needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'text> {
    /// An identifier or keyword.
    Ident(&'text str),
    /// A raw identifier, `r#name`, held without its `r#`: never a keyword.
    RawIdent(&'text str),
    /// One punctuation character.
    Punct(char),
    /// A literal, a lifetime, or anything else the scan doesn't judge.
    Other,
}

impl Token<'_> {
    /// The token, with a raw identifier as the plain identifier it names.
    const fn plain(self) -> Self {
        match self {
            Self::RawIdent(name) => Self::Ident(name),
            token => token,
        }
    }
}

/// The tokens of a Rust block, skipping whitespace and comments. Each byte is read once.
///
/// Literals, comments and identifiers follow `rustc_lexer`'s rules (`lifetime_or_char`,
/// `single_quoted_string`, `double_quoted_string`, `raw_double_quoted_string`, `block_comment`
/// and `line_comment`), so a literal ends exactly where rustc ends it and no token hides inside
/// one. Where the scan can't follow rustc (a non-ASCII identifier character beside a quote), it
/// fails closed.
struct Tokens<'text> {
    /// The block.
    text: &'text str,
    /// The byte offset of the next unread character.
    at: usize,
    /// What the text ended inside, when it ended inside a comment or a literal, or, when
    /// `strict`, the first literal rustc would reject as malformed.
    unterminated: Option<Found>,
    /// Whether the block is about to be compiled: a doc comment then reads as the `#[doc = ..]`
    /// attribute rustc lowers it to, and a malformed literal fails the scan closed.
    strict: bool,
    /// Tokens already read and not yet returned, the next one last.
    queued: Vec<Token<'text>>,
    /// Whether `c` and `cr` are literal prefixes, as they are from edition 2021 on. Before it,
    /// rustc reads `cr#".."#` as `cr`, `#` and an ordinary string.
    c_strings: bool,
}

impl<'text> Tokens<'text> {
    /// The tokens of `text` in `edition`, `strict` when the block is about to be compiled.
    fn new(text: &'text str, strict: bool, edition: Edition) -> Self {
        Self {
            text,
            at: 0,
            unterminated: None,
            strict,
            queued: Vec::new(),
            c_strings: matches!(edition, Edition::E2021 | Edition::E2024),
        }
    }

    /// Records, when `strict`, that the text holds a literal rustc rejects as `what`.
    fn malformed(&mut self, what: &'static str) {
        if self.strict && self.unterminated.is_none() {
            self.unterminated = Some(Found::Unscannable(what));
        }
    }

    /// Queues the tokens of the `#[doc = ".."]` attribute rustc lowers a doc comment to, with
    /// `!` after the `#` when `inner`, and returns its first.
    fn doc_attribute(&mut self, inner: bool) -> Token<'text> {
        self.queued.extend([
            Token::Punct(']'),
            Token::Other,
            Token::Punct('='),
            Token::Ident("doc"),
            Token::Punct('['),
        ]);
        if inner {
            self.queued.push(Token::Punct('!'));
        }
        Token::Punct('#')
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
        if let Some(token) = self.queued.pop() {
            return Some(token);
        }
        loop {
            self.bump_while(is_rust_whitespace);
            let rest = self.rest();
            let c = rest.chars().next()?;
            if rest.starts_with("//") {
                // rustc_lexer `line_comment`: `///` (not `////`) is outer, `//!` inner.
                let outer = rest.starts_with("///") && !rest.starts_with("////");
                let inner = rest.starts_with("//!");
                self.bump_while(|c| c != '\n');
                if self.strict && (outer || inner) {
                    return Some(self.doc_attribute(inner));
                }
            } else if rest.starts_with("/*") {
                // rustc_lexer `block_comment`: `/**` not followed by `*` or `/` is outer, `/*!`
                // inner.
                let outer = rest.starts_with("/**")
                    && !rest.starts_with("/***")
                    && !rest.starts_with("/**/");
                let inner = rest.starts_with("/*!");
                self.block_comment();
                if self.strict && self.unterminated.is_none() && (outer || inner) {
                    return Some(self.doc_attribute(inner));
                }
            } else if c == '"' {
                self.string();
                return Some(Token::Other);
            } else if c == '\'' {
                self.quote();
                return Some(Token::Other);
            } else if is_ident_char(c) {
                return Some(self.word());
            } else if self.strict && !is_rust_syntax(c) {
                self.at = self.text.len();
                self.unterminated = Some(Found::Character(c));
            } else {
                self.bump(1);
                return Some(Token::Punct(c));
            }
        }
    }
}

impl<'text> Tokens<'text> {
    /// Reads a character literal or a lifetime the cursor is on, as `rustc_lexer`'s
    /// `lifetime_or_char` does: `'` before `x'` is a character literal; `'` before an identifier
    /// run is a lifetime or label unless a `'` follows the run, which makes it a (malformed)
    /// character literal ending there; anything else is a character literal read by
    /// [`char_literal`](Self::char_literal). A non-ASCII character where rustc's choice depends
    /// on Unicode identifier classes fails the scan closed.
    fn quote(&mut self) {
        self.bump(1);
        let (first, second) = (self.peek(0), self.peek(1));
        let can_be_lifetime = second != Some('\'') && first.is_some_and(is_ident_char);
        if !can_be_lifetime {
            if first.is_some_and(|c| !c.is_ascii()) && second != Some('\'') {
                self.ended_inside("a quote before a non-ASCII character");
                return;
            }
            self.char_literal();
            return;
        }
        if first == Some('r') && second == Some('#') && self.peek(2).is_some_and(is_ident_char) {
            // A raw lifetime: `'r#name`.
            self.bump(2);
        }
        self.bump(1);
        self.bump_while(is_ident_char);
        match self.peek(0) {
            Some(c) if !c.is_ascii() => {
                self.ended_inside("a lifetime or character literal with a non-ASCII character");
            }
            Some('\'') => {
                self.bump(1);
                self.malformed("a character literal holding more than one character");
            }
            _ => {}
        }
    }

    /// Reads a character or byte literal whose opening `'` the cursor is past, as `rustc_lexer`'s
    /// `single_quoted_string` does: one character then `'`; otherwise up to the closing `'`, an
    /// escape taking two characters, stopping unterminated at a `/`, at a newline not followed by
    /// `'`, or at the end. A literal that isn't one character or one known escape is malformed.
    fn char_literal(&mut self) {
        if self.peek(1) == Some('\'') && self.peek(0) != Some('\\') {
            if matches!(self.peek(0), Some('\n' | '\r' | '\t' | '\'')) {
                self.malformed(
                    "a character literal holding an unescaped control character or quote",
                );
            }
            self.bump(2);
            return;
        }
        let start = self.at;
        loop {
            match self.peek(0) {
                Some('\'') => {
                    let body = self.text.get(start..self.at).unwrap_or_default();
                    self.bump(1);
                    if !is_escape(body) {
                        self.malformed("a character literal that isn't one character or escape");
                    }
                    return;
                }
                Some('/') | None => break,
                Some('\n') if self.peek(1) != Some('\'') => break,
                Some('\\') => self.bump(2),
                Some(_) => self.bump(1),
            }
        }
        self.ended_inside("an unterminated character literal");
    }

    /// Reads an identifier, a raw identifier, a prefixed or raw string, or a number the cursor is
    /// on. A `r#`, `br#` or (from edition 2021) `cr#` that starts neither a raw string nor a raw
    /// identifier fails a strict scan closed, as does a raw string with more than 255 `#`s, which
    /// rustc rejects. Before edition 2021, `c` and `cr` are identifiers like any other.
    fn word(&mut self) -> Token<'text> {
        let start = self.at;
        self.bump_while(is_ident_char);
        let word = self.text.get(start..self.at).unwrap_or_default();
        if word.starts_with(|c: char| c.is_ascii_digit()) {
            return Token::Other;
        }
        let raw_prefix = matches!(word, "r" | "br") || (self.c_strings && word == "cr");
        let (hashes, quote) = self.hashes_then_quote();
        if raw_prefix && quote {
            if hashes > 255 {
                self.malformed("a raw string with more than 255 `#`s");
            }
            self.raw_string(hashes);
            return Token::Other;
        }
        let string_prefix = word == "b" || (self.c_strings && word == "c");
        if string_prefix && self.peek(0) == Some('"') {
            self.string();
            return Token::Other;
        }
        if matches!(word, "b") && self.peek(0) == Some('\'') {
            self.bump(1);
            self.char_literal();
            return Token::Other;
        }
        if word == "r" && hashes == 1 && self.peek(1).is_some_and(is_ident_char) {
            self.bump(1);
            let start = self.at;
            self.bump_while(is_ident_char);
            return Token::RawIdent(self.text.get(start..self.at).unwrap_or_default());
        }
        if raw_prefix && hashes > 0 {
            self.malformed("a `#` after a raw prefix that starts no raw string or identifier");
        }
        Token::Ident(word)
    }
}

/// Whether `body`, a character literal's text between its quotes, is one escape rustc knows:
/// `\n`, `\r`, `\t`, `\\`, `\0`, `\'`, `\"`, `\x` and two hex digits, or `\u{..}` with one to six
/// hex digits.
fn is_escape(body: &str) -> bool {
    let Some(escape) = body.strip_prefix('\\') else {
        return false;
    };
    if matches!(escape, "n" | "r" | "t" | "\\" | "0" | "'" | "\"") {
        return true;
    }
    if let Some(hex) = escape.strip_prefix('x') {
        return hex.len() == 2 && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    escape
        .strip_prefix("u{")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|hex| {
            let digits = hex.chars().filter(|c| *c != '_').count();
            (1..=6).contains(&digits)
                && !hex.starts_with('_')
                && hex.chars().all(|c| c.is_ascii_hexdigit() || c == '_')
        })
}

/// Whether `c` can be part of an identifier or number, by the scan's ASCII-only rule: any other
/// character ends a word, which can only split words and so only add matches.
const fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `c`, outside a literal or comment and not whitespace, is a character Rust's syntax
/// uses: an ASCII letter, digit or punctuation mark other than a backtick or backslash. Before
/// compiling, any other character fails the scan closed: rustc may read a non-ASCII one as a
/// confusable `#`, `!` or bracket the scan never saw, and it rejects the rest.
const fn is_rust_syntax(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '`' | '\\')
}

/// Whether rustc skips `c` as whitespace: Unicode's `Pattern_White_Space`, as `rustc_lexer`'s
/// `is_whitespace`, which takes in the left-to-right and right-to-left marks, U+200E and U+200F.
/// Any other whitespace is a token of its own, as it is an error token to rustc.
const fn is_rust_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{000B}'
            | '\u{000C}'
            | '\r'
            | ' '
            | '\u{0085}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// The pieces of rustfmt's overflow error around its maximum and found widths, which reads
/// ``exceeded maximum width (maximum: N (see `max_width` option), found: M)``.
const OVERFLOW: (&str, &str, &str) = (
    "exceeded maximum width (maximum: ",
    " (see `max_width` option), found: ",
    ")",
);

/// `stderr`, rustfmt's report on a block it read inside a frame with its `max_width` grown by
/// `widened` columns, in the project's terms. An overflow error's maximum and found widths are
/// each `widened` less. A report location, which rustfmt writes `<stdin>:L:L:C` (its file and
/// line, then the line and column), is written `<stdin>:L:C`, the form [`Frame::hidden`]
/// renumbers; under an overflow error its column is moved back by `widened` less `hidden`, the
/// columns [`Frame::hidden`] then moves it back by, so it lands `widened` columns back.
/// `stderr` as it is when it isn't UTF-8.
///
/// [`Frame::hidden`]: super::fragment::Frame::hidden
#[must_use]
pub(crate) fn unwidened(stderr: &[u8], widened: usize, hidden: usize) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(stderr) else {
        return stderr.to_vec();
    };
    let mut overflow = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, end) = line
            .strip_suffix('\n')
            .map_or((line, ""), |body| (body, "\n"));
        if line.starts_with("error") {
            overflow = body.contains(OVERFLOW.0);
        }
        let restated = overflow_widths(body, widened)
            .or_else(|| {
                let back = if overflow {
                    widened.saturating_sub(hidden)
                } else {
                    0
                };
                report_location(body, back)
            })
            .unwrap_or_else(|| body.to_owned());
        out.push_str(&restated);
        out.push_str(end);
    }
    out.into_bytes()
}

/// `line`, an overflow error, with its maximum and found widths each `widened` less. `None`
/// for any other line.
fn overflow_widths(line: &str, widened: usize) -> Option<String> {
    let (head, rest) = line.split_once(OVERFLOW.0)?;
    let (maximum, rest) = rest.split_once(OVERFLOW.1)?;
    let (found, tail) = rest.split_once(OVERFLOW.2)?;
    let maximum = maximum.parse::<usize>().ok()?.saturating_sub(widened);
    let found = found.parse::<usize>().ok()?.saturating_sub(widened);
    Some(format!(
        "{head}{}{maximum}{}{found}{}{tail}",
        OVERFLOW.0, OVERFLOW.1, OVERFLOW.2
    ))
}

/// `line`, rustfmt's ` --> <stdin>:L:L:C` report location, as ` --> <stdin>:L:C` with its column
/// moved back by `back`, never before column 1. `None` for any other line.
fn report_location(line: &str, back: usize) -> Option<String> {
    let (head, numbers) = line.split_once("<stdin>:")?;
    if !head.trim_end().ends_with("-->") {
        return None;
    }
    let mut parts = numbers.split(':');
    let (Some(file_line), Some(line_again), Some(column), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let line_number = file_line.parse::<usize>().ok()?;
    if line_again.parse::<usize>().ok()? != line_number {
        return None;
    }
    let column = column.parse::<usize>().ok()?.saturating_sub(back).max(1);
    Some(format!("{head}<stdin>:{line_number}:{column}"))
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
        EmitDir, FRAME_NAMES, FRONTMATTER, Found, LineMap, MAX_SCAN_BYTES, Scan, Token, Tokens,
        attribute_edition, before_compiling, clippy_conf_dir, compile_time_read, config_args,
        lint_verdict, parse_error, scan, unwidened,
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
    fn the_line_map_points_each_body_line_at_its_doc_line() {
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

    /// The fastest of five scans of `block`, so a moment the machine spends elsewhere drops out.
    fn fastest_scan(block: &str) -> Duration {
        (0..5)
            .map(|_| {
                let started = Instant::now();
                let _found = scan(block, Some(Edition::E2024));
                started.elapsed()
            })
            .min()
            .unwrap_or_default()
    }

    #[test]
    fn pathological_blocks_scan_in_one_linear_pass() {
        // Measured against ordinary code of the same length on the same machine, not a fixed
        // time: a scan that went back over the block would take thousands of times as long.
        const RATIO: u32 = 100;
        for (name, block) in [
            ("200k `#[`", "#[".repeat(200_000)),
            ("100k `env /*`", "env /*".repeat(100_000)),
            ("100k `include /* */`", "include /* */ ".repeat(100_000)),
            ("100k raw strings", "r#\"\"".repeat(100_000)),
        ] {
            let ordinary: String = "let x = 1;\n".chars().cycle().take(block.len()).collect();

            let took = fastest_scan(&block);
            let baseline = fastest_scan(&ordinary);

            assert!(
                took < baseline.saturating_mul(RATIO),
                "{name} took {took:?}, over {RATIO} times the {baseline:?} of ordinary code"
            );
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
        crate::cli::testing::symlink(&target, root.path().join(".rustfmt.toml"))
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
        crate::cli::testing::symlink(
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
        crate::cli::testing::symlink(
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
            rustc_line("error", Some("E0308"), &span("/rustc/lib.rs", true, 7, 1)),
            rustc_line("warning", None, ""),
            rustc_line("error", None, ""),
            rustc_line("note", Some("x"), &span("<anon>", true, 1, 1)),
            "not json\n".to_owned(),
        ]
        .concat();

        let verdict = lint_verdict(
            "",
            false,
            Ok(Ran {
                success: true,
                stdout: Vec::new(),
                stderr: stderr.into_bytes(),
            }),
        );

        assert_eq!(
            verdict,
            Verdict::Diagnostics(vec![
                Diagnostic {
                    line: 2,
                    column: 13,
                    end_line: 2,
                    syntax: false,
                    rule: "clippy::ptr_arg".to_owned(),
                    message: "m".to_owned(),
                },
                Diagnostic {
                    line: 0,
                    column: 0,
                    end_line: 0,
                    syntax: false,
                    rule: "E0308".to_owned(),
                    message: "m".to_owned(),
                },
            ]),
            "a warning counts though clippy exits 0; a span outside the block goes to the fence"
        );
        let summaries_only = rustc_line("warning", None, "");
        assert_eq!(
            lint_verdict(
                "",
                false,
                Ok(Ran {
                    success: true,
                    stdout: Vec::new(),
                    stderr: summaries_only.into_bytes(),
                })
            ),
            Verdict::Clean
        );
    }

    #[test]
    fn a_codeless_error_is_a_syntax_error_and_a_coded_error_or_lint_is_not() {
        let brace = rustc_line("error", None, &span("<anon>", true, 4, 1))
            .replace("\"m\"", "\"unexpected closing delimiter: `}`\"");
        let spanning = span("<anon>", true, 2, 5).replace("\"line_end\":2", "\"line_end\":3");
        let stderr = [
            brace,
            rustc_line("error", Some("E0308"), &spanning),
            rustc_line(
                "warning",
                Some("clippy::ptr_arg"),
                &span("<anon>", true, 1, 1),
            ),
        ];

        let read: Vec<(usize, usize, bool)> = diagnostics_of(&stderr)
            .iter()
            .map(|found| (found.line, found.end_line, found.syntax))
            .collect();

        assert_eq!(read, [(4, 4, true), (2, 3, false), (1, 1, false)]);
    }

    /// One diagnostic line with `message`, at line 1 of the block.
    fn named_line(level: &str, code: &str, message: &str) -> String {
        rustc_line(level, Some(code), &span("<anon>", true, 1, 1)).replace("\"m\"", message)
    }

    /// The diagnostics of a clippy run that printed `lines`.
    fn diagnostics_of(lines: &[String]) -> Vec<Diagnostic> {
        match lint_verdict(
            "",
            false,
            Ok(Ran {
                success: false,
                stdout: Vec::new(),
                stderr: lines.concat().into_bytes(),
            }),
        ) {
            Verdict::Diagnostics(found) => found,
            other => panic!("expected diagnostics, got {other:?}"),
        }
    }

    /// The text clippy reads for a members block, and for a statements block.
    const MEMBERS: &str = "struct SupdocFragment; impl SupdocFragment {\nfn f(&self) {}\n}\n";
    const STATEMENTS: &str = "fn supdoc_fragment() {\nlet a = 1;\n}\n";
    const EXPRESSION: &str = "fn supdoc_fragment() -> impl Sized {\nSupdocFragment\n}\n";

    /// clippy's verdict on a framed run over `checked` that exited `success` and printed
    /// `stderr`.
    fn framed_verdict(checked: &str, success: bool, stderr: &str) -> Verdict {
        lint_verdict(
            checked,
            true,
            Ok(Ran {
                success,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            }),
        )
    }

    /// The messages of a framed run over `checked` that printed the diagnostic `lines`.
    fn framed_messages(checked: &str, lines: &[String]) -> Vec<String> {
        match framed_verdict(checked, false, &lines.concat()) {
            Verdict::Diagnostics(found) => found.into_iter().map(|found| found.message).collect(),
            other => panic!("expected diagnostics, got {other:?}"),
        }
    }

    #[test]
    fn every_frame_name_kept_out_of_messages_is_one_its_clippy_frame_declares() {
        let clippy = crate::code_blocks::tools::CARTRIDGES
            .iter()
            .find(|tool| tool.name == "clippy")
            .expect("the cartridges should include clippy");
        let frames: Vec<&str> = clippy
            .fragments
            .iter()
            .filter_map(|(kind, support)| support.frame(*kind))
            .map(|frame| frame.before)
            .collect();

        for (opens, name, _) in FRAME_NAMES {
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.starts_with(opens) && frame.contains(name)),
                "no frame opens with {opens:?} and declares {name}: {frames:?}"
            );
        }
    }

    #[test]
    fn a_members_message_names_the_frames_struct_as_self() {
        let line = named_line(
            "error",
            "E0609",
            "\"no field `w` on type `&SupdocFragment`\"",
        );

        assert_eq!(
            framed_messages(MEMBERS, &[line]),
            ["no field `w` on type `&Self`"]
        );
    }

    #[test]
    fn a_name_that_holds_a_frame_name_stays_as_written() {
        let lines = [
            named_line(
                "error",
                "E0425",
                "\"cannot find value `supdoc_fragment_total` in this scope\"",
            ),
            named_line(
                "error",
                "E0433",
                "\"failed to resolve: use of undeclared type `SupdocFragmentConfig`\"",
            ),
        ];

        assert_eq!(
            framed_messages(STATEMENTS, &lines),
            [
                "cannot find value `supdoc_fragment_total` in this scope",
                "failed to resolve: use of undeclared type `SupdocFragmentConfig`",
            ]
        );
        assert_eq!(
            framed_messages(MEMBERS, &lines[1..]),
            ["failed to resolve: use of undeclared type `SupdocFragmentConfig`"],
            "the members frame's struct is a whole name, never a prefix"
        );
    }

    #[test]
    fn a_name_the_frame_in_use_doesnt_declare_stays_as_written() {
        let line = named_line(
            "error",
            "E0425",
            "\"cannot find value `SupdocFragment` in this scope\"",
        );

        assert_eq!(
            framed_messages(EXPRESSION, &[line]),
            ["cannot find value `SupdocFragment` in this scope"],
            "the expression frame declares a function, never the members frame's struct"
        );
    }

    #[test]
    fn a_framed_syntax_error_wins_over_the_missing_names_rustc_recovers_into() {
        let syntax = rustc_line("error", None, &span("<anon>", true, 2, 10))
            .replace("\"m\"", "\"expected `;`, found `missing`\"");
        let missing = named_line(
            "error",
            "E0425",
            "\"cannot find function `missing` in this scope\"",
        );
        let lines = [syntax, missing];

        let Verdict::Diagnostics(framed) = framed_verdict(STATEMENTS, false, &lines.concat())
        else {
            panic!("a syntax error is a diagnostic");
        };
        let unframed = diagnostics_of(&lines);

        let syntax_only: Vec<(usize, bool)> = framed
            .iter()
            .map(|found| (found.line, found.syntax))
            .collect();
        assert_eq!(syntax_only, [(2, true)], "{framed:?}");
        assert_eq!(
            unframed.len(),
            1,
            "an unframed run keeps its missing names alone: {unframed:?}"
        );
        assert!(!unframed.iter().any(|found| found.syntax), "{unframed:?}");
    }

    #[test]
    fn a_framed_tool_failure_shows_the_rendered_text_without_frame_names_or_suggestions() {
        let rendered = "error: internal compiler error\\n --> <anon>:1:1\\n1 | fn \
                        supdoc_fragment() {\\n1 ~ fn supdoc_fragment() -> Result<(), E> {\\n4 + \
                        Ok(())\\n";
        let stderr = format!("{{\"rendered\":\"{rendered}\"}}\nthread 'rustc' panicked\n");

        let Verdict::Failed(super::format::Verdict::ToolFailed(detail)) =
            framed_verdict(STATEMENTS, false, &stderr)
        else {
            panic!("a run with no diagnostic that failed is a tool failure");
        };

        assert_eq!(
            detail,
            "error: internal compiler error\n --> <anon>:1:1\n1 | fn <the function supdoc adds>() {\n\
             thread 'rustc' panicked",
            "each JSON line becomes its rendered lines, where the lint step finds frame lines, \
             and a suggestion line, which no frame line matches, is left out"
        );
    }

    #[test]
    fn a_suggestion_line_has_a_line_number_then_a_change_marker() {
        for line in ["1 ~ fn f() {", "4 +     Ok(())", "12 - x", "  3 ~"] {
            assert!(super::suggestion_line(line), "{line:?}");
        }
        for line in ["1 | fn f() {", "  |", "1 ~x", "~ 1", "error: 1 + 2"] {
            assert!(!super::suggestion_line(line), "{line:?}");
        }
    }

    #[test]
    fn one_missing_name_reported_twice_is_one_diagnostic() {
        let found = diagnostics_of(&[
            named_line(
                "error",
                "E0425",
                "\"cannot find function `missing` in this scope\"",
            ),
            named_line(
                "error",
                "E0425",
                "\"cannot find function `missing` in this scope\"",
            ),
            named_line("error", "E0432", "\"unresolved import `proj::Nope`\""),
        ]);

        let names: Vec<_> = found.iter().filter_map(super::lint::missing_name).collect();
        assert_eq!(names, ["missing", "proj::Nope"], "{found:?}");
    }

    #[test]
    fn a_missing_name_drops_the_blocks_other_diagnostics() {
        let found = diagnostics_of(&[
            named_line("warning", "clippy::ptr_arg", "\"m\""),
            named_line(
                "error",
                "E0425",
                "\"cannot find function `nope` in this scope\"",
            ),
        ]);

        let rules: Vec<_> = found.iter().map(|found| found.rule.as_str()).collect();
        assert_eq!(rules, ["E0425"], "{found:?}");
    }

    #[test]
    fn without_a_missing_name_the_diagnostics_are_unchanged() {
        let found = diagnostics_of(&[
            named_line("warning", "clippy::ptr_arg", "\"m\""),
            named_line("error", "E0308", "\"mismatched types\""),
            named_line("warning", "clippy::ptr_arg", "\"m\""),
        ]);

        assert_eq!(found.len(), 3, "{found:?}");
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
                "const S: &str = concat!(include_str);\n",
                name("include_str"),
            ),
            (
                "const S: &str = r#include_bytes!(\"x\");\n",
                Scan::Read(Found::Macro("include_bytes".to_owned())),
            ),
            (
                "#[doc = $p] fn f() {}\n",
                Scan::Read(Found::MetavariableAttribute),
            ),
            (
                "#[$($a)*] mod x {}\n",
                Scan::Read(Found::MetavariableAttribute),
            ),
            (
                "#[cfg_attr(all(), path)]\nmod x {}\n",
                Scan::Read(Found::PathAttribute),
            ),
        ] {
            assert_eq!(
                before_compiling(block, Edition::E2024),
                refused,
                "{block:?}"
            );
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
            ("mod $n;\n", outline(None)),
            (
                "fn f() { println!(\"{}\", { mod x {} 1 }); }\n",
                outline(Some("x")),
            ),
        ] {
            assert_eq!(
                before_compiling(block, Edition::E2024),
                refused,
                "{block:?}"
            );
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
            assert_eq!(
                before_compiling(block, Edition::E2024),
                Scan::Clean,
                "{block:?}"
            );
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
            assert_eq!(
                before_compiling(block, Edition::E2024),
                refused,
                "{block:?}"
            );
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

        assert_eq!(before_compiling(block, Edition::E2024), Scan::Clean);
    }

    #[test]
    fn before_compiling_a_hash_a_macro_could_pair_with_any_bracket_group_is_refused() {
        for (block, what) in [
            (
                "fn f() { println!(\"{:?}\", #[allow(unused)] [1]); }\n",
                "a `#` inside a macro call's input",
            ),
            ("# $b fn f() {}\n", "a `#` that doesn't start an attribute"),
        ] {
            assert_eq!(
                before_compiling(block, Edition::E2024),
                Scan::Read(Found::Unscannable(what)),
                "{block:?}"
            );
        }
    }

    #[test]
    fn before_compiling_macro_export_passes_and_an_unsafe_wrapper_is_refused() {
        assert_eq!(
            before_compiling("#[macro_export]\npub fn f() {}\n", Edition::E2024),
            Scan::Clean
        );
        assert_eq!(
            before_compiling("#[unsafe(no_mangle)]\npub fn f() {}\n", Edition::E2024),
            Scan::Read(Found::Attribute("unsafe".to_owned()))
        );
    }

    /// The kinds of `input`'s tokens, read strictly, and what the scan made of the text after.
    fn kinds(input: &str) -> (Vec<Token<'_>>, Option<Found>) {
        let mut lexer = Tokens::new(input, true, Edition::E2024);
        let tokens = lexer.by_ref().collect();
        (tokens, lexer.unterminated)
    }

    #[test]
    fn lifetimes_and_character_literals_split_as_rustc_lexer_splits_them() {
        let other = || vec![Token::Other];
        for (input, tokens) in [
            ("'a", other()),
            ("'a'", other()),
            ("'\\''", other()),
            ("'\\\\'", other()),
            ("b'x'", other()),
            ("'static", other()),
            ("'_", other()),
            ("'r#fn", other()),
            ("'a x", vec![Token::Other, Token::Ident("x")]),
        ] {
            assert_eq!(kinds(input), (tokens, None), "{input:?}");
        }
        assert_eq!(
            kinds("'ab' x"),
            (
                vec![Token::Other, Token::Ident("x")],
                Some(Found::Unscannable(
                    "a character literal holding more than one character"
                ))
            ),
            "`'ab'` is one malformed character literal, as rustc reads it"
        );
        for (input, what) in [
            (
                "'\\q'",
                "a character literal that isn't one character or escape",
            ),
            ("'\\ /x'", "an unterminated character literal"),
            (
                "b'ab'",
                "a character literal that isn't one character or escape",
            ),
            (
                "'e\u{301}'",
                "a lifetime or character literal with a non-ASCII character",
            ),
            ("'\u{e9}x", "a quote before a non-ASCII character"),
            (
                "br#x",
                "a `#` after a raw prefix that starts no raw string or identifier",
            ),
        ] {
            assert_eq!(kinds(input).1, Some(Found::Unscannable(what)), "{input:?}");
        }
        assert_eq!(
            compile_time_read("const C: char = 'ab'; const D: &str = \"x\";\n"),
            Scan::Clean,
            "rustfmt's scan reads the same extents without refusing malformed literals"
        );
    }

    #[test]
    fn a_multi_character_literal_cant_hide_a_read_from_either_scan() {
        let marker = "/outside/marker/marker.rs";
        let probe_08 =
            format!("pub const C: char = 'ab'\\; pub const D: u8 = include!(\"{marker}\"); //'\n");
        let probe_10 = format!(
            "pub const C: char = 'ab'\\; pub static D: &str = include_str!(\"{marker}\"); //'\n"
        );
        let combining = format!(
            "pub const C: char = 'e\u{301}'\\; pub const D: u8 = include!(\"{marker}\"); //'\n"
        );
        for block in [&probe_08, &probe_10, &combining] {
            assert!(
                matches!(before_compiling(block, Edition::E2024), Scan::Read(_)),
                "{block:?}"
            );
        }
        assert_eq!(compile_time_read(&probe_08), read_of("include"));
        assert_eq!(compile_time_read(&probe_10), read_of("include_str"));
    }

    #[test]
    fn before_compiling_a_doc_comment_is_the_attribute_rustc_lowers_it_to() {
        let path = "/outside/marker/x.py";
        let probe_01 = format!(
            "macro_rules! m {{ ($h:tt $d:tt $b:tt) => {{ $h $b pub fn f() {{}} }} }}\n\
             m!(/// x\n[debugger_visualizer(gdb_script_file = \"{path}\")]);\n"
        );
        let in_a_call = format!(
            "fn f() {{ println!(\"{{}}\", /// x\n[debugger_visualizer(gdb_script_file = \"{path}\")]); }}\n"
        );

        assert!(matches!(
            before_compiling(&probe_01, Edition::E2024),
            Scan::Read(_)
        ));
        assert_eq!(
            before_compiling(&in_a_call, Edition::E2024),
            Scan::Read(Found::Unscannable("a `#` inside a macro call's input"))
        );
        assert_eq!(
            kinds("//! inner\n").0,
            [
                Token::Punct('#'),
                Token::Punct('!'),
                Token::Punct('['),
                Token::Ident("doc"),
                Token::Punct('='),
                Token::Other,
                Token::Punct(']'),
            ]
        );
        for plain in ["//// x\n", "/**/", "/*** x */", "// x\n"] {
            assert_eq!(kinds(plain).0, [], "{plain:?}");
        }
        assert_eq!(
            before_compiling(
                "//! Crate docs.\n/// Adds.\npub fn f() {}\n/** More. */\npub fn g() {}\n",
                Edition::E2024
            ),
            Scan::Clean
        );
    }

    #[test]
    fn before_compiling_macros_off_the_allowlist_are_refused() {
        let unlisted = |name: &str| Scan::Read(Found::UnlistedMacro(name.to_owned()));
        for (block, refused) in [
            (
                "macro_rules! m { () => {}; }\nm!();\n",
                unlisted("macro_rules"),
            ),
            ("fn f() { foo!(); }\n", unlisted("foo")),
            ("fn f() { core::arch::asm!(\"nop\"); }\n", unlisted("asm")),
            (
                "use core::arch::global_asm as println;\n",
                unlisted("as println"),
            ),
        ] {
            assert_eq!(
                before_compiling(block, Edition::E2024),
                refused,
                "{block:?}"
            );
        }
    }

    #[test]
    fn before_compiling_allowlisted_macros_and_negations_pass() {
        let block = "fn main() {\n    let v = vec![1, 2];\n    println!(\"{v:?}\");\n    \
                     assert_eq!(v.len(), 2);\n    if !(v.is_empty()) { println!(\"x\"); }\n    \
                     let _ = 1 != 2 && !matches!(v.first(), None);\n}\n";

        assert_eq!(before_compiling(block, Edition::E2024), Scan::Clean);
    }

    #[test]
    fn before_compiling_a_character_that_isnt_rust_syntax_is_refused_by_its_code_point() {
        for (block, c) in [
            (
                "\u{FF03}[debugger_visualizer(gdb_script_file = \"/etc/x.py\")] mod m {}\n",
                '\u{FF03}',
            ),
            ("fn f() { foo\u{FF01}(); }\n", '\u{FF01}'),
            ("fn f() {\u{A0}}\n", '\u{A0}'),
            ("fn f() { `x` }\n", '`'),
            ("fn f() { \\ }\n", '\\'),
            ("fn f() {\u{7}}\n", '\u{7}'),
            ("fn f() {\u{7F}}\n", '\u{7F}'),
            ("fn caf\u{E9}() {}\n", '\u{E9}'),
        ] {
            assert_eq!(
                before_compiling(block, Edition::E2024),
                Scan::Read(Found::Character(c)),
                "{block:?}"
            );
        }
        assert!(
            Found::Character('\u{FF03}').why().contains("U+FF03"),
            "the reason names the code point"
        );
        assert_eq!(
            compile_time_read("fn caf\u{E9}() {}\n"),
            Scan::Clean,
            "rustfmt compiles nothing, so its scan allows any character"
        );
    }

    #[test]
    fn before_compiling_non_ascii_text_in_a_string_or_comment_passes() {
        for block in [
            "const S: &str = \"caf\u{E9} \u{FF03}[x] `y` \\\\\";\n",
            "// caf\u{E9} \u{FF03}[debugger_visualizer]\n/* `x` \u{A0} */\nfn f() {}\n",
            "/// Caf\u{E9}.\npub fn f() {}\n",
            "const C: char = '\u{E9}';\n",
        ] {
            assert_eq!(
                before_compiling(block, Edition::E2024),
                Scan::Clean,
                "{block:?}"
            );
        }
    }

    #[test]
    fn before_compiling_a_raw_identifier_is_never_a_negating_keyword() {
        for (block, name) in [
            ("fn f() { r#if!(x); }\n", "if"),
            ("fn f() { r#match!(x); }\n", "match"),
        ] {
            assert_eq!(
                before_compiling(block, Edition::E2024),
                Scan::Read(Found::UnlistedMacro(name.to_owned())),
                "{block:?}"
            );
        }
        assert_eq!(
            before_compiling(
                "fn f(x: bool) -> bool { if !(x) { return !x; } x }\n",
                Edition::E2024
            ),
            Scan::Clean
        );
    }

    #[test]
    fn before_compiling_a_path_qualified_macro_call_is_refused() {
        for block in [
            "fn f() { ::core::println!(\"x\"); }\n",
            "fn f() { std::println!(\"x\"); }\n",
            "fn f() { crate::x::println!(\"x\"); }\n",
            "fn f() { std :: vec![1]; }\n",
        ] {
            assert!(
                matches!(
                    before_compiling(block, Edition::E2024),
                    Scan::Read(Found::QualifiedMacro(_))
                ),
                "{block:?}: {:?}",
                before_compiling(block, Edition::E2024)
            );
        }
        assert_eq!(
            before_compiling(
                "fn f() { println!(\"x\"); let _ = std::env::args(); }\n",
                Edition::E2024
            ),
            Scan::Clean
        );
    }

    #[test]
    fn before_compiling_a_block_opening_with_frontmatter_is_refused() {
        let read = "pub static Z: &str = include_str!(\"/outside/x.txt\");";
        for block in [
            format!("---\n\"\n---\n{read} // \"\n"),
            "---\n\"\n---\npub const D: u8 = include!(\"/outside/m.rs\"); // \"\n".to_owned(),
            format!("---\n/*\n---\n{read} // */\n"),
            format!("---cargo\n\"\n---\n{read} // \"\n"),
            format!("\n  \n---\n\"\n---\n{read} // \"\n"),
            format!("\u{B}---\n\"\n---\n{read} // \"\n"),
            format!("\u{C}\n---\n\"\n---\n{read} // \"\n"),
            format!("\u{85}---\n\"\n---\n{read} // \"\n"),
            "----\n[package]\n----\nfn main() {}\n".to_owned(),
        ] {
            assert_eq!(
                before_compiling(&block, Edition::E2024),
                Scan::Read(Found::Unscannable(FRONTMATTER)),
                "{block:?}"
            );
        }
        assert_eq!(
            before_compiling(
                "fn main() {}\nconst S: &str = \"\n---\n\";\n",
                Edition::E2024
            ),
            Scan::Clean,
            "a `---` after the first line isn't frontmatter"
        );
    }

    /// Asserts that `block` is refused under each of `editions`, as the `#` that rustc reads
    /// after the identifier `cr` before edition 2021.
    fn assert_hash_in_call_refused(block: &str, editions: &[Edition]) {
        for &edition in editions {
            assert_eq!(
                before_compiling(block, edition),
                Scan::Read(Found::Unscannable("a `#` inside a macro call's input")),
                "{edition:?}: {block:?}"
            );
        }
    }

    /// The editions in which `c` and `cr` are identifiers rather than literal prefixes.
    const BEFORE_C_STRINGS: [Edition; 2] = [Edition::E2015, Edition::E2018];

    #[test]
    fn before_compiling_probe_e1_cr_before_2021_hides_no_include_str() {
        assert_hash_in_call_refused(
            "pub const A: &str = stringify!(cr#\" \");\n\
             pub const B: &str = include_str!(\"/outside/nope/e1.txt\"); // \"#;\n",
            &BEFORE_C_STRINGS,
        );
    }

    #[test]
    fn before_compiling_probe_e2_cr_before_2021_hides_no_include() {
        assert_hash_in_call_refused(
            "pub const A: &str = stringify!(cr#\" \");\n\
             include!(\"/outside/secret.rs\"); // \"#;\n",
            &BEFORE_C_STRINGS,
        );
    }

    #[test]
    fn before_compiling_probe_e3_cr_before_2021_hides_no_env() {
        assert_hash_in_call_refused(
            "pub const A: &str = stringify!(cr#\" \");\n\
             pub const B: &str = env!(\"HOME\"); // \"#;\n",
            &BEFORE_C_STRINGS,
        );
    }

    #[test]
    fn before_compiling_probe_e4_cr_with_two_hashes_before_2021_hides_no_include_str() {
        assert_hash_in_call_refused(
            "pub const A: &str = stringify!(cr##\" \"#);\n\
             pub const B: &str = include_str!(\"/outside/nope/e4.txt\"); // \"##;\n",
            &BEFORE_C_STRINGS,
        );
    }

    #[test]
    fn before_compiling_probe_e5_cr_before_2021_hides_no_path_module() {
        assert_hash_in_call_refused(
            "pub fn f() { println!(\"{}\", stringify!(cr#\" \")); }\n\
             #[path = \"/outside/secret.rs\"] mod x; // \"#\n",
            &BEFORE_C_STRINGS,
        );
    }

    #[test]
    fn before_compiling_c_is_an_identifier_before_2021_and_a_prefix_from_it() {
        let c_string = "pub const S: &core::ffi::CStr = c\"plain\";\n";
        let raw_c_string = "pub const S: &core::ffi::CStr = cr#\"a \"quoted\" word\"#;\n";
        for edition in [Edition::E2021, Edition::E2024] {
            assert_eq!(
                before_compiling(c_string, edition),
                Scan::Clean,
                "{edition:?}"
            );
            assert_eq!(
                before_compiling(raw_c_string, edition),
                Scan::Clean,
                "{edition:?}"
            );
        }
        for edition in BEFORE_C_STRINGS {
            assert_eq!(
                before_compiling(c_string, edition),
                Scan::Clean,
                "{edition:?}"
            );
            assert_eq!(
                before_compiling(raw_c_string, edition),
                Scan::Read(Found::Unscannable("a `#` that doesn't start an attribute")),
                "{edition:?}: `cr` then `#`"
            );
        }
        for edition in [
            Edition::E2015,
            Edition::E2018,
            Edition::E2021,
            Edition::E2024,
        ] {
            assert_eq!(
                before_compiling("pub const R: &[u8] = br#\"a \"b\"\"#;\n", edition),
                Scan::Clean,
                "{edition:?}: `br` is a prefix in every edition"
            );
        }
    }

    /// Asserts that calling the macro renamed to `name` is refused under each of `editions`,
    /// in which `name` is an identifier and its `!` negates nothing.
    fn assert_renamed_call_refused(name: &str, editions: &[Edition]) {
        let block = format!("use std::thread_local as {name};\n{name}!{{ static X: u8 = 1; }}\n");
        for &edition in editions {
            assert_eq!(
                before_compiling(&block, edition),
                Scan::Read(Found::UnlistedMacro(name.to_owned())),
                "{edition:?}: {block:?}"
            );
        }
    }

    #[test]
    fn before_compiling_probe_g1_gen_is_a_macro_name_before_2024() {
        assert_renamed_call_refused("gen", &[Edition::E2015, Edition::E2018, Edition::E2021]);
    }

    #[test]
    fn before_compiling_probe_g2_dyn_is_a_macro_name_in_2015() {
        assert_renamed_call_refused("dyn", &[Edition::E2015]);
    }

    #[test]
    fn before_compiling_probe_g3_async_is_a_macro_name_in_2015() {
        assert_renamed_call_refused("async", &[Edition::E2015]);
        assert_renamed_call_refused("await", &[Edition::E2015]);
    }

    /// rustfmt's report on a line of a framed block passing a `max_width` grown from 100 to 104,
    /// as a nightly rustfmt with `error_on_line_overflow` prints it.
    const OVERFLOW_REPORT: &str = "\
error[internal]: line formatted, but exceeded maximum width (maximum: 104 (see `max_width` option), found: 133)
 --> <stdin>:2:2:105
  |
2 |     let v = xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx.yyyyyyyyyy;
  |                                                                                                         ^^^^^^^^^^
  |

warning: rustfmt has failed to format. See previous 1 errors.
";

    #[test]
    fn a_framed_overflow_report_shows_the_projects_width_and_the_blocks_column() {
        let rustfmt =
            crate::code_blocks::tools::formatter(crate::config::code_blocks::Language::Rust)
                .expect("Rust has a formatter");
        let frame = crate::code_blocks::fragment::Frame::of(
            rustfmt,
            Some(crate::code_blocks::fragment::Kind::Statements),
        )
        .expect("rustfmt reads statements inside a frame");

        let restated = String::from_utf8(unwidened(OVERFLOW_REPORT.as_bytes(), 4, 4))
            .expect("the restated report should stay UTF-8");
        let shown = frame.hidden(&restated, 1, "    ");

        assert!(
            restated.contains("(maximum: 100 (see `max_width` option), found: 129)"),
            "{restated}"
        );
        assert!(restated.contains(" --> <stdin>:2:105\n"), "{restated}");
        assert!(shown.contains(" --> <stdin>:1:101\n"), "{shown}");
        assert!(shown.contains("1 | let v = x"), "{shown}");
        assert!(!shown.contains("104") && !shown.contains("133"), "{shown}");
    }

    #[test]
    fn only_an_overflow_reports_column_moves_past_the_wider_width() {
        let tabbed = String::from_utf8(unwidened(OVERFLOW_REPORT.as_bytes(), 4, 1))
            .expect("the restated report should stay UTF-8");
        let trailing = "error[internal]: left behind trailing whitespace\n --> <stdin>:3:3:9\n";
        let other = String::from_utf8(unwidened(trailing.as_bytes(), 4, 4))
            .expect("the restated report should stay UTF-8");

        assert!(tabbed.contains(" --> <stdin>:2:102\n"), "{tabbed}");
        assert_eq!(
            other,
            "error[internal]: left behind trailing whitespace\n --> <stdin>:3:9\n"
        );
        assert_eq!(
            unwidened(b"\xff --> <stdin>:2:2:105", 4, 4),
            b"\xff --> <stdin>:2:2:105"
        );
    }

    #[test]
    fn dropping_an_emit_dir_whose_emit_directory_became_a_link_removes_nothing_it_leads_to() {
        let cache = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let neutral = cache.path().join("cwd");
        std::fs::create_dir(&neutral).expect("the cwd should be creatable");
        let emit = EmitDir::create(&neutral).expect("the emit dir should be creatable");
        let dir = emit
            .file()
            .parent()
            .expect("the file is in the dir")
            .to_path_buf();
        let base = dir.parent().expect("the dir is in emit").to_path_buf();
        let name = dir.file_name().expect("the run's name");
        let kept = outside.path().join(name).join("kept");
        std::fs::create_dir_all(outside.path().join(name)).expect("the outside twin");
        std::fs::write(&kept, "kept").expect("the outside file");
        std::fs::rename(&base, cache.path().join("emit-moved")).expect("the move");
        crate::cli::testing::symlink(outside.path(), &base).expect("the planted link");

        drop(emit);

        assert_eq!(std::fs::read_to_string(&kept).ok().as_deref(), Some("kept"));
    }
}
