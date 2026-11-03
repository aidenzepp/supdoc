//! A code block's info string: its language and the attributes after it.
//!
//! Words are separated by spaces only, as strict CommonMark reads an info string: the language is
//! everything before the first space. Text inside parentheses stays whole, so
//! `ignore(needs a server)` is one attribute.

/// The words of one info string, borrowed from its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InfoString<'text> {
    /// The first word, as written. Empty only when the info string is blank.
    pub(crate) language: &'text str,
    /// Every later word, in order.
    pub(crate) attributes: Vec<&'text str>,
}

impl<'text> InfoString<'text> {
    /// Splits `text` into its language and attributes. Never fails: judging the words is the
    /// caller's job.
    #[must_use]
    pub(crate) fn parse(text: &'text str) -> Self {
        let mut words = words(text, char::is_whitespace);
        let language = words.next().unwrap_or_default();
        Self {
            language,
            attributes: words.collect(),
        }
    }

    /// The language word with its commas turned into spaces, when it has any outside parentheses:
    /// how `rust,ignore` is written so that `ignore` is an attribute.
    #[must_use]
    pub(crate) fn spaced_language(&self) -> Option<String> {
        let parts: Vec<&str> = words(self.language, |c| c == ',').collect();
        (parts.len() > 1).then(|| parts.join(" "))
    }
}

/// Whether `attribute` is one supdoc knows: `ignore`, `no_format` and `no_lint` with or without a
/// reason, `from(...)` and `uses(...)`, an edition, or one of the bare run attributes.
///
/// Whether a reason or an excerpt destination is well formed is judged elsewhere.
#[must_use]
pub(crate) fn is_known_attribute(attribute: &str) -> bool {
    if is_path_attribute(attribute) {
        return true;
    }
    reasoned(attribute).is_some()
        || matches!(
            attribute,
            "edition2015"
                | "edition2018"
                | "edition2021"
                | "edition2024"
                | "run"
                | "no_run"
                | "compile_fail"
                | "should_panic"
        )
}

/// Whether `attribute` is `from(PATH#SYMBOL)` or `uses(PATH#SYMBOL)` with a non-blank argument.
///
/// Whether the argument names a real excerpt source is judged elsewhere.
fn is_path_attribute(attribute: &str) -> bool {
    ["from(", "uses("].iter().any(|opening| {
        attribute
            .strip_prefix(opening)
            .and_then(|rest| rest.strip_suffix(')'))
            .is_some_and(|argument| !argument.trim().is_empty())
    })
}

/// The `from(...)` or `uses(...)` spelling for an attribute written `from=...` or `uses=...`, if
/// it is one.
#[must_use]
pub(crate) fn parenthesized_spelling(attribute: &str) -> Option<String> {
    ["from", "uses"].iter().find_map(|name| {
        let argument = attribute.strip_prefix(name)?.strip_prefix('=')?;
        Some(format!("{name}({argument})"))
    })
}

/// An `ignore`, `no_format` or `no_lint` attribute, split into its name and the reason it gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reasoned<'text> {
    /// `ignore`, `no_format` or `no_lint`.
    pub(crate) name: &'text str,
    /// The trimmed text between the parentheses. `None` when the attribute is bare or its
    /// parentheses are empty or blank.
    pub(crate) reason: Option<&'text str>,
}

/// Splits `attribute` when it is a well-formed `ignore`, `no_format` or `no_lint`, with or
/// without a parenthesized reason. `None` for any other word, including an unclosed parenthesis.
#[must_use]
pub(crate) fn reasoned(attribute: &str) -> Option<Reasoned<'_>> {
    let (name, rest) = match attribute.split_once('(') {
        Some((name, rest)) => (name, Some(rest.strip_suffix(')')?)),
        None => (attribute, None),
    };
    matches!(name, "ignore" | "no_format" | "no_lint").then(|| Reasoned {
        name,
        reason: rest.map(str::trim).filter(|reason| !reason.is_empty()),
    })
}

/// The attributes supdoc knows, as a help names them.
pub(crate) const KNOWN_ATTRIBUTES: &str = "`ignore(reason)`, `no_format(reason)`, \
    `no_lint(reason)`, `from(PATH#SYMBOL)`, `uses(PATH#SYMBOL)`, `edition2015` to `edition2024`, \
    `run`, `no_run`, `compile_fail` and `should_panic`";

