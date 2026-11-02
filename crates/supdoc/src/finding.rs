//! Findings: one problem each, linter style, with where it is, how bad it is, what to do next, and
//! an exact edit when there's exactly one correct fix (04, 07).

use std::ops::Range;

use crate::link::SourcePath;

/// The code of a finding about a lockfile entry whose link is gone.
pub const OBSOLETE_CODE: &str = "link-obsolete";

/// One problem, linter style. Produced by the scanner and checks, rendered by output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable code from the owning module: `read-missing`, `link-unknown-namespace`,
    /// `scan-case-mismatch`. Never reworded.
    pub code: &'static str,
    /// How bad it is.
    pub severity: Severity,
    /// Where it is, when it is about one place in a source file.
    pub location: Option<Location>,
    /// What happened, in one line.
    pub message: String,
    /// What to do next.
    pub help: Option<String>,
    /// An exact edit, present only when there's exactly one correct fix. The fixer never guesses.
    pub fix: Option<Box<Fix>>,
    /// The source lines this finding covers, captured when it is created so rendering never reads
    /// files (07 G11).
    pub snippet: Option<Snippet>,
}

impl Finding {
    /// A finding with only its code, severity and message. Add the rest with the `with_*` methods.
    #[must_use]
    pub fn new(code: &'static str, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code,
            severity,
            location: None,
            message: message.into(),
            help: None,
            fix: None,
            snippet: None,
        }
    }

    /// Sets where the finding is.
    #[must_use]
    pub fn with_location(self, location: Location) -> Self {
        Self {
            location: Some(location),
            ..self
        }
    }

    /// Sets what to do next.
    #[must_use]
    pub fn with_help(self, help: impl Into<String>) -> Self {
        Self {
            help: Some(help.into()),
            ..self
        }
    }

    /// Sets the one correct edit.
    #[must_use]
    pub fn with_fix(self, fix: Fix) -> Self {
        Self {
            fix: Some(Box::new(fix)),
            ..self
        }
    }

    /// Sets the captured source excerpt.
    #[must_use]
    pub fn with_snippet(self, snippet: Snippet) -> Self {
        Self {
            snippet: Some(snippet),
            ..self
        }
    }
}

/// How bad a finding is (07 G5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Fails `check`.
    Error,
    /// Reported; fails `check` only under `--deny-warnings`.
    Warning,
    /// Information only, such as a moved link carrying its history (05 C6).
    Note,
}

/// One place in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The source file.
    pub file: SourcePath,
    /// Byte range, for fixes.
    pub range: Range<usize>,
    /// 1-based line of `range.start`, for humans.
    pub line: u32,
    /// 1-based column of `range.start`, counted in characters.
    pub column: u32,
}

/// The byte offset where each line of a text starts, computed once so that many lookups cost one
/// scan of the text instead of one scan each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Start offsets of every line, ascending; the first is always 0.
    starts: Vec<usize>,
}

impl LineIndex {
    /// Indexes the lines of `text`.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let newlines = text.match_indices('\n').map(|(at, _)| at + 1);
        Self {
            starts: std::iter::once(0).chain(newlines).collect(),
        }
    }

    /// The 1-based line containing byte `offset`.
    #[must_use]
    pub fn line_of(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset)
    }

    /// The byte offset where the line containing `offset` starts.
    fn line_start(&self, offset: usize) -> usize {
        self.starts
            .get(self.line_of(offset) - 1)
            .copied()
            .unwrap_or(0)
    }
}

/// A text with its [`LineIndex`], so every location and snippet taken from it costs a lookup
/// instead of a scan of the whole text.
#[derive(Debug, Clone)]
pub struct IndexedText<'text> {
    /// The text the index was built from.
    text: &'text str,
    /// Where each line of `text` starts.
    index: LineIndex,
}

impl<'text> IndexedText<'text> {
    /// Indexes `text` once.
    #[must_use]
    pub fn new(text: &'text str) -> Self {
        Self {
            text,
            index: LineIndex::new(text),
        }
    }

