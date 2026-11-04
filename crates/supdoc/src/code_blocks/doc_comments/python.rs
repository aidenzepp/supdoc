//! Python docstrings: the string that is the first statement of a module, class or function.
//!
//! A docstring's body, without its quotes and its common indentation, is the Markdown text.
//! `#` comments are not containers. Only fenced blocks are blocks: a `>>>` session and an
//! indented reST literal block are not.

use std::ops::Range;

use crate::finding::Location;
use crate::read::cartridge::{MarkdownRegion, SourceFile};
use crate::read::error::ReadError;

use super::super::block::CodeBlock;
use super::{Line, Run, blocks_of_runs};

/// Every code block in the docstrings of `file`, a Python file, in file order.
///
/// # Errors
///
/// The [`ReadError`] of the first docstring whose Markdown, or its NUL-replaced re-parse, can't be
/// parsed: the file's blocks are then unknown, not none.
pub(in super::super) fn blocks(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let text = file.text();
    let regions = file.language().markdown_regions(file)?;
    let runs = regions
        .iter()
        .flat_map(MarkdownRegion::ranges)
        .filter_map(|range| docstring_run(text, range.start_byte..range.end_byte))
        .collect();
    blocks_of_runs(file, runs, locate)
}

/// The docstring `string` of `text` as a [`Run`] and its Markdown text. `None` for a `#` comment
/// and for a string that isn't a plain text string (a `b` or `f` prefix).
fn docstring_run(text: &str, string: Range<usize>) -> Option<(Run, String)> {
    let source = text.get(string.clone())?;
    // A comment body starts right after its `#`.
    if text.get(..string.start)?.ends_with('#') {
        return None;
    }
    let after_prefix = source.trim_start_matches(['r', 'R', 'u', 'U']);
    let prefix_len = source.len() - after_prefix.len();
    let quote = after_prefix
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\''))?;
    let triple = after_prefix.len() >= 6 && after_prefix.starts_with(&quote.to_string().repeat(3));
    let quotes = if triple { 3 } else { 1 };
    let open_end = prefix_len + quotes;
    let closed =
        source.len() >= open_end + quotes && source.ends_with(&quote.to_string().repeat(quotes));
    let close_start = if closed {
        source.len() - quotes
    } else {
        source.len()
    };
    let body_start = string.start + open_end;
    let body = text.get(body_start..string.start + close_start)?;
    Some(run_of(text, body, body_start))
}

