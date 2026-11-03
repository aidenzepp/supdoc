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

/// The compile-time read `input`, a Rust block, holds, spelled as written: a call of `include!`,
/// `include_str!`, `include_bytes!`, `env!` or `option_env!`, with any whitespace or comment
/// before the `!`, or an attribute with a `path =` in it (`#[path = ...]`, `#[cfg_attr(.., path
/// = ...)]`). `None` when it holds none.
///
/// The scan reads every position of the text, comments and strings included, so a comment or a
/// string can only add a match, never hide one.
#[must_use]
pub(crate) fn compile_time_read(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    for (start, _) in input.char_indices() {
        let before = start.checked_sub(1).and_then(|at| bytes.get(at));
        if before.is_some_and(|byte| is_ident_byte(*byte)) {
            continue;
        }
        let rest = input.get(start..).unwrap_or_default();
        let word_end = rest.find(|c: char| !is_ident_char(c)).unwrap_or(rest.len());
        let word = rest.get(..word_end).unwrap_or_default();
        if COMPILE_TIME_READS.contains(&word)
            && skip_trivia(rest.get(word_end..).unwrap_or_default()).starts_with('!')
        {
            return Some(format!("{word}!"));
        }
        if rest.starts_with('#') && attribute_has_path(rest.get(1..).unwrap_or_default()) {
            return Some("#[path]".to_owned());
        }
    }
    None
}

/// Whether `after_hash`, the text after a `#`, opens an attribute (an optional `!`, then `[`,
/// trivia allowed between) whose bracketed body holds the word `path` followed by `=`.
fn attribute_has_path(after_hash: &str) -> bool {
    let mut rest = skip_trivia(after_hash);
    if let Some(inner) = rest.strip_prefix('!') {
        rest = skip_trivia(inner);
    }
    let Some(body) = rest.strip_prefix('[') else {
        return false;
    };
    let mut depth = 1_usize;
    let end = body
        .char_indices()
        .find(|(_, c)| {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
            depth == 0
        })
        .map_or(body.len(), |(at, _)| at);
    let body = body.get(..end).unwrap_or_default();
    body.match_indices("path").any(|(at, _)| {
        let before = at
            .checked_sub(1)
            .and_then(|before| body.as_bytes().get(before));
        let after = body.get(at + "path".len()..).unwrap_or_default();
        !before.is_some_and(|byte| is_ident_byte(*byte))
            && !after.starts_with(is_ident_char)
            && skip_trivia(after).starts_with('=')
    })
}

/// `text` after any leading whitespace, `//` line comments and (nested) `/* */` block comments.
fn skip_trivia(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        if let Some(comment) = trimmed.strip_prefix("//") {
            text = comment
                .find('\n')
                .map_or("", |end| comment.get(end..).unwrap_or_default());
        } else if trimmed.starts_with("/*") {
            text = after_block_comment(trimmed);
        } else {
            return trimmed;
        }
    }
}

/// `text`, which starts with `/*`, after the block comment it opens, nesting as Rust does. Empty
/// when the comment never closes.
fn after_block_comment(text: &str) -> &str {
    let mut depth = 0_usize;
    let mut at = 0;
    while let Some(rest) = text.get(at..) {
        if rest.starts_with("/*") {
            depth += 1;
            at += 2;
        } else if rest.starts_with("*/") {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return text.get(at..).unwrap_or_default();
            }
        } else if let Some(c) = rest.chars().next() {
            at += c.len_utf8();
        } else {
            break;
        }
    }
    ""
}

/// Whether `c` can be part of a Rust identifier, by the ASCII rule a token scan needs.
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `byte` can be part of an ASCII Rust identifier.
fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {

    use super::{compile_time_read, config_args, format_args};

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
            assert_eq!(compile_time_read(block).as_deref(), Some(read), "{block:?}");
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
            assert_eq!(compile_time_read(block), None, "{block:?}");
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
