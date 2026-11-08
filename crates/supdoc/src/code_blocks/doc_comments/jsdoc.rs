//! Doc comments of JavaScript, TypeScript and TSX files: the `/** ... */` containers.
//!
//! A comment's body, without `/**`, `*/` and each line's prefix, is the Markdown text. A line with
//! a `*` gutter loses its leading whitespace, the `*` and one space or tab after it. Every other
//! non-blank line loses the leading whitespace all of them share, so code indented inside a fence
//! keeps its indentation. `/* */` and `//` comments are not containers. Fenced blocks are blocks,
//! anywhere in the comment; an untagged fence is text. An `@example` section with no fence in it
//! is one block in the file's own language, since the tag defines that text as code.

use std::ops::Range;

use crate::finding::Location;
use crate::read::cartridge::{MarkdownRegion, SourceFile};
use crate::read::error::ReadError;

use super::super::block::{BodyLine, CodeBlock, Info};
use super::{Line, Run, blocks_of_runs};

/// The block tag that opens an example section.
const EXAMPLE: &str = "@example";

/// Every code block in the `/** */` comments of `file`, a JavaScript, TypeScript or TSX file, in
/// file order: its fenced blocks and its [unfenced `@example` sections](examples).
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
    let runs: Vec<_> = regions
        .iter()
        .flat_map(MarkdownRegion::ranges)
        .filter_map(|range| jsdoc_run(text, range.start_byte..range.end_byte))
        .collect();
    let mut examples: Vec<_> = file_tag(file)
        .map(|tag| {
            runs.iter()
                .flat_map(|(run, markdown)| examples(run, markdown, &tag, &locate))
                .collect()
        })
        .unwrap_or_default();
    let mut blocks = blocks_of_runs(file, runs, &locate)?;
    blocks.append(&mut examples);
    blocks.sort_by_key(|block| block.fence.range.start);
    Ok(blocks)
}

/// The language tag of `file`'s own language: its extension, lowercase, which names JSX and TSX
/// apart from plain JavaScript and TypeScript. `None` when it has no UTF-8 extension.
fn file_tag(file: &SourceFile) -> Option<String> {
    let extension = file.path().as_path().extension()?.to_str()?;
    Some(extension.to_ascii_lowercase())
}

/// The `@example` sections of `run`, whose Markdown text is `markdown`, that hold no fence, each
/// one block tagged `tag` and located by `locate`.
///
/// A section's body runs from the text after the tag and an optional `<caption>…</caption>` on
/// its line, or else from the next line, to the line before the next block tag (a line whose text
/// starts with `@`) or the comment's end. Its leading and trailing blank lines are not part of it,
/// and a section with no other line is no block. The block's fence spans the tag through its last
/// line, and its info is `tag` located at the tag, so it reads as the file's language and has no
/// attributes: an author opts out by writing the example as a fence.
fn examples(
    run: &Run,
    markdown: &str,
    tag: &str,
    locate: &impl Fn(Range<usize>) -> Option<Location>,
) -> Vec<CodeBlock> {
    let texts: Vec<&str> = markdown.split('\n').collect();
    let mut blocks = Vec::new();
    for (number, line) in texts.iter().enumerate() {
        let Some((at, after)) = example_tag(line) else {
            continue;
        };
        let end = texts
            .iter()
            .skip(number + 1)
            .position(|text| is_block_tag(text))
            .map_or(texts.len(), |found| number + 1 + found);
        let inline = line.get(after..).is_some_and(|rest| !is_blank(rest));
        let mut body: Vec<(usize, usize)> = inline.then_some((number, after)).into_iter().collect();
        body.extend((number + 1..end).map(|later| (later, 0)));
        let text_of = |&(line, offset): &(usize, usize)| {
            texts
                .get(line)
                .and_then(|text| text.get(offset..))
                .unwrap_or_default()
        };
        if body.iter().any(|part| opens_fence(text_of(part))) {
            continue;
        }
        let has_text = |part: &(usize, usize)| !is_blank(text_of(part));
        let trimmed = body
            .iter()
            .position(has_text)
            .zip(body.iter().rposition(has_text))
            .and_then(|(first, last)| body.get(first..=last));
        if let Some(block) =
            trimmed.and_then(|lines| example_block(run, &texts, (number, at), lines, tag, locate))
        {
            blocks.push(block);
        }
    }
    blocks
}

