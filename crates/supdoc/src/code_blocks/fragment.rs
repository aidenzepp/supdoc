//! The fragment kinds a block can declare with `fragment(<kind>)`, and how a tool says it checks one.
//!
//! A frame ([`Support::Framed`]) is one fixed pair of lines per tool and kind. Any name a frame
//! introduces that the block doesn't (a function, a type, a package) is spelled so no author
//! would write it, such as `__supdoc_fragment`, so a tool's message about the author's code can't
//! be mistaken for one about the frame. Each unit that adds a frame pair gives it a real-tool e2e
//! test asserting that no finding holds any frame line or frame-only name.

use super::tools::ToolCartridge;

/// The grammar position a fragment block holds, as `fragment(<kind>)` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The body of a function: statements, and items where the language allows them there.
    Statements,
    /// One expression.
    Expression,
    /// The entries of a map or object, without its braces.
    Entries,
    /// The children of a markup element, without the element.
    Children,
    /// The members of a type, without the type.
    Members,
    /// A file's top-level declarations, without the header the language requires.
    Declarations,
}

impl Kind {
    /// Every kind, in the order help lists them.
    pub(crate) const ALL: [Self; 6] = [
        Self::Statements,
        Self::Expression,
        Self::Entries,
        Self::Children,
        Self::Members,
        Self::Declarations,
    ];

    /// The kind's name, as written inside `fragment(...)`.
    #[must_use]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Statements => "statements",
            Self::Expression => "expression",
            Self::Entries => "entries",
            Self::Children => "children",
            Self::Members => "members",
            Self::Declarations => "declarations",
        }
    }

    /// The kind with its indefinite article: "a statements", "an expression".
    #[must_use]
    pub(crate) fn with_article(self) -> String {
        let name = self.name();
        let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
            "an"
        } else {
            "a"
        };
        format!("{article} {name}")
    }

    /// The kind written as `name`, `None` for any other text.
    #[must_use]
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The six kind names as a help lists them: `` `statements`, `expression`, ... ``.
    #[must_use]
    pub(crate) fn listed() -> String {
        Self::ALL
            .map(|kind| format!("`{}`", kind.name()))
            .join(", ")
    }
}

/// How a tool checks a fragment of one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Support {
    /// The tool parses the kind as written. `relax` names the rules that fire only because the
    /// enclosing construct is missing, such as ruff's `F706` (`return` outside a function) for
    /// `statements`; the tool leaves them off for a block of this kind.
    Native {
        /// The rules left off for this kind.
        relax: &'static [&'static str],
    },
    /// The tool reads the kind only inside a fixed frame: `before` and `after` are the lines it
    /// sees around the block, never shown, written or chosen from the block's content.
    /// `out_before` and `out_after` are the frame lines a formatter is expected to print back.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the first frame pair (P5-FR10 to FR15) constructs it"
        )
    )]
    Framed {
        /// The lines the tool sees before the block.
        before: &'static str,
        /// The lines the tool sees after the block.
        after: &'static str,
        /// The lines a formatter prints for `before`.
        out_before: &'static str,
        /// The lines a formatter prints for `after`.
        out_after: &'static str,
    },
}

impl Support {
    /// Native support with nothing relaxed.
    pub(crate) const NATIVE: Self = Self::Native { relax: &[] };

    /// The rules left off for a block of this kind.
    #[must_use]
    pub(crate) const fn relax(self) -> &'static [&'static str] {
        match self {
            Self::Native { relax } => relax,
            Self::Framed { .. } => &[],
        }
    }

    /// The frame the tool puts around a block of `kind`: `None` when it reads the kind as
    /// written.
    #[must_use]
    pub(crate) const fn frame(self, kind: Kind) -> Option<Frame> {
        match self {
            Self::Native { .. } => None,
            Self::Framed {
                before,
                after,
                out_before,
                out_after,
            } => Some(Frame {
                kind,
                before,
                after,
                out_before,
                out_after,
            }),
        }
    }
}

/// What a tool's report shows in place of a line that refers to a line of the frame.
pub(crate) const FRAME_LINE: &str = "<the frame supdoc adds>";

/// The fixed lines a tool sees around a block that declares a kind it reads only inside a frame
/// (see [`Support::Framed`]), and the lines a formatter is expected to print back. The linter
/// sees the block as written; the formatter sees each of its lines indented by one unit, so the
/// lines it copies verbatim come back exact once that unit is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The kind the block declares.
    pub(crate) kind: Kind,
    /// The lines before the block. Empty for none.
    pub(crate) before: &'static str,
    /// The lines after the block. Empty for none.
    pub(crate) after: &'static str,
    /// The lines a formatter's output must begin with.
    pub(crate) out_before: &'static str,
    /// The lines a formatter's output must end with.
    pub(crate) out_after: &'static str,
}

