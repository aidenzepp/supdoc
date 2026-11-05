//! The fragment kinds a block can declare with `fragment(<kind>)`, and how a tool says it checks one.
//!
//! A frame ([`Support::Framed`]) is one fixed pair of lines per tool and kind. Any name a frame
//! introduces that the block doesn't (a function, a type, a package) is spelled so no author
//! would write it, such as `supdocFragment$`, so a tool's message about the author's code can't
//! be mistaken for one about the frame. The spelling also draws no rule of the tool's own: no
//! leading underscore, which biome's `noDanglingUnderscore` flags, and a case its naming rules
//! accept. Each unit that adds a frame pair gives it a real-tool e2e
//! test asserting that no finding holds any frame line or frame-only name.

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

    /// The lines the tool sees before and after a block of this kind: `None` when it reads the
    /// kind as written.
    #[must_use]
    pub(crate) const fn frame(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Native { .. } => None,
            Self::Framed { before, after, .. } => Some((before, after)),
        }
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

        assert_eq!(framed.frame(), Some(("({", "})")));
        assert_eq!(framed.relax(), &[] as &[&str]);
        assert_eq!(Support::NATIVE.frame(), None);
    }
}
