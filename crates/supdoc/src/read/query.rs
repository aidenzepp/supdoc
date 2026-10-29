//! [`QueryCartridge`]: a language that is pure data, a grammar plus its `tags.scm` and
//! `injections.scm` (03 R3, The query contract).
//!
//! Both files use tree-sitter's own capture names, so they stay ordinary tree-sitter queries:
//!
//! | Capture | Meaning |
//! |---|---|
//! | `@definition.<kind>` | An addressable symbol. Its node's range is what gets hashed |
//! | `@name` | Its name, one path segment |
//! | `@injection.content` + `injection.language` | With `markdown_inline`: a region where links are read |
//! | `@supdoc.scope` | A container that adds segments to definitions inside it but isn't addressable (a Rust `impl`) |
//! | `@supdoc.scope.name` | That segment. Inside a definition pattern, a parent segment containment can't express (a Go receiver) |
//! | `@supdoc.scope.qualifier` | A second segment after the name (the trait in `impl Trait for Type`) |
//! | `@supdoc.scope.arguments`, `@supdoc.scope.qualifier.arguments` | Type arguments of the name or qualifier, for the full-type alias (03 R5) |
//!
//! See [`tag_symbols`] for how a path is built from them.
//!
//! A run of adjacent line comments of the same kind is one link region, in every language: see
//! [`injected_markdown`].

use tree_sitter::{Point, QueryCursor, Range, StreamingIterator};

use crate::read::cartridge::{
    Cartridge, CompiledQueries, LanguageId, MarkdownRegion, SourceFile, SymbolTable,
};
use crate::read::symbols::tag_symbols;

/// The `injection.language` that marks a region where links are read (03 R9).
pub const MARKDOWN_INLINE: &str = "markdown_inline";

/// Markers that open a line comment, longest first so `///` isn't read as `//`.
///
/// Each marker is its own comment kind: a `///` doc comment beside a `//` comment, or a `#!`
/// shebang above a `#` comment, are separate regions.
const LINE_COMMENT_MARKERS: [&str; 5] = ["///", "//!", "//", "#!", "#"];

/// A cartridge that is only data: its grammar and two compiled query files.
#[derive(Debug)]
pub struct QueryCartridge {
    /// Stable id.
    id: LanguageId,
    /// Lowercase extensions without the dot.
    extensions: &'static [&'static str],
    /// The grammar.
    grammar: tree_sitter::Language,
    /// `tags.scm` and `injections.scm`, compiled against `grammar`.
    queries: CompiledQueries,
}

impl QueryCartridge {
    /// Compiles `tags` and `injections` against `grammar`.
    ///
    /// Built-in cartridges are built once, on first use. A test compiles every one of them, so a
    /// failure here is a supdoc bug or a grammar upgrade that renamed node kinds.
    ///
    /// # Errors
    ///
    /// [`QueryFileError`] naming the query file that doesn't compile and tree-sitter's reason.
    pub fn new(
        id: LanguageId,
        extensions: &'static [&'static str],
        grammar: tree_sitter::Language,
        tags: &str,
        injections: &str,
    ) -> Result<Self, QueryFileError> {
        let compile = |file: &'static str, source: &str| {
            tree_sitter::Query::new(&grammar, source).map_err(|error| QueryFileError {
                language: id,
                file,
                error,
            })
        };
        let queries = CompiledQueries::new(
            compile("tags.scm", tags)?,
            compile("injections.scm", injections)?,
        );
        Ok(Self {
            id,
            extensions,
            grammar,
            queries,
        })
    }
}

impl Cartridge for QueryCartridge {
    fn id(&self) -> LanguageId {
        self.id
    }

    fn extensions(&self) -> &'static [&'static str] {
        self.extensions
    }

    fn grammar(&self) -> &tree_sitter::Language {
        &self.grammar
    }

    fn queries(&self) -> &CompiledQueries {
        &self.queries
    }

    fn symbols(&self, file: &SourceFile) -> SymbolTable {
        tag_symbols(file)
    }

    fn markdown_regions(&self, file: &SourceFile) -> Vec<MarkdownRegion> {
        injected_markdown(file)
    }
}

