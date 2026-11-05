//! Go doc comments: the comments directly above a `package` clause or a top-level declaration.
//!
//! A doc comment is a run of `//` lines, or one `/* */` comment, that starts its line and ends on
//! the line just above the clause or declaration, as `go/doc` reads it. A `//` line loses its
//! indentation, `//` and one space after it; a tab after `//` stays in the text. A `/* */`
//! comment's text runs from after `/*` to before `*/`, with no prefix on its later lines. A
//! directive line (`//go:generate`) is not doc text: the comment's Markdown ends before it.
//!
//! Only fenced blocks are blocks. Go's own code blocks are indented, and go.dev renders them as
//! preformatted text, not Go, so they are text, never blocks.

use std::collections::HashSet;
use std::ops::Range;

use crate::finding::Location;
use crate::read::cartridge::{MarkdownRegion, SourceFile};
use crate::read::error::ReadError;

use super::super::block::CodeBlock;
use super::{Line, Run, blocks_of_runs};

/// The top-level nodes a doc comment documents.
const DOCUMENTED: [&str; 6] = [
    "package_clause",
    "function_declaration",
    "method_declaration",
    "type_declaration",
    "const_declaration",
    "var_declaration",
];

/// Every code block in the doc comments of `file`, a Go file, in file order.
///
/// # Errors
///
/// The [`ReadError`] of the first doc comment whose Markdown, or its NUL-replaced re-parse, can't
/// be parsed: the file's blocks are then unknown, not none.
pub(in super::super) fn blocks(
    file: &SourceFile,
    locate: impl Fn(Range<usize>) -> Option<Location>,
) -> Result<Vec<CodeBlock>, ReadError> {
    let text = file.text();
    let rows = documented_rows(file);
    let regions = file.language().markdown_regions(file)?;
    let runs = regions
        .iter()
        .filter_map(|region| doc_run(text, region, &rows))
        .collect();
    blocks_of_runs(file, runs, locate)
}

/// The rows on which a [`DOCUMENTED`] node of `file` starts.
fn documented_rows(file: &SourceFile) -> HashSet<usize> {
    let root = file.tree().root_node();
    let mut cursor = root.walk();
    root.children(&mut cursor)
        .filter(|node| DOCUMENTED.contains(&node.kind()))
        .map(|node| node.start_position().row)
        .collect()
}

/// `region`, a run of `//` comments or one other comment, as a doc comment's [`Run`] and Markdown
/// text. `None` when it isn't a doc comment: it doesn't start its line, or the line after it
/// doesn't start one of `rows`.
fn doc_run(text: &str, region: &MarkdownRegion, rows: &HashSet<usize>) -> Option<(Run, String)> {
    let (first, last) = region.ranges().first().zip(region.ranges().last())?;
    // A line comment's range starts after its `//`; any other comment's range is the whole node.
    let line = text.get(..first.start_byte)?.ends_with("//");
    let start = if line {
        first.start_byte - 2
    } else {
        first.start_byte
    };
    let ends_on = if line {
        last.start_point.row
    } else {
        last.end_point.row
    };
    let begins = line_start(text, start);
    let starts_line = text
        .get(begins..start)
        .is_some_and(|indent| indent.trim_start_matches([' ', '\t']).is_empty());
    if !starts_line || !rows.contains(&(ends_on + 1)) {
        return None;
    }
    if line {
        Some(line_run(text, region))
    } else {
        block_run(text, first.start_byte..first.end_byte)
    }
}

/// The run of `//` lines `region` as a [`Run`] and its Markdown text, which ends before the first
/// directive line.
fn line_run(text: &str, region: &MarkdownRegion) -> (Run, String) {
    let mut lines = Vec::new();
    let mut markdown = String::new();
    for range in region.ranges() {
        let body = text
            .get(range.start_byte..range.end_byte)
            .unwrap_or_default();
        if is_directive(body) {
            break;
        }
        let space = usize::from(body.starts_with(' '));
        lines.push(Line {
            synthetic: markdown.len(),
            file: range.start_byte + space,
            begins: line_start(text, range.start_byte),
        });
        markdown.push_str(body.get(space..).unwrap_or_default());
    }
    (Run { lines }, markdown)
}

