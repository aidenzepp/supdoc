//! Rust's format and lint steps.

use std::ffi::OsString;
use std::path::Path;

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

/// `rustfmt`'s arguments for any block: `--edition 2024 --emit stdout`. The block arrives on
/// stdin as written, with no wrapping; rustfmt takes no stdin file name, so the doc and the
/// block's index don't appear.
#[must_use]
pub(crate) fn format_args() -> Vec<OsString> {
    ["--edition", "2024", "--emit", "stdout"]
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

/// The largest block, in bytes, the compile-time-read scan reads. A bigger one is not checked.
pub(crate) const MAX_SCAN_BYTES: usize = 256 * 1024;

/// What the compile-time-read scan found in a Rust block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scan {
    /// The block holds no compile-time read.
    Clean,
    /// The block holds this compile-time read, spelled as `include_str!` or `#[path]`.
    Read(String),
    /// The block is over [`MAX_SCAN_BYTES`], so it wasn't scanned.
    TooLarge,
}

/// Scans `input`, a Rust block, for a compile-time read: a call of `include!`, `include_str!`,
/// `include_bytes!`, `env!` or `option_env!` (comments and whitespace may sit before the `!`,
/// and `r#env` counts), or an attribute with `path =` anywhere in its brackets (`#[path = ..]`,
/// `#![path = ..]`, `#[cfg_attr(.., path = ..)]`). [`Scan::TooLarge`] past [`MAX_SCAN_BYTES`].
///
/// One linear pass: [`Tokens`] skips comments and string and character literals once each, and
/// each token is judged from the one or two before it.
#[must_use]
pub(crate) fn compile_time_read(input: &str) -> Scan {
    if input.len() > MAX_SCAN_BYTES {
        return Scan::TooLarge;
    }
    scan(input).map_or(Scan::Clean, Scan::Read)
}

/// The first compile-time read in `input`, by [`compile_time_read`]'s rules, without the size cap.
fn scan(input: &str) -> Option<String> {
    let mut before = Token::Other;
    let mut before_that = Token::Other;
    // The bracket depth inside the attribute being read, or 0 outside one.
    let mut attribute = 0_usize;
    for token in Tokens::new(input) {
        match token {
            Token::Punct('!') => {
                if let Token::Ident(word) = before
                    && COMPILE_TIME_READS.contains(&word)
                {
                    return Some(format!("{word}!"));
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
                return Some("#[path]".to_owned());
            }
            _ => {}
        }
        before_that = before;
        before = token;
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
}

impl<'text> Tokens<'text> {
    /// The tokens of `text`.
    const fn new(text: &'text str) -> Self {
        Self { text, at: 0 }
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
            self.bump_while(char::is_whitespace);
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
    /// literals, and anything else is a lifetime or label.
    fn quote(&mut self) {
        self.bump(1);
        if self.peek(0) == Some('\\') {
            self.bump(2);
            self.bump_while(|c| c != '\'' && c != '\n');
            self.bump(1);
        } else if self.peek(1) == Some('\'') {
            self.bump(2);
        } else {
            self.bump_while(is_ident_char);
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

/// Whether `c` can be part of a Rust identifier or number.
fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {

    use std::time::{Duration, Instant};

    use super::{MAX_SCAN_BYTES, Scan, compile_time_read, config_args, format_args, scan};

    #[test]
    fn rustfmt_reads_the_block_as_2024_and_prints_it() {
        assert_eq!(format_args(), ["--edition", "2024", "--emit", "stdout"]);
    }

    #[test]
    fn compile_time_reads_are_found_however_they_are_spaced() {
        for (block, read) in [
            (
                "const S: &str = include_str!(\"/etc/passwd\");\n",
                "include_str!",
            ),
            ("fn f() { let _ = std::env!(\"HOME\"); }\n", "env!"),
            ("fn f() { option_env /* x */ !(\"A\"); }\n", "option_env!"),
            ("include_bytes\n!(\"x\");\n", "include_bytes!"),
            ("r#include!(\"x\");\n", "include!"),
            ("#[path = \"/outside.rs\"]\nmod m;\n", "#[path]"),
            ("# [ cfg_attr(all(), path=\"/o.rs\") ]\nmod m;\n", "#[path]"),
        ] {
            assert_eq!(
                compile_time_read(block),
                Scan::Read(read.to_owned()),
                "{block:?}"
            );
        }
    }

    #[test]
    fn ordinary_uses_of_the_same_words_are_not_reads() {
        for block in [
            "fn main() { let path = std::env::args(); }\n",
            "use std::env;\nfn f() -> String { env::var(\"A\").unwrap_or_default() }\n",
            "#[serde(rename = \"path\")]\nstruct S;\n",
            "fn environment() {}\nmacro_rules! my_env { () => {} }\nmy_env!();\n",
        ] {
            assert_eq!(compile_time_read(block), Scan::Clean, "{block:?}");
        }
    }

    #[test]
    fn reads_in_strings_and_comments_are_not_code_and_cant_hide_code() {
        for block in [
            "const S: &str = \"env!(x) #[path = y]\";\n",
            "const S: &str = r#\"include!(\"x\")\"#;\n// env!(\"A\")\n/* include_str!(\"b\") */\n",
            "fn f<'a>(c: char) -> bool { c == '\"' || c == '\\'' }\n",
        ] {
            assert_eq!(compile_time_read(block), Scan::Clean, "{block:?}");
        }
        let after_a_string = "const S: &str = \"/*\"; const T: &str = env!(\"HOME\"); // */\n";
        assert_eq!(
            compile_time_read(after_a_string),
            Scan::Read("env!".to_owned())
        );
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