/// The regions `file`'s `injections.scm` marks as `markdown_inline`, in file order (03 R9, R14).
///
/// The language comes from a `#set! injection.language` on the pattern or from an
/// `@injection.language` capture, tree-sitter's two ways of naming it.
///
/// A run of adjacent line comments of the same kind (`///`, `//`, `#`) is one region made of the
/// comment bodies, so a link may wrap across lines and a reference definition applies to the
/// whole run. Block comments and docstrings are one region each. See [`join_line_comments`].
#[must_use]
pub fn injected_markdown(file: &SourceFile) -> Vec<MarkdownRegion> {
    let query = file.language().queries().injections();
    let content = query.capture_index_for_name("injection.content");
    let language = query.capture_index_for_name("injection.language");
    let text = file.text();

    let mut regions = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, file.tree().root_node(), text.as_bytes());
    while let Some(found) = matches.next() {
        let set = query
            .property_settings(found.pattern_index)
            .iter()
            .any(|property| {
                &*property.key == "injection.language"
                    && property.value.as_deref() == Some(MARKDOWN_INLINE)
            });
        let captured = found.captures().iter().any(|capture| {
            Some(capture.index) == language
                && text.get(capture.node.byte_range()) == Some(MARKDOWN_INLINE)
        });
        if !set && !captured {
            continue;
        }
        regions.extend(
            found
                .captures()
                .iter()
                .filter(|capture| Some(capture.index) == content)
                .map(|capture| capture.node.range()),
        );
    }
    regions.sort_by_key(|range| (range.start_byte, range.end_byte));
    regions.dedup();
    join_line_comments(regions, text)
}

/// `ranges`, in file order, as regions: each run of adjacent line comments of the same kind
/// becomes one region of their bodies, and every other range is a region of its own.
///
/// A line comment is a range on one line that starts with one of [`LINE_COMMENT_MARKERS`]. Two
/// line comments are adjacent when the second starts on the next row with only whitespace between
/// them, so a blank line or code splits a run.
fn join_line_comments(ranges: Vec<Range>, text: &str) -> Vec<MarkdownRegion> {
    let mut regions = Vec::new();
    let mut run = Vec::new();
    // The run's last comment and its marker.
    let mut last: Option<(&str, Range)> = None;
    for range in ranges {
        let marker = line_comment_marker(range, text);
        let joins = matches!(
            (marker, last),
            (Some(marker), Some((last_marker, last_range)))
                if marker == last_marker && adjacent(last_range, range, text)
        );
        if !joins {
            regions.extend(MarkdownRegion::new(std::mem::take(&mut run)));
        }
        if let Some(marker) = marker {
            run.push(line_comment_body(range, marker, text));
            last = Some((marker, range));
        } else {
            regions.push(MarkdownRegion::from(range));
            last = None;
        }
    }
    regions.extend(MarkdownRegion::new(run));
    regions
}

/// The marker `range` opens with, if it is a line comment: one line of text, ignoring a trailing
/// line ending, that starts with one of [`LINE_COMMENT_MARKERS`].
fn line_comment_marker(range: Range, text: &str) -> Option<&'static str> {
    let comment = text.get(range.start_byte..range.end_byte)?;
    if comment.trim_end_matches(['\r', '\n']).contains('\n') {
        return None;
    }
    LINE_COMMENT_MARKERS
        .into_iter()
        .find(|marker| comment.starts_with(marker))
}

/// Whether line comment `next` starts on the row after `last` with only whitespace between them.
fn adjacent(last: Range, next: Range, text: &str) -> bool {
    next.start_point.row == last.start_point.row + 1
        && text
            .get(last.end_byte..next.start_byte)
            .is_some_and(|gap| gap.chars().all(char::is_whitespace))
}

/// The body of line comment `range`: after `marker`, through the end of its line.
///
/// The line ending is part of the body, so the joined bodies of a run read as lines of Markdown
/// and words on either side of a line break stay apart.
fn line_comment_body(range: Range, marker: &str, text: &str) -> Range {
    let start_byte = range.start_byte + marker.len();
    let start_point = Point {
        row: range.start_point.row,
        column: range.start_point.column + marker.len(),
    };
    let ends_line = text
        .get(range.start_byte..range.end_byte)
        .is_some_and(|comment| comment.ends_with('\n'));
    let line_ending = text
        .get(range.end_byte..)
        .and_then(|after| {
            ["\r\n", "\n"]
                .into_iter()
                .find(|eol| after.starts_with(eol))
        })
        .filter(|_| !ends_line);
    let (end_byte, end_point) = match line_ending {
        Some(eol) => (
            range.end_byte + eol.len(),
            Point {
                row: range.end_point.row + 1,
                column: 0,
            },
        ),
        None => (range.end_byte, range.end_point),
    };
    Range {
        start_byte,
        end_byte,
        start_point,
        end_point,
    }
}

