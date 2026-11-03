//! One state per block, and the counts `check` reports for them.

use annotate_snippets::renderer::{AnsiColor, Style};

use crate::config::code_blocks::Language;

use super::block::CodeBlock;
use super::info::{InfoString, reasoned};

/// The one state a code block is in. A block takes the first that applies, in this order:
///
/// 1. A block with a `from(...)` attribute: [`Failed`](Self::Failed) when one of its sources is
///    missing, [`Stale`](Self::Stale) when its body differs from them, otherwise
///    [`Passed`](Self::Passed).
/// 2. `ignore(...)`: [`OptedOut`](Self::OptedOut), with its reason when it gives one.
/// 3. No tag, or `text`, `console` or `output`: [`NotChecked`] for [`NotChecked::Text`]. A fence
///    with no tag in a Rust doc comment is Rust, as rustdoc reads it.
/// 4. A Rust, shell or Python tag whose `[features.code-blocks.<language>]` table is absent:
///    [`NotChecked::LanguageOff`].
/// 5. A tag of a configured language: [`NotChecked::Pending`], until its format and lint steps
///    exist.
/// 6. Any other tag: [`UnknownLanguage`](Self::UnknownLanguage).
///
/// Only what the author wrote and the config decide a state: supdoc never guesses a block into a
/// skip state. `uses(...)` findings leave the state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlockState {
    /// Every check run on the block passed.
    Passed,
    /// A check failed, or an excerpt's source is missing.
    Failed,
    /// An excerpt differs from its source.
    Stale,
    /// The author wrote `ignore(...)`.
    OptedOut {
        /// The reason in the parentheses, if any.
        reason: Option<String>,
    },
    /// Nothing checked the block, for the reason held.
    NotChecked(NotChecked),
    /// The block's language has no checker in supdoc.
    UnknownLanguage,
}

/// Why a block was not checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotChecked {
    /// The block is untagged prose: no tag, `text`, `console` or `output`.
    Text,
    /// The block's language has no table under `[features.code-blocks]`.
    LanguageOff,
    /// The block's language is on, but its format and lint steps don't exist yet.
    Pending,
}

/// What a block's `from(...)` values came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Excerpt {
    /// Some value doesn't resolve.
    SourceMissing,
    /// The body differs from the parts.
    Stale,
    /// The body is the parts, or the doc couldn't be read to compare it.
    Current,
}

/// The state of `block`, by the order on [`BlockState`]. `excerpt` is what its `from(...)`
/// values came to, `None` when it has none; `on` says whether a language's table is written.
#[must_use]
pub(crate) fn state_of(
    block: &CodeBlock,
    excerpt: Option<Excerpt>,
    on: impl Fn(Language) -> bool,
) -> BlockState {
    match excerpt {
        Some(Excerpt::SourceMissing) => return BlockState::Failed,
        Some(Excerpt::Stale) => return BlockState::Stale,
        Some(Excerpt::Current) => return BlockState::Passed,
        None => {}
    }
    let parsed = block
        .info
        .as_ref()
        .map(|info| InfoString::parse(&info.text));
    let ignored = parsed
        .iter()
        .flat_map(|parsed| &parsed.attributes)
        .find_map(|attribute| reasoned(attribute).filter(|found| found.name == "ignore"));
    if let Some(found) = ignored {
        return BlockState::OptedOut {
            reason: found.reason.map(str::to_owned),
        };
    }
    let tag = parsed
        .map(|parsed| parsed.language)
        .filter(|tag| !tag.is_empty())
        .or_else(|| block.default_language());
    match tag {
        None | Some("text" | "console" | "output") => BlockState::NotChecked(NotChecked::Text),
        Some(tag) => match Language::from_tag(tag) {
            Some(language) if on(language) => BlockState::NotChecked(NotChecked::Pending),
            Some(_) => BlockState::NotChecked(NotChecked::LanguageOff),
            None => BlockState::UnknownLanguage,
        },
    }
}