    /// The 1-based line and character column of byte `offset`.
    ///
    /// `None` if `offset` is past the end of the text or not on a character boundary.
    #[must_use]
    pub fn position(&self, offset: usize) -> Option<(u32, u32)> {
        let line_start = self.index.line_start(offset);
        let column = self.text.get(line_start..offset)?.chars().count() + 1;
        let line = self.index.line_of(offset);
        Some((u32::try_from(line).ok()?, u32::try_from(column).ok()?))
    }

    /// Locates `range` in the text, as [`Location::in_text`] does.
    #[must_use]
    pub fn locate(&self, file: SourcePath, range: Range<usize>) -> Option<Location> {
        Location::in_indexed(file, self.text, &self.index, range)
    }

    /// Captures the lines `range` covers, as [`Snippet::capture`] does.
    #[must_use]
    pub fn snippet(&self, range: Range<usize>) -> Option<Snippet> {
        Snippet::capture_indexed(self.text, &self.index, range)
    }
}

impl Location {
    /// Locates `range` in `text`, the contents of `file`, computing its line and column.
    ///
    /// `None` if `range` is reversed, runs past the end of `text`, or doesn't fall on character
    /// boundaries.
    #[must_use]
    pub fn in_text(file: SourcePath, text: &str, range: Range<usize>) -> Option<Self> {
        Self::in_indexed(file, text, &LineIndex::new(text), range)
    }

    /// Like [`in_text`](Self::in_text), for many ranges of one `text` whose `index` was built once.
    #[must_use]
    pub fn in_indexed(
        file: SourcePath,
        text: &str,
        index: &LineIndex,
        range: Range<usize>,
    ) -> Option<Self> {
        // Rejects a reversed range, one past the end, or one splitting a character.
        let _covered: &str = text.get(range.clone())?;
        let line_start = index.line_start(range.start);
        let line = u32::try_from(index.line_of(range.start)).ok()?;
        let column = u32::try_from(text.get(line_start..range.start)?.chars().count() + 1).ok()?;
        Some(Self {
            file,
            range,
            line,
            column,
        })
    }
}

/// Replace `range` in `file` with `replacement`. Applied by `supdoc fix --yes` (05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// The source file to edit.
    pub file: SourcePath,
    /// The bytes to replace.
    pub range: Range<usize>,
    /// The new text.
    pub replacement: String,
    /// The bytes `range` held in the scanned text. `supdoc fix` applies the edit only if the file
    /// still holds them. Never serialized: this type has no serde derive.
    pub scanned: Box<str>,
}

/// A bounded window of the lines a finding's range covers, captured when the finding is created (07 G11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// 1-based line number of the first line of `text`.
    pub first_line: u32,
    /// The covered lines, cut to a window around the range, without the final newline.
    pub text: String,
    /// The highlighted range, relative to `text`.
    pub highlight: Range<usize>,
}

/// Bytes of the covered lines kept on each side of the highlighted range.
const SNIPPET_CONTEXT: usize = 200;

/// Most bytes of the highlighted range a snippet keeps; a longer range is cut short.
const SNIPPET_MAX_HIGHLIGHT: usize = 1024;

impl Snippet {
    /// Captures the lines of `text` that `range` covers, cut to a window of [`SNIPPET_CONTEXT`]
    /// bytes around a range of at most [`SNIPPET_MAX_HIGHLIGHT`] bytes.
    ///
    /// `None` if `range` is reversed, runs past the end of `text`, or doesn't fall on character
    /// boundaries.
    #[must_use]
    pub fn capture(text: &str, range: Range<usize>) -> Option<Self> {
        Self::capture_indexed(text, &LineIndex::new(text), range)
    }