impl Frame {
    /// The frame `tool` puts around a block of `kind`. `None` for a block checked as a whole
    /// file, and for a kind the tool reads as written or doesn't list.
    #[must_use]
    pub(crate) fn of(tool: &ToolCartridge, kind: Option<Kind>) -> Option<Self> {
        let kind = kind?;
        let (_, support) = tool.fragments.iter().find(|(listed, _)| *listed == kind)?;
        support.frame(kind)
    }

    /// The text a linter checks for a block whose `input` is its body as written: `before`, the
    /// body and `after`, each starting on its own line.
    #[must_use]
    pub(crate) fn wrap(self, input: &str) -> String {
        self.wrap_indented(input, "")
    }

    /// The text a formatter reads for a block whose `input` is its body as written: as
    /// [`Self::wrap`], with each non-empty line of the body led by `indent`.
    #[must_use]
    pub(crate) fn wrap_indented(self, input: &str, indent: &str) -> String {
        let mut text = String::with_capacity(
            self.before.len() + input.len() + self.after.len() + 2 + indent.len() * 8,
        );
        let mut push = |part: &str| {
            text.push_str(part);
            if !part.ends_with('\n') {
                text.push('\n');
            }
        };
        if !self.before.is_empty() {
            push(self.before);
        }
        for line in input.split_inclusive('\n') {
            if line == "\n" {
                push(line);
            } else {
                push(&format!("{indent}{line}"));
            }
        }
        if !self.after.is_empty() {
            push(self.after);
        }
        text
    }

    /// How many lines `before` takes.
    #[must_use]
    pub(crate) fn lead(self) -> usize {
        self.before.lines().count()
    }

    /// The block's lines in `output`, a formatter's stdout for [`Self::wrap_indented`] with
    /// `indent`, each ended by LF: the lines between `out_before` and `out_after`, each non-empty
    /// one with its leading `indent` removed. `None` when `output` doesn't begin with
    /// `out_before` and end with `out_after`, or a non-empty line between them doesn't start with
    /// `indent`.
    #[must_use]
    pub(crate) fn inner(self, output: &str, indent: &str) -> Option<String> {
        let lines: Vec<&str> = output.lines().collect();
        let head: Vec<&str> = self.out_before.lines().collect();
        let tail: Vec<&str> = self.out_after.lines().collect();
        let body = lines
            .strip_prefix(head.as_slice())?
            .strip_suffix(tail.as_slice())?;
        let mut inner = String::with_capacity(output.len());
        for line in body {
            if !line.is_empty() {
                inner.push_str(line.strip_prefix(indent)?);
            }
            inner.push('\n');
        }
        Some(inner)
    }

    /// Whether `output`, a formatter's stdout, is the frame alone: its lines with nothing
    /// between them, or `out_before` and `out_after` joined on one line, as rustfmt prints
    /// `fn main() {}` and biome `({});` for an empty body.
    #[must_use]
    pub(crate) fn alone(self, output: &str) -> bool {
        let lines: Vec<&str> = output.lines().collect();
        let joined = format!("{}{}", self.out_before, self.out_after);
        self.inner(output, "").is_some_and(|inner| inner.is_empty())
            || (!joined.contains('\n') && lines == [joined.as_str()])
    }

