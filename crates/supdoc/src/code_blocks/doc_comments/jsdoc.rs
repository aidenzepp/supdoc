//! Doc comments of JavaScript, TypeScript and TSX files: the `/** ... */` containers.
//!
//! A comment's body, without `/**`, `*/` and each line's gutter, is the Markdown text. A line's
//! gutter is its leading whitespace plus an optional `*` and one space or tab after it. `/* */` and
//! `//` comments are not containers. Fenced blocks are blocks, anywhere in the comment; an untagged
//! fence is text.

use std::ops::Range;

use crate::finding::Location;
use crate::read::cartridge::{MarkdownRegion, SourceFile};
use crate::read::error::ReadError;

use super::super::block::CodeBlock;
use super::{Line, Run, blocks_of_runs};

/// Every code block in the `/** */` comments of `file`, a JavaScript, TypeScript or TSX file, in
/// file order.
///
/// # Errors
///
/// The [`ReadError`] of the first comment whose Markdown, or its NUL-replaced re-parse, can't be
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
        .filter_map(|range| jsdoc_run(text, range.start_byte..range.end_byte))
        .collect();
    blocks_of_runs(file, runs, locate)
}

/// The comment `comment` of `text` as a [`Run`] and its Markdown text. `None` unless it opens with
/// `/**` and not `/***`, and isn't the empty `/**/`.
fn jsdoc_run(text: &str, comment: Range<usize>) -> Option<(Run, String)> {
    let source = text.get(comment.clone())?;
    let after_open = source.strip_prefix("/**")?;
    if after_open.starts_with(['*', '/']) {
        return None;
    }
    let body = after_open.strip_suffix("*/").unwrap_or(after_open);
    let start = comment.start + "/**".len();
    Some(run_of(text, body, start))
}

/// The [`Run`] and Markdown text of `body`, a `/** */` comment without its `/**` and `*/`, which
/// starts at `start` in `text`.
///
/// Line 1 is the text after `/**`; the last line ends before `*/`. Each line loses its gutter.
fn run_of(text: &str, body: &str, start: usize) -> (Run, String) {
    let mut lines = Vec::new();
    let mut markdown = String::new();
    let mut at = start;
    for (number, line) in body.split('\n').enumerate() {
        let begins = text
            .get(..at)
            .and_then(|head| head.rfind('\n'))
            .map_or(0, |newline| newline + 1);
        let skipped = gutter_len(line);
        if number > 0 {
            markdown.push('\n');
        }
        lines.push(Line {
            synthetic: markdown.len(),
            file: at + skipped,
            // A line's prefix is what lies before its text on its file line: for line 1, `/**`
            // and the indentation before it too.
            begins: if number == 0 { begins } else { at },
        });
        markdown.push_str(line.get(skipped..).unwrap_or_default());
        at += line.len() + 1;
    }
    (Run { lines }, markdown)
}