/// How many blocks are in each state. `blocks` is the sum of the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CodeBlockCounts {
    /// Every block counted.
    pub(crate) blocks: usize,
    /// Blocks whose checks all passed.
    pub(crate) passed: usize,
    /// Blocks with a failed check or a missing excerpt source.
    pub(crate) failed: usize,
    /// Excerpts that differ from their source.
    pub(crate) stale: usize,
    /// Blocks the author marked `ignore(...)`.
    pub(crate) opted_out: usize,
    /// Blocks nothing checked: text, a language turned off, or a step not built yet.
    pub(crate) not_checked: usize,
    /// Blocks whose language has `formatter = false`.
    pub(crate) format_off: usize,
    /// Blocks the author marked `no_format(...)`.
    pub(crate) format_skipped: usize,
    /// Blocks whose language has `linter = false`.
    pub(crate) lint_off: usize,
    /// Blocks the author marked `no_lint(...)`.
    pub(crate) lint_skipped: usize,
    /// Blocks in a language supdoc has no checker for.
    pub(crate) unknown_language: usize,
    /// Blocks whose selected tool isn't installed.
    pub(crate) tool_missing: usize,
}

/// How bad a count is, for its color in the summary line.
#[derive(Clone, Copy)]
enum Tone {
    /// A failure.
    Bad,
    /// Something left unchecked that the author didn't choose.
    Unchecked,
    /// The author's own choice.
    Plain,
    /// All checks passed.
    Good,
}

impl CodeBlockCounts {
    /// Counts `states`.
    #[must_use]
    pub(crate) fn tally<'state>(states: impl IntoIterator<Item = &'state BlockState>) -> Self {
        let mut counts = Self::default();
        for state in states {
            counts.blocks += 1;
            *match state {
                BlockState::Passed => &mut counts.passed,
                BlockState::Failed => &mut counts.failed,
                BlockState::Stale => &mut counts.stale,
                BlockState::OptedOut { .. } => &mut counts.opted_out,
                BlockState::NotChecked(_) => &mut counts.not_checked,
                BlockState::UnknownLanguage => &mut counts.unknown_language,
            } += 1;
        }
        counts
    }

    /// `Code blocks  1 failed · 2 not checked · 3 passed`: worst first, zero counts left out.
    ///
    /// No blocks at all says `0 passed`, so the line is never empty. Without `unicode` the counts
    /// are separated by `, ` instead of `·`.
    #[must_use]
    pub(crate) fn line(&self, color: bool, unicode: bool) -> String {
        let worst_first = [
            (self.failed, "failed", Tone::Bad),
            (self.stale, "stale", Tone::Bad),
            (self.tool_missing, "tool missing", Tone::Unchecked),
            (self.unknown_language, "unknown language", Tone::Unchecked),
            (self.not_checked, "not checked", Tone::Unchecked),
            (self.format_off, "format off", Tone::Unchecked),
            (self.lint_off, "lint off", Tone::Unchecked),
            (self.format_skipped, "format skipped", Tone::Plain),
            (self.lint_skipped, "lint skipped", Tone::Plain),
            (self.opted_out, "opted out", Tone::Plain),
            (self.passed, "passed", Tone::Good),
        ];
        let mut parts: Vec<String> = worst_first
            .into_iter()
            .filter(|(count, _, _)| *count > 0)
            .map(|(count, label, tone)| paint(color, tone, &format!("{count} {label}")))
            .collect();
        if parts.is_empty() {
            parts.push(paint(color, Tone::Good, "0 passed"));
        }
        let separator = if unicode { " · " } else { ", " };
        format!("Code blocks  {}", parts.join(separator))
    }
}

/// Wraps `text` in its tone's color, when color is on.
fn paint(color: bool, tone: Tone, text: &str) -> String {
    let style = match tone {
        Tone::Bad => Style::new().fg_color(Some(AnsiColor::Red.into())),
        Tone::Unchecked => Style::new().fg_color(Some(AnsiColor::Yellow.into())),
        Tone::Good => Style::new().fg_color(Some(AnsiColor::Green.into())),
        Tone::Plain => return text.to_owned(),
    };
    if !color {
        return text.to_owned();
    }
    format!("{style}{text}{style:#}")
}