/// A built-in query file doesn't compile against its grammar.
#[derive(Debug, thiserror::Error)]
#[error("the built-in {file} for {language} does not compile (supdoc bug): {error}")]
pub struct QueryFileError {
    /// The cartridge.
    pub language: LanguageId,
    /// `tags.scm` or `injections.scm`.
    pub file: &'static str,
    /// tree-sitter's reason, with row and column.
    pub error: tree_sitter::QueryError,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::path::AbsolutePath;

    fn rust_file(text: &str) -> SourceFile {
        let rust =
            crate::cartridges::rust::cartridge().expect("the built-in Rust queries should compile");
        let path = AbsolutePath::new(Path::new("/repo/src/lib.rs"))
            .expect("the fixture path should be absolute");
        SourceFile::parse(path, text.to_owned(), rust).expect("tree-sitter should return a tree")
    }

    /// Each region's text, comment markers left out, in file order.
    fn region_texts(file: &SourceFile) -> Vec<String> {
        file.language()
            .markdown_regions(file)
            .iter()
            .map(|region| {
                region
                    .text_within(0..file.text().len(), file.text())
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn rust_comments_are_markdown_regions_in_file_order() {
        let file =
            rust_file("/// See [add](../docs/a.md).\nfn add() {\n    // inline\n}\n/* block */\n");

        let texts = region_texts(&file);

        assert_eq!(texts.len(), 3, "one region per comment, got {texts:?}");
        assert!(
            texts
                .first()
                .is_some_and(|text| text.contains("[add](../docs/a.md)")),
            "the doc comment comes first: {texts:?}",
        );
        assert!(
            texts.get(1).is_some_and(|text| text.contains("inline")),
            "{texts:?}"
        );
        assert_eq!(texts.get(2).map(String::as_str), Some("/* block */"));
    }

    #[test]
    fn adjacent_line_comments_of_one_kind_are_one_region_of_their_bodies() {
        let file = rust_file("/// One\n///two\n    /// three\nfn f() {}\n");

        assert_eq!(region_texts(&file), [" One\ntwo\n three\n"]);
    }

    #[test]
    fn blank_line_code_or_another_kind_splits_a_comment_run() {
        let file = rust_file("/// a\n\n/// b\nfn f() {}\n/// c\n// d\n//! e\n/* f */\n// g\n");

        assert_eq!(
            region_texts(&file),
            [" a\n", " b\n", " c\n", " d\n", " e\n", "/* f */", " g\n"],
        );
    }

    #[test]
    fn joined_region_ranges_carry_the_file_offsets() {
        let text = "fn f() {}\n  // one\n  // two";
        let file = rust_file(text);

        let regions = file.language().markdown_regions(&file);
        let region = regions.first().expect("the two comments are one region");
        let ranges: Vec<_> = region
            .ranges()
            .iter()
            .map(|range| (range.start_byte..range.end_byte, range.start_point))
            .collect();

        assert_eq!(
            ranges,
            [
                (14..19, Point { row: 1, column: 4 }),
                (23..27, Point { row: 2, column: 4 }),
            ],
        );
        assert_eq!(text.get(14..19), Some(" one\n"));
        assert_eq!(text.get(23..27), Some(" two"));
    }

    #[test]
    fn string_literal_is_not_a_markdown_region() {
        let file = rust_file("fn f() { let s = \"[a](b.rs#c)\"; }\n");

        assert_eq!(region_texts(&file), Vec::<&str>::new());
    }

    #[test]
    fn query_error_names_the_file_and_language() {
        let error = QueryCartridge::new(
            LanguageId::new("rust"),
            &["rs"],
            tree_sitter_rust::LANGUAGE.into(),
            "(no_such_node) @definition.function",
            "",
        )
        .expect_err("a query naming an unknown node kind should not compile");

        assert_eq!(error.file, "tags.scm");
        assert!(
            error
                .to_string()
                .starts_with("the built-in tags.scm for rust does not compile (supdoc bug): "),
            "got {error}",
        );
    }
}