/// The length of `line`'s gutter: its leading whitespace, then an optional `*` and one space or
/// tab after it.
fn gutter_len(line: &str) -> usize {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let Some(after_star) = line.get(indent..).and_then(|rest| rest.strip_prefix('*')) else {
        return indent;
    };
    let space = usize::from(after_star.starts_with([' ', '\t']));
    indent + 1 + space
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cartridges::{js, ts, tsx};
    use crate::code_blocks::block::{BodyLine, blocks_in, replacement_for};
    use crate::path::AbsolutePath;
    use crate::read::query::QueryCartridge;

    /// The blocks of `text` parsed as `/repo/src/<name>` with `cartridge`.
    fn blocks_with(name: &str, cartridge: &'static QueryCartridge, text: &str) -> Vec<CodeBlock> {
        let path = AbsolutePath::new(&Path::new("/repo/src").join(name))
            .expect("the fixture path should be absolute");
        let file = SourceFile::parse(path, text.to_owned(), cartridge)
            .expect("tree-sitter should return a tree");
        blocks_in(&file, |range| {
            let source = format!("src/{name}")
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        })
    }

    /// The blocks of `text` parsed as `/repo/src/lib.ts`.
    fn blocks_of(text: &str) -> Vec<CodeBlock> {
        let cartridge = ts::cartridge().expect("the built-in TypeScript queries should compile");
        blocks_with("lib.ts", cartridge, text)
    }

    /// The one block of `text`, a TypeScript file.
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

    /// The language tag of `block`'s info string.
    fn tag(block: &CodeBlock) -> Option<String> {
        block
            .info
            .as_ref()
            .map(|info| info.parsed().language.to_owned())
    }

    #[test]
    fn a_fenced_ts_block_in_a_tsdoc_comment_is_found_on_its_file_lines() {
        let text =
            "/**\n * Adds.\n *\n * ```ts\n * add(1, 2);\n * ```\n */\nexport function add() {}\n";

        let block = only(text);

        assert_eq!(block.fence.line, 4, "the fence opens on line 4");
        assert_eq!(tag(&block).as_deref(), Some("ts"));
        assert_eq!(lines(text, &block), [(" * ", "add(1, 2);")]);
    }

    #[test]
    fn a_fenced_block_is_found_in_a_js_and_a_tsx_file() {
        let text = "/**\n * ```js\n * f();\n * ```\n */\nfunction f() {}\n";
        let js = js::cartridge().expect("the built-in JavaScript queries should compile");
        let tsx = tsx::cartridge().expect("the built-in TSX queries should compile");

        for (name, cartridge) in [("lib.js", js), ("view.tsx", tsx)] {
            let blocks = blocks_with(name, cartridge, text);

            let found: Vec<_> = blocks.iter().map(|block| lines(text, block)).collect();
            assert_eq!(found, [[(" * ", "f();")]], "{name}");
        }
    }

    #[test]
    fn a_fence_inside_an_example_section_is_the_only_block() {
        let text = "/**\n * @example\n * Call it:\n * ```ts\n * f();\n * ```\n * @returns nothing\n */\nfunction f() {}\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [(" * ", "f();")]);
    }

    #[test]
    fn an_example_section_without_a_fence_is_not_a_block_yet() {
        let text = "/**\n * @example\n * f();\n */\nfunction f() {}\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn plain_block_and_line_comments_are_not_containers() {
        let text =
            "/* ```ts\n x;\n ``` */\n// ```ts\n// x;\n// ```\n/*** ```ts\n x;\n ``` */\n/**/\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn a_gutterless_comment_keeps_each_lines_indentation_as_its_prefix() {
        let text = "/**\n  Adds.\n\n  ```ts\n  add(1, 2);\n  ```\n*/\n";

        assert_eq!(lines(text, &only(text)), [("  ", "add(1, 2);")]);
    }

    #[test]
    fn a_close_on_the_last_code_line_ends_the_block_before_it() {
        let text = "/**\n * ```ts\n * f(); */\nfunction f() {}\n";

        assert_eq!(lines(text, &only(text)), [(" * ", "f(); ")]);
    }

    #[test]
    fn an_untagged_fence_is_text() {
        let block = only("/**\n * ```\n * f();\n * ```\n */\n");

        assert_eq!(block.info, None);
        assert_eq!(block.default_language(), None);
    }

    #[test]
    fn a_replacement_keeps_a_methods_gutter() {
        let text = "class C {\n  /**\n   * ```ts\n   * old\n   * ```\n   */\n  f() {}\n}\n";
        let block = only(text);

        let (range, replacement) =
            replacement_for(&block, text, "a\n\nb").expect("the block has a body to replace");

        assert_eq!(replacement, "   * a\n   *\n   * b");
        assert_eq!(text.get(range), Some("   * old"));
    }

    #[test]
    fn a_replacement_after_a_blank_gutter_line_keeps_the_gutters_space() {
        let text = "/**\n * ```ts\n *\n * old\n * ```\n */\n";
        let block = only(text);

        let (_, replacement) =
            replacement_for(&block, text, "a\nb").expect("the block has a body to replace");

        assert_eq!(replacement, " * a\n * b");
    }

    #[test]
    fn an_empty_block_is_filled_behind_the_gutter() {
        let text = "/**\n * ```ts\n * ```\n */\n";
        let block = only(text);

        let (range, replacement) =
            replacement_for(&block, text, "a").expect("an empty block gets an insertion");

        assert_eq!(replacement, " * a\n");
        assert_eq!(
            range.start,
            text.find(" * ```\n */").expect("the closing fence")
        );
    }

    #[test]
    fn a_crlf_file_keeps_its_line_endings_in_the_prefix_map() {
        let text = "/**\r\n * ```ts\r\n * f();\r\n * ```\r\n */\r\n";

        assert_eq!(lines(text, &only(text)), [(" * ", "f();\r")]);
    }
}