#[cfg(test)]
mod tests {
    use super::{BlockState, CodeBlockCounts, Excerpt, NotChecked, state_of};
    use crate::code_blocks::block::{CodeBlock, Info};
    use crate::config::code_blocks::Language;
    use crate::finding::Location;
    use crate::link::SourcePath;

    /// A block in `file` with `info` as its info string, or none.
    fn block(file: &str, info: Option<&str>) -> CodeBlock {
        let file: SourcePath = file
            .parse()
            .expect("the fixture path should be a source path");
        let location = Location {
            file,
            range: 0..1,
            line: 1,
            column: 1,
        };
        CodeBlock {
            fence: location.clone(),
            info: info.map(|text| Info {
                text: text.to_owned(),
                location,
            }),
            body: Vec::new(),
        }
    }

    /// The state of `block` with no excerpt and only Rust's table written.
    fn state(block: &CodeBlock) -> BlockState {
        state_of(block, None, |language| language == Language::Rust)
    }

    #[test]
    fn an_excerpt_takes_its_outcome_before_ignore() {
        let block = block("docs/a.md", Some("rust from(src/a.rs) ignore(why)"));
        let cases = [
            (Excerpt::SourceMissing, BlockState::Failed),
            (Excerpt::Stale, BlockState::Stale),
            (Excerpt::Current, BlockState::Passed),
        ];
        for (excerpt, expected) in cases {
            assert_eq!(state_of(&block, Some(excerpt), |_| true), expected);
        }
    }

    #[test]
    fn ignore_opts_out_with_its_reason_when_it_has_one() {
        assert_eq!(
            state(&block("docs/a.md", Some("rust ignore(needs a server)"))),
            BlockState::OptedOut {
                reason: Some("needs a server".to_owned())
            }
        );
        assert_eq!(
            state(&block("docs/a.md", Some("zsh ignore"))),
            BlockState::OptedOut { reason: None }
        );
    }

    #[test]
    fn untagged_and_text_blocks_are_not_checked_as_text() {
        for info in [
            None,
            Some(""),
            Some("text"),
            Some("console"),
            Some("output"),
        ] {
            assert_eq!(
                state(&block("docs/a.md", info)),
                BlockState::NotChecked(NotChecked::Text),
                "info {info:?}"
            );
        }
    }

    #[test]
    fn an_untagged_block_in_a_rust_doc_comment_is_rust() {
        assert_eq!(
            state(&block("src/lib.rs", None)),
            BlockState::NotChecked(NotChecked::Pending)
        );
    }

    #[test]
    fn a_language_without_its_table_is_off_and_with_it_pending() {
        assert_eq!(
            state(&block("docs/a.md", Some("rs"))),
            BlockState::NotChecked(NotChecked::Pending)
        );
        for tag in ["bash", "python3"] {
            assert_eq!(
                state(&block("docs/a.md", Some(tag))),
                BlockState::NotChecked(NotChecked::LanguageOff),
                "tag `{tag}`"
            );
        }
    }

    #[test]
    fn any_other_tag_is_an_unknown_language() {
        for tag in ["zsh", "rust,ignore", "toml"] {
            assert_eq!(
                state(&block("docs/a.md", Some(tag))),
                BlockState::UnknownLanguage,
                "tag `{tag}`"
            );
        }
    }

    #[test]
    fn blocks_is_the_sum_and_the_line_puts_the_worst_first() {
        let states = [
            BlockState::Passed,
            BlockState::Passed,
            BlockState::Failed,
            BlockState::OptedOut { reason: None },
            BlockState::NotChecked(NotChecked::Text),
            BlockState::UnknownLanguage,
        ];

        let counts = CodeBlockCounts::tally(&states);

        assert_eq!(counts.blocks, 6);
        assert_eq!(
            counts.line(false, true),
            "Code blocks  1 failed · 1 unknown language · 1 not checked · 1 opted out · 2 passed"
        );
        assert_eq!(
            CodeBlockCounts::default().line(false, false),
            "Code blocks  0 passed"
        );
    }
}
