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
#[expect(
    dead_code,
    reason = "the first cartridge to list a fragment kind constructs it in P5-FR03"
)]
pub(crate) enum Support {
    /// The tool parses the kind as written.
    Native,
}
