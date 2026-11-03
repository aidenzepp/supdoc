//! Rust's format and lint steps.

use std::ffi::OsString;
use std::path::Path;

use crate::config::code_blocks::Edition;
use crate::finding::{IndexedText, Location};

use super::block::CodeBlock;

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

/// A Rust block's hidden lines: each one's 0-based line in the checked text, and its text there.
/// No line is added or removed, so the checked text's lines are the block's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hidden {
    /// Each hidden line, in order: its line in the checked text and its text without `# `.
    lines: Vec<(usize, String)>,
}

/// `input`, a Rust block with every line LF-ended, as its tools check it, with its hidden lines.
/// A line that is `#` alone or starts with `# ` is hidden and checked without that prefix; a line
/// starting `##` is checked without its first `#`; `#[` and `#!` are ordinary lines.
#[must_use]
pub(crate) fn unhide(input: &str) -> (String, Hidden) {
    let mut checked = String::with_capacity(input.len());
    let mut lines = Vec::new();
    for (at, line) in input.split_terminator('\n').enumerate() {
        let text = if line.starts_with("##") {
            line.get(1..).unwrap_or_default()
        } else if let Some(text) = (line == "#").then_some("").or(line.strip_prefix("# ")) {
            lines.push((at, text.to_owned()));
            text
        } else {
            line
        };
        checked.push_str(text);
        checked.push('\n');
    }
    (checked, Hidden { lines })
}

impl Hidden {
    /// `output`, a formatter's output for the checked text with no final line ending, written
    /// back as the block: each hidden line's `# ` restored, and a `#` added before a shown line
    /// that would otherwise read as hidden or escaped.
    ///
    /// # Errors
    ///
    /// The checked-text line of the first hidden line that can't be matched one-to-one with a
    /// line of `output`: its text changed, it was removed, or its place among `output`'s lines
    /// is ambiguous, so the formatter added, removed or reordered hidden lines.
    pub(crate) fn rehide(&self, output: &str) -> Result<String, usize> {
        let lines: Vec<&str> = output.split('\n').collect();
        let earliest = first_matches(lines.iter().copied().enumerate(), self.lines.iter())?;
        let mut latest = first_matches(
            lines.iter().copied().enumerate().rev(),
            self.lines.iter().rev(),
        )?;
        latest.reverse();
        if let Some(((at, _), _)) = self
            .lines
            .iter()
            .zip(earliest.iter().zip(&latest))
            .find(|(_, (early, late))| early != late)
        {
            return Err(*at);
        }
        let mut hidden = earliest.into_iter().peekable();
        let written: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(at, line)| {
                if hidden.next_if_eq(&at).is_some() {
                    if line.is_empty() {
                        "#".to_owned()
                    } else {
                        format!("# {line}")
                    }
                } else if *line == "#" || line.starts_with("# ") || line.starts_with("##") {
                    format!("#{line}")
                } else {
                    (*line).to_owned()
                }
            })
            .collect();
        Ok(written.join("\n"))
    }
}

/// The output line each of `hidden` matches, walking `lines` in the given order and taking the
/// first equal line each time.
///
/// # Errors
///
/// The checked-text line of the first hidden line with no match.
fn first_matches<'output, 'hidden>(
    mut lines: impl Iterator<Item = (usize, &'output str)>,
    hidden: impl Iterator<Item = &'hidden (usize, String)>,
) -> Result<Vec<usize>, usize> {
    hidden
        .map(|(at, text)| {
            lines
                .find(|(_, line)| line == text)
                .map(|(found, _)| found)
                .ok_or(*at)
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
    /// An attribute with `path =` in it.
    PathAttribute,
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
/// each token is judged from the one or two before it. The scan fails closed: an unterminated
/// block comment, string or character literal, or a first line starting `#!` but not `#![`, is
/// [`Found::Unscannable`]. Identifiers are ASCII, so any other character ends a word, which can
/// only add matches.
#[must_use]
pub(crate) fn compile_time_read(input: &str) -> Scan {
    if input.len() > MAX_SCAN_BYTES {
        return Scan::TooLarge;
    }
    scan(input).map_or(Scan::Clean, Scan::Read)
}

/// The first compile-time read in `input`, by [`compile_time_read`]'s rules, without the size cap.
fn scan(input: &str) -> Option<Found> {
    if input.starts_with("#!") && !input.starts_with("#![") {
        return Some(Found::Unscannable(
            "a `#!` first line that isn't an inner attribute",
        ));
    }
    let mut before = Token::Other;
    let mut before_that = Token::Other;
    // The bracket depth inside the attribute being read, or 0 outside one.
    let mut attribute = 0_usize;
    let mut tokens = Tokens::new(input);
    for token in tokens.by_ref() {
        match token {
            Token::Punct('!') => {
                if let Token::Ident(word) = before
                    && COMPILE_TIME_READS.contains(&word)
                {
                    return Some(Found::Macro(word.to_owned()));
                }
            }
            Token::Punct('[') if attribute > 0 => attribute += 1,
            Token::Punct('[') => {
                let opens = before == Token::Punct('#')
                    || (before == Token::Punct('!') && before_that == Token::Punct('#'));
                if opens {
                    attribute = 1;
                }
            }
            Token::Punct(']') => attribute = attribute.saturating_sub(1),
            Token::Punct('=') if attribute > 0 && before == Token::Ident("path") => {
                return Some(Found::PathAttribute);
            }
            _ => {}
        }
        before_that = before;
        before = token;
    }
    tokens.unterminated
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

#[cfg(test)]
mod tests {

    use std::time::{Duration, Instant};

    use super::{
        Found, LineMap, MAX_SCAN_BYTES, Scan, attribute_edition, compile_time_read, config_args,
        format_args, scan, unhide,
    };
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::code_blocks::block::blocks_in;
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
            let _found = scan(&block);
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
}