/// The block of an `@example` section of `run`, whose Markdown lines are `texts`: its tag at
/// offset `tag_at.1` of line `tag_at.0`, and its body lines, each a line and the offset its text
/// starts at. `None` when the body is empty or a range can't be located.
fn example_block(
    run: &Run,
    texts: &[&str],
    tag_at: (usize, usize),
    body: &[(usize, usize)],
    tag: &str,
    locate: &impl Fn(Range<usize>) -> Option<Location>,
) -> Option<CodeBlock> {
    let in_file = |line: usize, offset: usize| Some(run.lines.get(line)?.file + offset);
    let start = in_file(tag_at.0, tag_at.1)?;
    let lines = body
        .iter()
        .map(|&(line, offset)| {
            let content = in_file(line, offset)?;
            let length = texts.get(line)?.len().checked_sub(offset)?;
            Some(BodyLine {
                prefix: run.lines.get(line)?.begins..content,
                content: content..content + length,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let end = lines.last()?.content.end;
    Some(CodeBlock {
        fence: locate(start..end)?,
        info: Some(Info {
            text: tag.to_owned(),
            location: locate(start..start + EXAMPLE.len())?,
        }),
        body: lines,
    })
}

/// Where `line`, a line of a comment's Markdown text, starts the `@example` tag, and where the
/// text after the tag and an optional `<caption>…</caption>` starts, past its whitespace. `None`
/// when the line's text doesn't start with the tag.
fn example_tag(line: &str) -> Option<(usize, usize)> {
    let at = indent_len(line);
    let rest = line.get(at..)?.strip_prefix(EXAMPLE)?;
    if rest.starts_with(|c: char| !c.is_whitespace() && c != '<') {
        return None;
    }
    let mut after = line.len() - rest.len();
    if let Some(caption) = rest.trim_start().strip_prefix("<caption>") {
        after = caption.find("</caption>").map_or(line.len(), |close| {
            line.len() - caption.len() + close + "</caption>".len()
        });
    }
    let remainder = line.get(after..)?;
    Some((at, after + remainder.len() - remainder.trim_start().len()))
}

/// Whether `text`, a line of a comment's Markdown text, starts a block tag: `@` after its leading
/// whitespace.
fn is_block_tag(text: &str) -> bool {
    text.get(indent_len(text)..)
        .is_some_and(|rest| rest.starts_with('@'))
}

/// Whether `text` opens a fence: three backticks or tildes after its leading whitespace.
fn opens_fence(text: &str) -> bool {
    let rest = text.trim_start_matches([' ', '\t']);
    rest.starts_with("```") || rest.starts_with("~~~")
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
/// Line 1 is the text after `/**`, with no prefix; the last line ends before `*/`. A later line
/// with a `*` gutter loses it. A later line without one loses the leading whitespace shared, byte
/// for byte, by every non-blank line after the first that has no gutter. A blank line keeps all its
/// text and has an empty prefix.
fn run_of(text: &str, body: &str, start: usize) -> (Run, String) {
    let margin = shared_indent(body);
    let mut lines = Vec::new();
    let mut markdown = String::new();
    let mut at = start;
    for (number, line) in body.split('\n').enumerate() {
        let begins = text
            .get(..at)
            .and_then(|head| head.rfind('\n'))
            .map_or(0, |newline| newline + 1);
        let skipped = match star_gutter_len(line) {
            _ if number == 0 => 0,
            Some(gutter) => gutter,
            None if is_blank(line) => 0,
            None => margin,
        };
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

/// The length of the longest leading run of spaces and tabs that every non-blank line of `body`
/// after the first, and without a `*` gutter, starts with. Zero when there is no such line.
fn shared_indent(body: &str) -> usize {
    body.split('\n')
        .skip(1)
        .filter(|line| star_gutter_len(line).is_none() && !is_blank(line))
        .map(|line| line.get(..indent_len(line)).unwrap_or_default())
        .reduce(|shared, indent| {
            let common = shared
                .bytes()
                .zip(indent.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            shared.get(..common).unwrap_or_default()
        })
        .map_or(0, str::len)
}

/// The length of `line`'s `*` gutter: its leading whitespace, the `*`, and one space or tab after
/// it. `None` when the line's first character after its whitespace isn't `*`.
fn star_gutter_len(line: &str) -> Option<usize> {
    let indent = indent_len(line);
    let after_star = line.get(indent..)?.strip_prefix('*')?;
    let space = usize::from(after_star.starts_with([' ', '\t']));
    Some(indent + 1 + space)
}

/// The length of `line`'s leading run of spaces and tabs.
fn indent_len(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// Whether `line` holds only spaces, tabs and a CR.
fn is_blank(line: &str) -> bool {
    line.trim_matches([' ', '\t', '\r']).is_empty()
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cartridges::{js, ts, tsx};
    use crate::code_blocks::block::{BodyLine, blocks_in, replacement_for};
    use crate::read::query::QueryCartridge;

    /// The blocks of `text` parsed as `/repo/src/<name>` with `cartridge`.
    fn blocks_with(name: &str, cartridge: &'static QueryCartridge, text: &str) -> Vec<CodeBlock> {
        let path = crate::path::fixture(&format!("/repo/src/{name}"));
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
    fn an_example_section_without_a_fence_is_one_block_in_the_files_language() {
        let text = "/**\n * Calls.\n * @example\n * f();\n * g();\n */\nfunction f() {}\n";

        let block = only(text);

        assert_eq!(
            block.fence.line, 3,
            "the block's fence is its `@example` tag"
        );
        assert_eq!(tag(&block).as_deref(), Some("ts"));
        assert_eq!(lines(text, &block), [(" * ", "f();"), (" * ", "g();")]);
    }

    #[test]
    fn an_unfenced_example_in_a_js_file_is_js_and_in_a_tsx_file_is_tsx() {
        let text = "/**\n * @example\n * <A />;\n */\nfunction f() {}\n";
        let js = js::cartridge().expect("the built-in JavaScript queries should compile");
        let tsx = tsx::cartridge().expect("the built-in TSX queries should compile");

        for (name, cartridge, expected) in [("lib.js", js, "js"), ("view.tsx", tsx, "tsx")] {
            let blocks = blocks_with(name, cartridge, text);

            let tags: Vec<_> = blocks.iter().map(tag).collect();
            assert_eq!(tags, [Some(expected.to_owned())], "{name}");
        }
    }

    #[test]
    fn a_caption_is_not_part_of_the_example() {
        let text = "/**\n * @example <caption>Adding.</caption>\n * add(1, 2);\n */\n";

        assert_eq!(lines(text, &only(text)), [(" * ", "add(1, 2);")]);
    }

    #[test]
    fn code_after_the_tag_on_its_line_starts_the_example() {
        let text = "/**\n * @example <caption>Adding.</caption> add(1, 2);\n * add(3, 4);\n */\n";

        assert_eq!(
            lines(text, &only(text)),
            [
                (" * @example <caption>Adding.</caption> ", "add(1, 2);"),
                (" * ", "add(3, 4);")
            ]
        );
    }

    #[test]
    fn the_next_block_tag_ends_the_example_and_blank_lines_around_it_are_not_its_own() {
        let text = "/**\n * @example\n *\n * f();\n *\n * @returns nothing\n */\n";

        assert_eq!(lines(text, &only(text)), [(" * ", "f();")]);
    }

    #[test]
    fn an_empty_example_is_no_block() {
        let text = "/**\n * @example\n *\n * @returns nothing\n */\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn a_tag_that_only_starts_with_example_opens_no_section() {
        let text = "/**\n * @examples\n * f();\n */\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn a_gutterless_example_keeps_its_codes_indentation() {
        let text = "/**\n  @example\n  if (a) {\n    b();\n  }\n*/\n";

        assert_eq!(
            lines(text, &only(text)),
            [("  ", "if (a) {"), ("  ", "  b();"), ("  ", "}")]
        );
    }

    #[test]
    fn a_close_on_the_last_example_line_ends_the_example_before_it() {
        let text = "/**\n * @example\n * f(); */\nfunction f() {}\n";

        assert_eq!(lines(text, &only(text)), [(" * ", "f(); ")]);
    }

    #[test]
    fn an_example_replacement_keeps_the_gutter_and_the_next_tag() {
        let text = "/**\n * @example\n * old\n * @returns x\n */\n";
        let block = only(text);

        let (range, replacement) =
            replacement_for(&block, text, "a\n\nb").expect("the block has a body to replace");

        assert_eq!(replacement, " * a\n *\n * b");
        assert_eq!(text.get(range), Some(" * old"));
    }

    #[test]
    fn a_fenced_block_and_an_unfenced_example_come_in_file_order() {
        let text = "/**\n * @example\n * first();\n * @see\n * ```ts\n * second();\n * ```\n */\n";

        let found: Vec<_> = blocks_of(text)
            .iter()
            .map(|block| lines(text, block))
            .collect();

        assert_eq!(found, [[(" * ", "first();")], [(" * ", "second();")]]);
    }

    #[test]
    fn plain_block_and_line_comments_are_not_containers() {
        let text =
            "/* ```ts\n x;\n ``` */\n// ```ts\n// x;\n// ```\n/*** ```ts\n x;\n ``` */\n/**/\n";

        assert_eq!(blocks_of(text), []);
    }

    #[test]
    fn a_gutterless_comment_shares_one_prefix_so_indented_code_keeps_its_indent() {
        let text = "/**\n  Makes.\n\n  ```ts\n  const a = {\n    b: {\n      c: 1,\n    },\n  };\n  ```\n*/\n";

        assert_eq!(
            lines(text, &only(text)),
            [
                ("  ", "const a = {"),
                ("  ", "  b: {"),
                ("  ", "    c: 1,"),
                ("  ", "  },"),
                ("  ", "};"),
            ]
        );
    }

    #[test]
    fn a_gutterless_replacement_keeps_the_comments_and_the_codes_indentation() {
        let text = "  /**\n    ```ts\n    old\n    ```\n  */\n";
        let block = only(text);

        let (range, replacement) = replacement_for(&block, text, "a = {\n  b: 1,\n};")
            .expect("the block has a body to replace");

        assert_eq!(replacement, "    a = {\n      b: 1,\n    };");
        assert_eq!(text.get(range), Some("    old"));
    }

    #[test]
    fn gutter_and_gutterless_lines_each_lose_their_own_prefix() {
        let text =
            "/**\n * Doc.\n *\n   ```ts\n   if (a) {\n * b();\n     c();\n   }\n   ```\n */\n";

        assert_eq!(
            lines(text, &only(text)),
            [
                ("   ", "if (a) {"),
                (" * ", "b();"),
                ("   ", "  c();"),
                ("   ", "}"),
            ]
        );
    }

    #[test]
    fn mixed_tabs_and_spaces_share_only_their_matching_leading_bytes() {
        let text = "/**\n\t```ts\n\tx = {\n\t\ty,\n\t    z,\n\t};\n\t```\n*/\n";

        assert_eq!(
            lines(text, &only(text)),
            [
                ("\t", "x = {"),
                ("\t", "\ty,"),
                ("\t", "    z,"),
                ("\t", "};")
            ]
        );
    }

    #[test]
    fn line_one_after_the_opener_has_no_prefix() {
        let text = "/** ```ts\n * f();\n * ```\n */\n";

        let block = only(text);

        assert_eq!(block.fence.line, 1, "the fence opens on line 1");
        assert_eq!(lines(text, &block), [(" * ", "f();")]);
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