    /// `detail`, a tool's report on the checked text of a block of `lines` lines whose each
    /// non-empty line the tool read led by `indent`, in the block's own coordinates and with no
    /// line of the frame shown.
    ///
    /// A report line refers to the checked text's line N by a source gutter (`N |`, biome's
    /// `N │`, either after a `>` marker) or, failing that, by an `N:C` location. One that refers
    /// to a frame line is replaced by [`FRAME_LINE`]. One that refers to a block line is
    /// renumbered to the block's line, its location's column moved back past `indent`, and its
    /// quoted source and the marker lines under it shifted left by `indent`'s width. A line that
    /// refers to no line is replaced when, trimmed, it is one of the frame's lines.
    #[must_use]
    pub(crate) fn hidden(self, detail: &str, lines: usize, indent: &str) -> String {
        let lead = self.lead();
        let block_line = |line: usize| {
            line.checked_sub(lead)
                .filter(|line| (1..=lines).contains(line))
        };
        let mut under_block = false;
        detail
            .lines()
            .map(|line| {
                if let Some(gutter) = Gutter::of(line) {
                    let renumbered =
                        block_line(gutter.number).map(|number| gutter.at(number, indent));
                    under_block = renumbered.is_some();
                    return renumbered.unwrap_or_else(|| FRAME_LINE.to_owned());
                }
                if let Some(code) = marker_code(line) {
                    return if under_block {
                        format!(
                            "{}{}",
                            line.get(..line.len() - code.len()).unwrap_or_default(),
                            unindent(code, indent)
                        )
                    } else {
                        line.to_owned()
                    };
                }
                if let Some(at) = Location::of(line) {
                    return block_line(at.line).map_or_else(
                        || FRAME_LINE.to_owned(),
                        |number| at.moved(line, number, indent),
                    );
                }
                if self.holds(line) {
                    FRAME_LINE.to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether `line`, trimmed, is one of the frame's lines.
    fn holds(self, line: &str) -> bool {
        let line = line.trim();
        !line.is_empty()
            && self
                .before
                .lines()
                .chain(self.after.lines())
                .any(|frame| frame.trim() == line)
    }
}

/// The columns a report shows `indent` taking: a tab as four, as rustc's source listings do.
fn width(indent: &str) -> usize {
    indent.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// `code`, a source line as a report quotes it, without the leading `indent` the tool read: the
/// unit itself, or as many spaces as the report shows it taking. `code` as it is when it starts
/// with neither.
fn unindent<'code>(code: &'code str, indent: &str) -> &'code str {
    if indent.is_empty() {
        return code;
    }
    code.strip_prefix(indent)
        .or_else(|| code.strip_prefix(&" ".repeat(width(indent))))
        .unwrap_or(code)
}

/// What follows the gutter of `line` when it is an unnumbered gutter line, such as the marker
/// line rustc prints under a quoted source line (`  |     ^`): the text after the bar and one
/// space. `None` for any other line.
fn marker_code(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix(['|', '\u{2502}'])?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// A report line that quotes the checked text's line `number` behind a source gutter.
struct Gutter<'line> {
    /// What leads the number: indentation and a `>` marker.
    lead: &'line str,
    /// How many columns the number takes, padding included.
    columns: usize,
    /// The line number.
    number: usize,
    /// From the end of the number to the end of the bar and its one space.
    bar: &'line str,
    /// The quoted source.
    code: &'line str,
}

impl<'line> Gutter<'line> {
    /// `line` as a gutter line: `None` when it doesn't start, after spaces and an optional `>`,
    /// with a number followed by `|` or `│`.
    fn of(line: &'line str) -> Option<Self> {
        let start = line.len() - line.trim_start().len();
        let after_marker = line.get(start..)?;
        let marked = after_marker
            .strip_prefix('>')
            .map_or(start, |rest| line.len() - rest.trim_start().len());
        let from = line.get(marked..)?;
        let digits = from
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(from.len());
        let number = from.get(..digits)?.parse().ok()?;
        let after = from.get(digits..)?;
        let spaces = after.len() - after.trim_start().len();
        let bar_char = after
            .trim_start()
            .chars()
            .next()
            .filter(|c| matches!(c, '|' | '\u{2502}'))?;
        let mut bar_end = digits + spaces + bar_char.len_utf8();
        if from.get(bar_end..)?.starts_with(' ') {
            bar_end += 1;
        }
        Some(Self {
            lead: line.get(..marked)?,
            columns: digits,
            number,
            bar: from.get(digits..bar_end)?,
            code: from.get(bar_end..)?,
        })
    }

    /// The line quoting the block's line `number` instead, its source without `indent`.
    fn at(&self, number: usize, indent: &str) -> String {
        format!(
            "{}{number:>columns$}{}{}",
            self.lead,
            self.bar,
            unindent(self.code, indent),
            columns = self.columns
        )
    }
}

/// The first `N:C` location in a report line.
struct Location {
    /// Where `N:C` lies in the line.
    span: std::ops::Range<usize>,
    /// N, 1-based.
    line: usize,
    /// C, 1-based.
    column: usize,
}

impl Location {
    /// The first `N:C` pair in `text`, `None` when it has none.
    fn of(text: &str) -> Option<Self> {
        let mut from = 0;
        while let Some(found) = text.get(from..)?.find(|c: char| c.is_ascii_digit()) {
            let start = from + found;
            let rest = text.get(start..)?;
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            let after = rest.get(digits..)?;
            if let Some(column) = after.strip_prefix(':') {
                let width = column
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(column.len());
                if width > 0 {
                    return Some(Self {
                        span: start..start + digits + 1 + width,
                        line: rest.get(..digits)?.parse().ok()?,
                        column: column.get(..width)?.parse().ok()?,
                    });
                }
            }
            from = start + digits;
        }
        None
    }

    /// `text`, holding this location, with it naming line `number` and its column moved back
    /// past `indent`, never before column 1.
    fn moved(&self, text: &str, number: usize, indent: &str) -> String {
        let column = self.column.saturating_sub(indent.chars().count()).max(1);
        format!(
            "{}{number}:{column}{}",
            text.get(..self.span.start).unwrap_or_default(),
            text.get(self.span.end..).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_takes_the_article_its_spelling_needs() {
        assert_eq!(Kind::Expression.with_article(), "an expression");
        assert_eq!(Kind::Statements.with_article(), "a statements");
        assert_eq!(Kind::Entries.with_article(), "an entries");
        assert_eq!(Kind::Children.with_article(), "a children");
    }

    #[test]
    fn only_a_framed_kind_has_frame_lines_and_none_relaxed() {
        let framed = Support::Framed {
            before: "({",
            after: "})",
            out_before: "({",
            out_after: "});",
        };

        let frame = framed
            .frame(Kind::Entries)
            .expect("a framed kind has a frame");
        assert_eq!((frame.before, frame.after), ("({", "})"));
        assert_eq!(framed.relax(), &[] as &[&str]);
        assert_eq!(Support::NATIVE.frame(Kind::Entries), None);
    }

    /// rustfmt's statements frame, as fragments-design §2.6 lists it.
    const STATEMENTS: Frame = Frame {
        kind: Kind::Statements,
        before: "fn main() {",
        after: "}",
        out_before: "fn main() {",
        out_after: "}",
    };

    #[test]
    fn the_formatter_reads_each_non_empty_line_indented_and_the_linter_reads_it_as_written() {
        let body = "if x {\n    y();\n}\n\nz();\n";

        assert_eq!(
            STATEMENTS.wrap_indented(body, "\t"),
            "fn main() {\n\tif x {\n\t    y();\n\t}\n\n\tz();\n}\n"
        );
        assert_eq!(STATEMENTS.wrap(body), format!("fn main() {{\n{body}}}\n"));
    }

    #[test]
    fn a_report_line_naming_a_frame_line_is_hidden_by_its_number_and_the_blocks_are_kept() {
        let rustc = "error: mismatched closing delimiter: `}`\n --> <stdin>:4:1\n  |\n\
                     1 | fn main() {\n  |           - opened here\n3 |     }\n  |     ^ here\n4 | }\n  | ^ closes";
        let biome = "file.ts:2:6 parse\n  > 1 \u{2502} fn main() {\n    2 \u{2502}     if x {\n    \
                     4 \u{2502} }";

        assert_eq!(
            STATEMENTS.hidden(rustc, 2, "    "),
            "error: mismatched closing delimiter: `}`\n<the frame supdoc adds>\n  |\n\
             <the frame supdoc adds>\n  |           - opened here\n2 | }\n  | ^ here\n\
             <the frame supdoc adds>\n  | ^ closes"
        );
        assert_eq!(
            STATEMENTS.hidden(biome, 2, "    "),
            "file.ts:1:2 parse\n<the frame supdoc adds>\n    1 \u{2502} if x {\n\
             <the frame supdoc adds>"
        );
    }

    #[test]
    fn a_report_line_with_no_number_is_hidden_when_it_is_a_frame_line() {
        assert_eq!(
            STATEMENTS.hidden("panic\n  fn main() {\nlet x = 1;\n}\nend", 1, ""),
            "panic\n<the frame supdoc adds>\nlet x = 1;\n<the frame supdoc adds>\nend"
        );
    }

    #[test]
    fn a_renumbered_gutter_keeps_its_width_and_a_tab_unit_comes_off_as_rustc_shows_it() {
        let two_digit = Frame {
            before: "a\nb\nc\nd\ne\nf\ng\nh\ni\nj",
            ..STATEMENTS
        };

        assert_eq!(
            two_digit.hidden("11 |     x\n   |     ^", 3, "    "),
            " 1 | x\n   | ^"
        );
        assert_eq!(
            STATEMENTS.hidden(" --> <stdin>:2:11\n2 |     let x = 1", 1, "\t"),
            " --> <stdin>:1:10\n1 | let x = 1"
        );
    }
}