/// The `/* */` comment `comment` of `text` as a [`Run`] and its Markdown text: its text between
/// `/*` and `*/`, each line with no prefix but line 1's `/*`. `None` for any other comment.
fn block_run(text: &str, comment: Range<usize>) -> Option<(Run, String)> {
    let source = text.get(comment.clone())?;
    let inner = source.strip_prefix("/*")?;
    let inner = inner.strip_suffix("*/").unwrap_or(inner);
    let start = comment.start + 2;
    let mut lines = Vec::new();
    let mut at = start;
    for (number, line) in inner.split('\n').enumerate() {
        lines.push(Line {
            synthetic: at - start,
            file: at,
            begins: if number == 0 {
                line_start(text, comment.start)
            } else {
                at
            },
        });
        at += line.len() + 1;
    }
    Some((Run { lines }, inner.to_owned()))
}

/// Where the line holding `offset` starts in `text`.
fn line_start(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .and_then(|head| head.rfind('\n'))
        .map_or(0, |newline| newline + 1)
}

/// Whether `body`, a `//` line after its `//`, is a directive rather than doc text, as `go/ast`
/// decides it: `line `, `extern ` or `export `, or lowercase letters and digits, a colon and one
/// more of them (`go:generate`, `nolint:all`).
fn is_directive(body: &str) -> bool {
    if ["line ", "extern ", "export "]
        .into_iter()
        .any(|prefix| body.starts_with(prefix))
    {
        return true;
    }
    let is_word = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let Some(colon) = body.find(':').filter(|&colon| colon > 0) else {
        return false;
    };
    let bytes = body.as_bytes();
    bytes
        .get(..colon)
        .is_some_and(|word| word.iter().all(is_word))
        && bytes.get(colon + 1).is_some_and(is_word)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::cartridges::go::cartridge;
    use crate::code_blocks::block::{blocks_in, replacement_for};
    use crate::path::AbsolutePath;

    /// The blocks of `text` parsed as `/repo/p.go`.
    fn blocks_of(text: &str) -> Vec<CodeBlock> {
        let path = AbsolutePath::new(Path::new("/repo/p.go"))
            .expect("the fixture path should be absolute");
        let go = cartridge().expect("the built-in Go queries should compile");
        let file =
            SourceFile::parse(path, text.to_owned(), go).expect("tree-sitter should return a tree");
        blocks_in(&file, |range| {
            let source = "p.go".parse().expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        })
    }

    /// The one block of `text`.
    fn only(text: &str) -> CodeBlock {
        let mut blocks = blocks_of(text);
        assert_eq!(blocks.len(), 1, "blocks: {blocks:?}");
        blocks.remove(0)
    }

    /// The text of `range` in `text`.
    fn slice<'text>(text: &'text str, range: &Range<usize>) -> &'text str {
        text.get(range.clone())
            .expect("a block range should lie inside the text")
    }

    /// Each body line of `block`: its prefix and content in `text`.
    fn lines<'text>(text: &'text str, block: &CodeBlock) -> Vec<(&'text str, &'text str)> {
        block
            .body
            .iter()
            .map(|line| (slice(text, &line.prefix), slice(text, &line.content)))
            .collect()
    }

    const FUNC: &str = "package p\n\n// Add adds.\n//\n// ```go\n// x := 1\n//\n// y := 2\n// ```\nfunc Add() {}\n";

    #[test]
    fn a_fenced_block_above_a_func_is_found_on_its_file_lines() {
        let block = only(FUNC);

        assert_eq!(block.fence.line, 5, "the fence opens on line 5");
        assert_eq!(
            slice(FUNC, &block.fence.range),
            "```go\n// x := 1\n//\n// y := 2\n// ```"
        );
        let info = block.info.as_ref().expect("the fence has an info string");
        assert_eq!(info.text, "go");
        assert_eq!(
            lines(FUNC, &block),
            [("// ", "x := 1"), ("//", ""), ("// ", "y := 2")]
        );
    }

    #[test]
    fn a_replacement_keeps_the_marker_and_trims_it_on_a_blank_line() {
        let block = only(FUNC);

        let (range, replacement) =
            replacement_for(&block, FUNC, "a\n\nb").expect("the block has a body to replace");

        assert_eq!(slice(FUNC, &range), "// x := 1\n//\n// y := 2");
        assert_eq!(replacement, "// a\n//\n// b");
    }

    #[test]
    fn an_empty_blocks_replacement_takes_the_closing_fences_marker() {
        let text = "// ```go\n// ```\nfunc F() {}\n";
        let block = only(text);

        let (range, replacement) =
            replacement_for(&block, text, "x").expect("an empty closed block can be filled");

        assert_eq!(range, 9..9, "the body goes after the opening fence's line");
        assert_eq!(replacement, "// x\n");
    }

    #[test]
    fn each_documented_declaration_and_the_package_clause_hold_blocks() {
        let fence = "// ```go\n// x\n// ```\n";
        let declarations = [
            "package p\n",
            "func F() {}\n",
            "func (T) M() {}\n",
            "type T int\n",
            "const C = 1\n",
            "var V = 1\n",
            "var (\n\tA = 1\n)\n",
        ];

        for declaration in declarations {
            let text = if declaration.starts_with("package") {
                format!("{fence}{declaration}")
            } else {
                format!("package p\n\n{fence}{declaration}")
            };
            assert_eq!(blocks_of(&text).len(), 1, "{text}");
        }
    }

    #[test]
    fn an_indented_go_code_block_is_not_a_block() {
        let text = "package p\n\n// Add adds.\n//\n//\tx := 1\n//\tAdd()\nfunc Add() {}\n";

        assert_eq!(blocks_of(text), Vec::new());
    }

    #[test]
    fn a_comment_not_directly_above_a_declaration_is_not_a_doc_comment() {
        let cases = [
            "package p\n\n// ```go\n// x\n// ```\n\nfunc F() {}\n",
            "package p\n\nfunc F() {\n\t// ```go\n\t// x\n\t// ```\n\tF()\n}\n",
            "package p\n\n// ```go\n// x\n// ```\n",
            "package p\n\nvar a = 1 /* ```go\nx\n``` */\nfunc F() {}\n",
            "package p\n\nimport (\n\t// ```go\n\t// x\n\t// ```\n\t\"fmt\"\n)\n",
        ];

        for text in cases {
            assert_eq!(blocks_of(text), Vec::new(), "{text}");
        }
    }

    #[test]
    fn a_block_comment_doc_holds_a_block_with_no_line_prefix() {
        let text = "/*\nPackage p adds.\n\n```go\nx := 1\n```\n*/\npackage p\n";

        let block = only(text);

        assert_eq!(block.fence.line, 4, "the fence opens on line 4");
        assert_eq!(lines(text, &block), [("", "x := 1")]);
    }

    #[test]
    fn a_directive_ends_the_doc_text() {
        let text = "package p\n\n// ```go\n// x\n// ```\n//go:generate stringer\n// ```go\n// y\n// ```\nfunc F() {}\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [("// ", "x")]);
    }

    #[test]
    fn a_fence_open_at_a_directive_ends_before_it() {
        let text = "package p\n\n// ```go\n// x\n//go:generate stringer\nfunc F() {}\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [("// ", "x")]);
    }

    #[test]
    fn a_tab_after_the_marker_stays_in_the_text() {
        let text = "// ```go\n//\tx := 1\n// ```\nfunc F() {}\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [("//", "\tx := 1")]);
    }

    #[test]
    fn an_untagged_fence_is_text() {
        let block = only("// ```\n// x\n// ```\nfunc F() {}\n");

        assert_eq!(block.info, None);
        assert_eq!(block.default_language(), None);
    }

    #[test]
    fn a_crlf_doc_comment_maps_its_lines() {
        let text = "// ```go\r\n// x\r\n// ```\r\nfunc F() {}\r\n";

        let block = only(text);

        assert_eq!(lines(text, &block), [("// ", "x\r")]);
    }

    #[test]
    fn directives_are_read_as_go_ast_reads_them() {
        let directives = [
            "go:generate x",
            "go:build linux",
            "nolint:all",
            "line a.go:1",
            "export F",
            "extern f",
        ];
        let text = [
            " go:generate x",
            "Go:generate",
            "go:",
            ":x",
            "go: x",
            "see: x",
        ];

        for body in directives {
            assert!(is_directive(body), "`{body}` is a directive");
        }
        for body in text {
            assert!(!is_directive(body), "`{body}` is doc text");
        }
    }
}