/// The non-empty pieces of `text` between characters that `separates`, skipping separators inside
/// parentheses.
fn words(text: &str, separates: fn(char) -> bool) -> impl Iterator<Item = &str> {
    let mut depth = 0_usize;
    let mut start = 0;
    let mut pieces = Vec::new();
    for (offset, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 && separates(c) => {
                pieces.extend(text.get(start..offset));
                start = offset + c.len_utf8();
            }
            _ => {}
        }
    }
    pieces.extend(text.get(start..));
    pieces.into_iter().filter(|piece| !piece.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_separated_attributes_follow_the_language() {
        let info = InfoString::parse("rust no_run edition2021");

        assert_eq!(info.language, "rust");
        assert_eq!(info.attributes, ["no_run", "edition2021"]);
        assert_eq!(info.spaced_language(), None);
    }

    #[test]
    fn a_comma_stays_inside_the_language_word() {
        let info = InfoString::parse("rust,ignore lint");

        assert_eq!(info.language, "rust,ignore");
        assert_eq!(info.attributes, ["lint"]);
        assert_eq!(info.spaced_language().as_deref(), Some("rust ignore"));
    }

    #[test]
    fn a_parenthesized_reason_with_spaces_is_one_attribute() {
        let info = InfoString::parse("sh  ignore(needs a live server)   from(a.sh#run)");

        assert_eq!(info.language, "sh");
        assert_eq!(
            info.attributes,
            ["ignore(needs a live server)", "from(a.sh#run)"]
        );
    }

    #[test]
    fn known_attributes_are_the_closed_list() {
        let known = [
            "ignore",
            "ignore(slow)",
            "no_format(generated)",
            "no_lint",
            "from(../src/lib.rs#add)",
            "uses(/src/lib.rs#Limits.resolve)",
            "edition2024",
            "run",
            "no_run",
            "compile_fail",
            "should_panic",
        ];
        let unknown = [
            "lint",
            "edition2030",
            "no_run(why)",
            "ignore(open",
            "hidden",
            "rust",
            "from=../src/lib.rs#add",
            "uses=/src/lib.rs#Limits.resolve",
            "from()",
            "uses(  )",
            "from(open",
        ];

        for attribute in known {
            assert!(is_known_attribute(attribute), "{attribute} should be known");
        }
        for attribute in unknown {
            assert!(
                !is_known_attribute(attribute),
                "{attribute} should be unknown"
            );
        }
    }

    #[test]
    fn an_equals_spelling_maps_to_the_parenthesized_one() {
        assert_eq!(
            parenthesized_spelling("from=a.rs#f").as_deref(),
            Some("from(a.rs#f)")
        );
        assert_eq!(
            parenthesized_spelling("uses=a.rs#f").as_deref(),
            Some("uses(a.rs#f)")
        );
        assert_eq!(parenthesized_spelling("from(a.rs#f)"), None);
    }

    #[test]
    fn a_reason_may_hold_spaces() {
        let found = reasoned("ignore(needs a live server)");

        assert_eq!(found.and_then(|r| r.reason), Some("needs a live server"));
    }

    #[test]
    fn a_reason_may_hold_commas() {
        let found = reasoned("no_format(generated, then pasted)");

        assert_eq!(found.and_then(|r| r.reason), Some("generated, then pasted"));
    }

    #[test]
    fn a_reason_is_trimmed() {
        let found = reasoned("no_lint(  slow  )");

        assert_eq!(found.and_then(|r| r.reason), Some("slow"));
    }

    #[test]
    fn empty_or_blank_parentheses_carry_no_reason() {
        for attribute in ["ignore()", "ignore(  )"] {
            let found = reasoned(attribute);

            assert_eq!(found.map(|r| r.name), Some("ignore"), "{attribute}");
            assert_eq!(found.and_then(|r| r.reason), None, "{attribute}");
        }
    }

    #[test]
    fn a_bare_name_carries_no_reason_for_each_attribute() {
        for name in ["ignore", "no_format", "no_lint"] {
            let found = reasoned(name);

            assert_eq!(
                found,
                Some(Reasoned { name, reason: None }),
                "bare {name} should parse without a reason"
            );
        }
    }

    #[test]
    fn an_unclosed_parenthesis_is_not_a_reasoned_attribute() {
        for name in ["ignore", "no_format", "no_lint"] {
            let attribute = format!("{name}(open");

            assert_eq!(reasoned(&attribute), None, "{attribute}");
            assert!(!is_known_attribute(&attribute), "{attribute}");
        }
    }
}
