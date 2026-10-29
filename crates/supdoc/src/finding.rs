//! Findings: one problem each, linter style, with where it is, how bad it is, what to do next, and
//! an exact edit when there's exactly one correct fix (04, 07).

use std::ops::Range;

use crate::link::SourcePath;

/// One problem, linter style. Produced by the scanner and checks, rendered by output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable code from the owning module: `read/missing`, `link/unknown-namespace`,
    /// `scan/case-mismatch`. Never reworded.
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
    pub fix: Option<Fix>,
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
            fix: Some(fix),
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

impl Location {
    /// Locates `range` in `text`, the contents of `file`, computing its line and column.
    ///
    /// `None` if `range` is reversed, runs past the end of `text`, or doesn't fall on character
    /// boundaries.
    #[must_use]
    pub fn in_text(file: SourcePath, text: &str, range: Range<usize>) -> Option<Self> {
        // Rejects a reversed range, one past the end, or one splitting a character.
        let _covered: &str = text.get(range.clone())?;
        let before = text.get(..range.start)?;
        let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
        let line = u32::try_from(before.matches('\n').count() + 1).ok()?;
        let column = u32::try_from(before.get(line_start..)?.chars().count() + 1).ok()?;
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
}

/// The whole lines a finding's range covers, captured when the finding is created (07 G11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// 1-based line number of the first line of `text`.
    pub first_line: u32,
    /// The whole lines the finding's range covers, nothing more, without the final newline.
    pub text: String,
    /// The highlighted range, relative to `text`.
    pub highlight: Range<usize>,
}

impl Snippet {
    /// Captures the lines of `text` that `range` covers.
    ///
    /// `None` if `range` is reversed, runs past the end of `text`, or doesn't fall on character
    /// boundaries.
    #[must_use]
    pub fn capture(text: &str, range: Range<usize>) -> Option<Self> {
        // Rejects a reversed range, one past the end, or one splitting a character.
        let _covered: &str = text.get(range.clone())?;
        let before = text.get(..range.start)?;
        let start = before.rfind('\n').map_or(0, |newline| newline + 1);
        let after = text.get(range.end..)?;
        let end = after
            .find('\n')
            .map_or(text.len(), |newline| range.end + newline);
        Some(Self {
            first_line: u32::try_from(before.matches('\n').count() + 1).ok()?,
            text: text.get(start..end)?.to_owned(),
            highlight: (range.start - start)..(range.end - start),
        })
    }
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
        };
        let snippet =
            Snippet::capture(GUIDE, range_of("add")).expect("the range should be capturable");

        let finding = Finding::new("resolve/not-found", Severity::Error, "no symbol `sub`")
            .with_location(location.clone())
            .with_help("did you mean `minus`?")
            .with_fix(fix.clone())
            .with_snippet(snippet.clone());

        assert_eq!(
            finding,
            Finding {
                code: "resolve/not-found",
                severity: Severity::Error,
                location: Some(location),
                message: "no symbol `sub`".to_owned(),
                help: Some("did you mean `minus`?".to_owned()),
                fix: Some(fix),
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
