//! The fragment kinds a block can declare with `fragment(<kind>)`, and how a tool says it checks one.

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
}

impl Support {
    /// Native support with nothing relaxed.
    pub(crate) const NATIVE: Self = Self::Native { relax: &[] };

    /// The rules left off for a block of this kind.
    #[must_use]
    pub(crate) const fn relax(self) -> &'static [&'static str] {
        match self {
            Self::Native { relax } => relax,
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
}
