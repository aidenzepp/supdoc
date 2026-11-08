//! Comparing an excerpt with the source it was copied from.
//!
//! A `from(...)` block holds exactly its parts' text: each part loses the indentation its first
//! line has in the source, so a method reads from column 0, and the parts join in written order
//! with one blank line between them. No tool runs on an excerpt.

use crate::finding::{Finding, Severity};
use crate::hash::LineEndings;

use super::block::CodeBlock;
use super::format::{self, FixDoc, Guarded};
use super::source::SourceText;

/// The code for a `from(...)` block that differs from its source.
pub(crate) const STALE: &str = "code-block-stale";

/// The text a `from(...)` block with `parts`, in written order, must hold. Each part loses its
/// first line's indentation from every line and its final line endings, and the parts join with
/// one blank line between them. CRLF reads as LF when `line_endings` is `normalize`.
#[must_use]
pub(crate) fn expected(parts: &[SourceText], line_endings: LineEndings) -> String {
    parts
        .iter()
        .map(|part| {
            let text = dedent(&part.text, &part.indent);
            let text = match line_endings {
                LineEndings::Normalize => text.replace("\r\n", "\n"),
                LineEndings::Exact => text,
            };
            text.trim_end_matches('\n').to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `text` with `indent` removed from the start of every line after the first, or as much of it
/// as the line starts with. The first line starts at the symbol, so it has none.
fn dedent(text: &str, indent: &str) -> String {
    text.split('\n')
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                return line;
            }
            let shared = line
                .bytes()
                .zip(indent.bytes())
                .take_while(|(have, want)| have == want)
                .count();
            line.get(shared..).unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `block`'s body as written in `text`, without container prefixes, its lines joined by LF and
/// with no final line ending. A line's CR before LF is dropped when `line_endings` is
/// `normalize`. `None` when a line's range isn't inside `text`.
pub(crate) fn body(block: &CodeBlock, text: &str, line_endings: LineEndings) -> Option<String> {
    let lines = block
        .body
        .iter()
        .map(|line| {
            let content = text.get(line.content.clone())?;
            Some(match line_endings {
                LineEndings::Normalize => content.strip_suffix('\r').unwrap_or(content),
                LineEndings::Exact => content,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(lines.join("\n"))
}

/// The `code-block-stale` error at `block` when its body in `doc`, the doc it was found in, isn't
/// byte for byte `expected`, with nothing after it. Its fix replaces exactly the body, keeping
/// container prefixes, when [`format::safe_fix`] allows it; when it refuses, the error keeps no
/// fix and its `code-block-fix-unsafe` warning follows. Empty when the body is `expected`, or its
/// ranges don't lie inside `doc`.
#[must_use]
pub(crate) fn stale(
    block: &CodeBlock,
    doc: &FixDoc,
    expected: &str,
    line_endings: LineEndings,
) -> Vec<Finding> {
    if body(block, doc.text(), line_endings).is_none_or(|body| body == expected) {
        return Vec::new();
    }
    let error = Finding::new(
        STALE,
        Severity::Error,
        "the `from(...)` block differs from its source",
    )
    .with_location(block.fence.clone());
    match format::safe_fix(block, doc, expected, "the `from(...)` source") {
        Guarded::Fix(fix) => {
            let help = format!(
                "run `supdoc fix {} --yes` to copy the source into the block",
                fix.file.as_str()
            );
            vec![error.with_help(help).with_fix(fix)]
        }
        Guarded::Unplaced => Vec::new(),
        Guarded::Refused(unsafe_fix) => vec![error, unsafe_fix],
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::cartridges::markdown::cartridge::cartridge;
    use crate::code_blocks::block::blocks_in;
    use crate::code_blocks::info::SourceKind;
    use crate::code_blocks::source::{Unresolved, read};
    use crate::config::Config;
    use crate::finding::Location;
    use crate::limits::Limits;
    use crate::path::AbsolutePath;
    use crate::read::cartridge::{Languages, SourceFile};

    /// The source the parts come from: `add` at column 0 and `Limits.resolve` inside an `impl`.
    const LIB: &str = "pub fn add() {}\n\npub struct Limits;\n\nimpl Limits {\n    pub fn resolve() {\n        let _ = 1;\n    }\n}\n";

    /// The method `Limits.resolve`, read from column 0.
    const RESOLVE: &str = "pub fn resolve() {\n    let _ = 1;\n}";

    /// Reads each of `values` as a `from(...)` part from a project holding [`LIB`], for a block in
    /// `docs/guide.md`.
    fn parts(values: &[&str]) -> Vec<SourceText> {
        let root = TempDir::new().expect("a temp root should be creatable");
        fs::create_dir_all(root.path().join("src")).expect("src should be creatable");
        fs::write(root.path().join("src/lib.rs"), LIB).expect("the lib should be writable");
        let config =
            Config::load(&AbsolutePath::new(root.path()).expect("a temp path is absolute"))
                .expect("an empty project's config should load");
        let languages = Languages::builtin().expect("the built-in cartridges should load");
        values
            .iter()
            .map(|value| {
                read(&config, languages, "docs/guide.md", SourceKind::From, value).unwrap_or_else(
                    |unresolved| match unresolved {
                        Unresolved::Missing(missing) => {
                            panic!("{value} should resolve: {missing:?}")
                        }
                        Unresolved::CartridgeLoad(error) => panic!("a cartridge failed: {error}"),
                    },
                )
            })
            .collect()
    }

    /// The one code block in the Markdown `text`, as `docs/guide.md`.
    fn block_of(text: &str) -> CodeBlock {
        let path = crate::path::fixture("/repo/docs/guide.md");
        let file = SourceFile::parse(path, text.to_owned(), cartridge())
            .expect("tree-sitter should return a tree");
        let mut blocks = blocks_in(&file, |range| {
            let source = "docs/guide.md"
                .parse()
                .expect("the fixture source should be valid");
            Location::in_text(source, text, range)
        });
        assert_eq!(blocks.len(), 1, "expected one block in {text:?}");
        blocks.remove(0)
    }

    /// The one finding [`stale`] gives `block` in `text`, if any.
    fn stale_of(
        block: &CodeBlock,
        text: &str,
        expected: &str,
        line_endings: LineEndings,
    ) -> Option<Finding> {
        let doc = FixDoc::new(Arc::from(text), Limits::PARSE_TIMEOUT);
        let mut findings = stale(block, &doc, expected, line_endings);
        assert!(
            findings.len() <= 1,
            "expected one finding at most: {findings:?}"
        );
        findings.pop()
    }

    /// `text` with `finding`'s fix applied.
    fn fixed(text: &str, finding: &Finding) -> String {
        let fix = finding
            .fix
            .as_ref()
            .expect("a stale finding should carry a fix");
        let mut out = text.to_owned();
        out.replace_range(fix.range.clone(), &fix.replacement);
        out
    }

    #[test]
    fn a_method_inside_an_impl_reads_from_column_zero() {
        let expected = expected(
            &parts(&["/src/lib.rs#Limits.resolve"]),
            LineEndings::Normalize,
        );

        assert_eq!(expected, RESOLVE);
    }

    #[test]
    fn two_parts_join_with_one_blank_line() {
        let expected = expected(
            &parts(&["/src/lib.rs#add", "/src/lib.rs#Limits.resolve"]),
            LineEndings::Normalize,
        );

        assert_eq!(expected, format!("pub fn add() {{}}\n\n{RESOLVE}"));
    }

    #[test]
    fn parts_join_in_written_order() {
        let expected = expected(
            &parts(&["/src/lib.rs#Limits.resolve", "/src/lib.rs#add"]),
            LineEndings::Normalize,
        );

        assert_eq!(expected, format!("{RESOLVE}\n\npub fn add() {{}}"));
    }

    #[test]
    fn an_exact_copy_is_not_stale_and_crlf_reads_as_lf_when_normalized() {
        let text = "# G\n\n```rust from(/src/lib.rs#add)\r\npub fn add() {}\r\n```\r\n";
        let block = block_of(text);

        let normalized = stale_of(&block, text, "pub fn add() {}", LineEndings::Normalize);
        let exact = stale_of(&block, text, "pub fn add() {}", LineEndings::Exact);

        assert!(normalized.is_none(), "{normalized:?}");
        assert!(exact.is_some(), "a CR is a difference under `exact`");
    }

    #[test]
    fn a_fix_replaces_only_the_body() {
        let text =
            "# G\n\n```rust from(/src/lib.rs#add)\nfn old() {}\nfn older() {}\n```\n\nAfter.\n";
        let block = block_of(text);

        let finding = stale_of(&block, text, RESOLVE, LineEndings::Normalize)
            .expect("a differing body should be stale");

        assert_eq!(finding.code, STALE);
        assert_eq!(
            fixed(text, &finding),
            format!("# G\n\n```rust from(/src/lib.rs#add)\n{RESOLVE}\n```\n\nAfter.\n")
        );
    }

    #[test]
    fn a_fix_keeps_a_list_item_prefix_and_leaves_blank_lines_bare() {
        let text = "- Add:\n\n  ```rust from(/src/lib.rs#add)\n  fn old() {}\n  ```\n";
        let block = block_of(text);

        let finding = stale_of(
            &block,
            text,
            "fn a() {}\n\nfn b() {}",
            LineEndings::Normalize,
        )
        .expect("a differing body should be stale");

        assert_eq!(
            fixed(text, &finding),
            "- Add:\n\n  ```rust from(/src/lib.rs#add)\n  fn a() {}\n\n  fn b() {}\n  ```\n"
        );
    }

    #[test]
    fn a_fix_keeps_a_quote_prefix() {
        let text = "> ```rust from(/src/lib.rs#add)\n> fn old() {}\n> ```\n";
        let block = block_of(text);

        let finding = stale_of(&block, text, "fn a() {}", LineEndings::Normalize)
            .expect("a differing body should be stale");

        assert_eq!(
            fixed(text, &finding),
            "> ```rust from(/src/lib.rs#add)\n> fn a() {}\n> ```\n"
        );
    }

    #[test]
    fn a_fix_fills_an_empty_body() {
        let text = "```rust from(/src/lib.rs#add)\n```\n";
        let block = block_of(text);

        let finding = stale_of(&block, text, "pub fn add() {}", LineEndings::Normalize)
            .expect("an empty body should be stale");

        assert_eq!(
            fixed(text, &finding),
            "```rust from(/src/lib.rs#add)\npub fn add() {}\n```\n"
        );
    }

    #[test]
    fn a_source_with_a_line_that_would_close_the_fence_gets_no_fix_and_a_warning() {
        let text = "- Add:\n\n  ```rust from(/src/lib.rs#add)\n  fn old() {}\n  ```\n";
        let block = block_of(text);

        for expected in ["let s = r\"\n```\n\";", "let s = \"\0\n\t```\";"] {
            let doc = FixDoc::new(Arc::from(text), Limits::PARSE_TIMEOUT);
            let findings = stale(&block, &doc, expected, LineEndings::Normalize);

            let codes: Vec<&str> = findings.iter().map(|finding| finding.code).collect();
            assert_eq!(codes, [STALE, "code-block-fix-unsafe"], "{expected:?}");
            assert!(
                findings.iter().all(|finding| finding.fix.is_none()),
                "{expected:?} should get no fix"
            );
        }
    }
}
