//! The fragment kinds a block can declare with `fragment(<kind>)`, and how a tool says it checks one.
//!
//! A frame ([`Support::Framed`]) is one fixed pair of lines per tool and kind. Any name a frame
//! introduces that the block doesn't (a function, a type, a package) is spelled so no author
//! would write it, such as `__supdoc_fragment`, so a tool's message about the author's code can't
//! be mistaken for one about the frame. Each unit that adds a frame pair gives it a real-tool e2e
//! test asserting that no finding holds any frame line or frame-only name.

use super::format::first_line;
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

    /// `detail`, a tool's report on the checked text of a block of `lines` lines, with each line
    /// that refers to a line of the frame replaced by [`FRAME_LINE`]. A report line refers to the
    /// checked text's line N by a source gutter (`N |`, biome's `N │`, either after a `>`
    /// marker) or, failing that, by a `N:C` location. The block's own lines, whatever their text,
    /// are never replaced.
    #[must_use]
    pub(crate) fn hidden(self, detail: &str, lines: usize) -> String {
        let lead = self.lead();
        let on_frame = |line: usize| line != 0 && (line <= lead || line > lead + lines);
        detail
            .lines()
            .map(|line| {
                if refers(line).is_some_and(on_frame) {
                    FRAME_LINE
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The 1-based line of the checked text that `line`, one line of a tool's report, refers to:
/// the number in its source gutter, else its first `N:C` location. `None` when it names none.
fn refers(line: &str) -> Option<usize> {
    let shown = line.trim_start();
    let shown = shown.strip_prefix('>').map_or(shown, str::trim_start);
    let digits = shown
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(shown.len());
    let gutter = shown
        .get(digits..)
        .map(str::trim_start)
        .is_some_and(|rest| rest.starts_with(['|', '\u{2502}']));
    if digits > 0 && gutter {
        return shown.get(..digits)?.parse().ok();
    }
    first_line(line).map(|line| line + 1)
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
                     1 | fn main() {\n  | - opened here\n3 | }\n4 | }\n  | ^ closes";
        let biome = "file.ts:1:2 parse\n  > 1 \u{2502} fn main() {\n    2 \u{2502} if x {\n    \
                     4 \u{2502} }";

        assert_eq!(
            STATEMENTS.hidden(rustc, 2),
            "error: mismatched closing delimiter: `}`\n<the frame supdoc adds>\n  |\n\
             <the frame supdoc adds>\n  | - opened here\n3 | }\n<the frame supdoc adds>\n  | ^ closes"
        );
        assert_eq!(
            STATEMENTS.hidden(biome, 2),
            "<the frame supdoc adds>\n<the frame supdoc adds>\n    2 \u{2502} if x {\n\
             <the frame supdoc adds>"
        );
    }
}