    /// Like [`capture`](Self::capture), for many ranges of one `text` whose `index` was built once.
    #[must_use]
    pub fn capture_indexed(text: &str, index: &LineIndex, range: Range<usize>) -> Option<Self> {
        // Rejects a reversed range, one past the end, or one splitting a character.
        let _covered: &str = text.get(range.clone())?;
        let line_start = index.line_start(range.start);
        let after = text.get(range.end..)?;
        let line_end = after
            .find('\n')
            .map_or(text.len(), |newline| range.end + newline);
        let highlight_end = floor_boundary(
            text,
            range
                .end
                .min(range.start.saturating_add(SNIPPET_MAX_HIGHLIGHT)),
        )
        .max(range.start);
        let start = ceil_boundary(
            text,
            line_start.max(range.start.saturating_sub(SNIPPET_CONTEXT)),
        );
        let end = floor_boundary(
            text,
            line_end.min(highlight_end.saturating_add(SNIPPET_CONTEXT)),
        );
        Some(Self {
            first_line: u32::try_from(index.line_of(range.start)).ok()?,
            text: text.get(start..end)?.to_owned(),
            highlight: (range.start - start)..(highlight_end - start),
        })
    }
}

/// The largest char boundary of `text` at or before `offset`.
fn floor_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The smallest char boundary of `text` at or after `offset`.
fn ceil_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(raw: &str) -> SourcePath {
        raw.parse()
            .expect("the fixture source path should be valid")
    }

    const GUIDE: &str = "# Guide\n\nSee [add](../src/lib.rs#add) and\n[sub](x.rs#sub) too.\n";

    fn range_of(needle: &str) -> Range<usize> {
        let start = GUIDE
            .find(needle)
            .expect("the needle should be in the fixture");
        start..start + needle.len()
    }

    /// Asserts `range` of `text` captures a window of at most `bound` bytes holding the
    /// highlight (cut to `highlight_len`), starting on line `first_line`.
    fn assert_window(text: &str, range: Range<usize>, highlight_len: usize, first_line: u32) {
        let bound = 2 * SNIPPET_CONTEXT + SNIPPET_MAX_HIGHLIGHT + 8;
        let index = LineIndex::new(text);
        let snippet = Snippet::capture_indexed(text, &index, range.clone())
            .expect("the range should be capturable");

        assert!(
            snippet.text.len() <= bound,
            "snippet is {} bytes",
            snippet.text.len()
        );
        assert_eq!(snippet.first_line, first_line);
        assert_eq!(
            snippet.text.get(snippet.highlight.clone()).map(str::len),
            Some(highlight_len),
            "the highlight should be on character boundaries"
        );
        assert_eq!(
            snippet.text.get(snippet.highlight.clone()),
            text.get(range.start..range.start + highlight_len)
        );
    }

    #[test]
    fn snippet_of_a_long_line_is_a_window() {
        let anchor = "[a](b.rs#c)";
        // Three-byte characters: the context distance (200) and the highlight cap (1024) are not
        // multiples of 3, so every window edge lands inside a character and must be moved to a
        // boundary. An edge left where it fell makes the capture fail.
        let filler = "€".repeat(500_000);
        let text = format!("{filler}{anchor}{filler}\nnext");
        assert_window(
            &text,
            filler.len()..filler.len() + anchor.len(),
            anchor.len(),
            1,
        );

        // A highlight over the cap is cut short, on a boundary.
        let wide = "€".repeat(5_000);
        let text = format!("{filler}{wide}{filler}");
        assert_window(
            &text,
            filler.len()..filler.len() + wide.len(),
            SNIPPET_MAX_HIGHLIGHT - SNIPPET_MAX_HIGHLIGHT % 3,
            1,
        );

        // A link within the context of each edge of its line keeps the whole edge.
        let near = format!("{}{anchor}{}\n", "a".repeat(100), "b".repeat(100));
        let text = format!("first\n{near}last");
        let start = "first\n".len() + 100;
        let index = LineIndex::new(&text);
        let snippet = Snippet::capture_indexed(&text, &index, start..start + anchor.len())
            .expect("the range should be capturable");
        assert_eq!(snippet.text, near.trim_end());
        assert_eq!(snippet.first_line, 2);

        // Many findings on one huge line each copy only a window.
        for offset in (0..1000).map(|i| i * 999) {
            assert_window(&filler, offset..offset + 3, 3, 1);
        }
    }

    #[test]
    fn location_counts_lines_and_character_columns_from_one() {
        let text = "é = 1\nlet [a](b.rs#c)";
        let start = text.find('[').expect("the fixture should contain a link");
        let location = Location::in_text(source("src/x.rs"), text, start..text.len())
            .expect("a range inside the text should be locatable");

        assert_eq!((location.line, location.column), (2, 5));
        assert_eq!(location.range, start..text.len());
    }

    #[test]
    fn a_range_far_into_a_many_line_text_is_located_by_the_line_index() {
        let text = "ab\n".repeat(5000) + "é [a](b.rs#c)\nlast";
        let start = text.find('[').expect("the fixture should contain a link");
        let range = start..start + 9;
        let index = LineIndex::new(&text);
        let location = Location::in_indexed(source("a.md"), &text, &index, range.clone())
            .expect("a range inside the text should be locatable");
        let snippet =
            Snippet::capture_indexed(&text, &index, range).expect("the range should be capturable");

        assert_eq!((location.line, location.column), (5001, 3));
        assert_eq!(
            (snippet.first_line, snippet.text.as_str()),
            (5001, "é [a](b.rs#c)")
        );
    }

    #[test]
    fn location_on_the_first_line_starts_at_column_one() {
        let location = Location::in_text(source("a.md"), "[x](y.rs)", 0..9)
            .expect("a range inside the text should be locatable");

        assert_eq!((location.line, location.column), (1, 1));
    }

    #[test]
    fn location_and_snippet_reject_ranges_outside_the_text_or_inside_a_character() {
        assert_eq!(Location::in_text(source("a.md"), "abc", 2..9), None);
        assert_eq!(Location::in_text(source("a.md"), "é", 1..2), None);
        assert_eq!(Snippet::capture("abc", 2..9), None);
        assert_eq!(Snippet::capture("é", 1..2), None);
    }

    #[test]
    fn snippet_captures_only_the_lines_the_range_covers() {
        let range = range_of("[add](../src/lib.rs#add)");
        let snippet = Snippet::capture(GUIDE, range).expect("the range should be capturable");

        assert_eq!(snippet.first_line, 3);
        assert_eq!(snippet.text, "See [add](../src/lib.rs#add) and");
        assert_eq!(
            snippet.text.get(snippet.highlight.clone()),
            Some("[add](../src/lib.rs#add)")
        );
    }

    #[test]
    fn snippet_spanning_lines_keeps_each_whole_line() {
        let start = GUIDE.find("and").expect("the fixture should contain `and`");
        let end = GUIDE
            .find(" too")
            .expect("the fixture should contain ` too`");
        let snippet = Snippet::capture(GUIDE, start..end).expect("the range should be capturable");

        assert_eq!(snippet.first_line, 3);
        assert_eq!(
            snippet.text,
            "See [add](../src/lib.rs#add) and\n[sub](x.rs#sub) too."
        );
        assert_eq!(
            snippet.text.get(snippet.highlight.clone()),
            Some("and\n[sub](x.rs#sub)")
        );
    }

    #[test]
    fn finding_builder_sets_each_part() {
        let location = Location::in_text(source("docs/guide.md"), GUIDE, range_of("add"))
            .expect("the range should be locatable");
        let fix = Fix {
            file: source("docs/guide.md"),
            range: range_of("sub"),
            replacement: "minus".to_owned(),
            scanned: "sub".into(),
        };
        let snippet =
            Snippet::capture(GUIDE, range_of("add")).expect("the range should be capturable");

        let finding = Finding::new("resolve-not-found", Severity::Error, "no symbol `sub`")
            .with_location(location.clone())
            .with_help("did you mean `minus`?")
            .with_fix(fix.clone())
            .with_snippet(snippet.clone());

        assert_eq!(
            finding,
            Finding {
                code: "resolve-not-found",
                severity: Severity::Error,
                location: Some(location),
                message: "no symbol `sub`".to_owned(),
                help: Some("did you mean `minus`?".to_owned()),
                fix: Some(Box::new(fix)),
                snippet: Some(snippet),
            },
        );
    }

    #[test]
    fn severity_serializes_lowercase() {
        let json = serde_json::to_string(&[Severity::Error, Severity::Warning, Severity::Note])
            .expect("severities should serialize");

        assert_eq!(json, r#"["error","warning","note"]"#);
    }
}