/// The [`Run`] and Markdown text of the docstring `body`, which starts at `start` in `text`.
///
/// Line 1 is the text after the opening quotes. Each later line loses the common indentation of
/// the non-blank lines after the first, as `inspect.cleandoc` computes it; a blank line keeps all
/// its text and has an empty prefix.
fn run_of(text: &str, body: &str, start: usize) -> (Run, String) {
    let indent_of = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();
    let is_blank = |line: &str| line.trim_matches([' ', '\t', '\r']).is_empty();
    let margin = body
        .split('\n')
        .skip(1)
        .filter(|line| !is_blank(line))
        .map(indent_of)
        .min()
        .unwrap_or(0);
    let mut lines = Vec::new();
    let mut markdown = String::new();
    let mut at = start;
    for (number, line) in body.split('\n').enumerate() {
        let begins = text
            .get(..at)
            .and_then(|head| head.rfind('\n'))
            .map_or(0, |newline| newline + 1);
        let skipped = if number == 0 || is_blank(line) {
            0
        } else {
            margin
        };
        if number > 0 {
            markdown.push('\n');
        }
        lines.push(Line {
            synthetic: markdown.len(),
            file: at + skipped,
            // A line's prefix is what lies before its text on its file line: for line 1, the
            // opening quotes too.
            begins: if number == 0 { begins } else { at },
        });
        markdown.push_str(line.get(skipped..).unwrap_or_default());
        at += line.len() + 1;
    }
    (Run { lines }, markdown)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cartridges::python::cartridge;
    use crate::code_blocks::block::{BodyLine, blocks_in, replacement_for};
    use crate::path::AbsolutePath;

    /// The blocks of `text` parsed as `/repo/src/lib.py`.
    fn blocks_of(text: &str) -> Vec<CodeBlock> {
        let path = AbsolutePath::new(Path::new("/repo/src/lib.py"))
            .expect("the fixture path should be absolute");
        let python = cartridge().expect("the built-in Python queries should compile");
        let file = SourceFile::parse(path, text.to_owned(), python)
            .expect("tree-sitter should return a tree");
        blocks_in(&file, |range| {
            let source = "src/lib.py"
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        })
    }

    /// The one block of `text`.
    fn only(text: &str) -> CodeBlock {
        let mut blocks = blocks_of(text);
        assert_eq!(blocks.len(), 1, "blocks: {blocks:?}");
        blocks.remove(0)
    }

    /// Each body line of `block`: its prefix and content in `text`.
    fn lines<'text>(text: &'text str, block: &CodeBlock) -> Vec<(&'text str, &'text str)> {
        let slice = |range: &Range<usize>| text.get(range.clone()).expect("a range inside text");
        block
            .body
            .iter()
            .map(|BodyLine { prefix, content }| (slice(prefix), slice(content)))
            .collect()
    }

    #[test]
    fn a_module_docstring_block_is_found() {
        let text = "\"\"\"Mod.\n\n```python\nx = 1\n```\n\"\"\"\n";

        let block = only(text);

        assert_eq!(block.fence.line, 3, "the fence opens on line 3");
        assert_eq!(lines(text, &block), [("", "x = 1")]);
    }

    #[test]
    fn a_class_and_a_function_docstring_block_is_found() {
        let text = "class C:\n    \"\"\"C.\n\n    ```python\n    a\n    ```\n    \"\"\"\n\n    def f(self):\n        '''F.\n\n        ```python\n        b\n        ```\n        '''\n";

        let blocks = blocks_of(text);

        let found: Vec<_> = blocks.iter().map(|block| lines(text, block)).collect();
        assert_eq!(found, [[("    ", "a")], [("        ", "b")]]);
    }

    #[test]
    fn a_raw_docstring_block_is_found() {
        let text =
            "def f():\n    r\"\"\"R.\n\n    ```python\n    re = '\\d'\n    ```\n    \"\"\"\n";

        assert_eq!(lines(text, &only(text)), [("    ", "re = '\\d'")]);
    }

    #[test]
    fn a_replacement_keeps_the_docstrings_indentation() {
        let text = "def f():\n    \"\"\"F.\n\n    ```python\n    old\n    ```\n    \"\"\"\n";
        let block = only(text);

        let (range, replacement) =
            replacement_for(&block, text, "a\n\nb").expect("the block has a body to replace");

        assert_eq!(replacement, "    a\n\n    b");
        assert_eq!(text.get(range), Some("    old"));
    }

    #[test]
    fn a_one_line_docstring_has_no_block() {
        assert_eq!(blocks_of("def f():\n    \"\"\"One line.\"\"\"\n"), []);
    }

    #[test]
    fn a_doctest_session_is_not_a_block() {
        let text = "def f():\n    \"\"\"F.\n\n    >>> f()\n    1\n    \"\"\"\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn an_indented_literal_block_is_not_a_block() {
        let text = "def f():\n    \"\"\"F::\n\n        x = 1\n    \"\"\"\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn a_string_that_is_not_the_first_statement_is_not_a_docstring() {
        let text =
            "def f():\n    x = 1\n    \"\"\"Not.\n\n    ```python\n    y\n    ```\n    \"\"\"\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn a_comment_is_not_a_container() {
        assert_eq!(blocks_of("#```python\n# x\n#```\n"), []);
    }

    #[test]
    fn a_fence_never_closed_runs_to_the_end_of_the_docstring() {
        let text = "def f():\n    \"\"\"F.\n\n    ```python\n    x\n    \"\"\"\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [("    ", "x"), ("", "    ")]);
    }

    #[test]
    fn a_crlf_file_keeps_its_line_endings_in_the_prefix_map() {
        let text =
            "def f():\r\n    \"\"\"F.\r\n\r\n    ```python\r\n    x\r\n    ```\r\n    \"\"\"\r\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [("    ", "x\r")]);
    }
}
