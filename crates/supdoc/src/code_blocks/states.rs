//! One state per block, and the counts `check` reports for them.

use annotate_snippets::renderer::{AnsiColor, Style};

use crate::config::code_blocks::Language;

use super::block::CodeBlock;
use super::info::reasoned;
use super::tools::formatter;

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
/// 5. A tag of a configured language: [`NotChecked::Pending`] until its tool runs.
/// 6. Any other tag: [`UnknownLanguage`](Self::UnknownLanguage).
///
/// `check` then looks up each pending language's tool: a tool not on `PATH` makes its blocks
/// [`ToolMissing`](Self::ToolMissing), and one whose real path is inside the checkout makes them
/// [`NotChecked::ToolInsideRoot`]. A tool found outside it runs on each block of a language whose
/// format step exists (shell), which leaves it [`Passed`](Self::Passed) or
/// [`Failed`](Self::Failed); a Rust or Python block stays pending.
///
/// Only what the author wrote and the config decide a state: supdoc never guesses a block into a
/// skip state. `uses(...)` findings leave the state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlockState {
    /// Every check run on the block passed.
    Passed,
    /// A check failed, or an excerpt's source is missing, for the reason held.
    Failed(Failure),
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
    /// The tool the block's language needs isn't on `PATH`.
    ToolMissing,
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
    /// The tool's binary on `PATH` is inside the checkout, so the checkout chose it.
    ToolInsideRoot,
    /// The block's language has `formatter = false`.
    FormatOff,
    /// The author marked the block `no_format(...)`.
    FormatSkipped,
}

/// Why a block failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// One of its `from(...)` sources is missing.
    SourceMissing,
    /// Its formatter's output differs from it.
    Unformatted,
    /// Its tool exited unexpectedly or wrote past the output cap.
    ToolFailed,
    /// Its tool passed the `[limits] code-block-timeout` limit.
    ToolTimeout,
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
        Some(Excerpt::SourceMissing) => return BlockState::Failed(Failure::SourceMissing),
        Some(Excerpt::Stale) => return BlockState::Stale,
        Some(Excerpt::Current) => return BlockState::Passed,
        None => {}
    }
    let parsed = block.info.as_ref().map(|info| info.parsed());
    let ignored = parsed
        .iter()
        .flat_map(|parsed| &parsed.attributes)
        .find_map(|attribute| reasoned(attribute).filter(|found| found.name == "ignore"));
    if let Some(found) = ignored {
        return BlockState::OptedOut {
            reason: found.reason.map(str::to_owned),
        };
    }
    match tag_of(block) {
        None | Some("text" | "console" | "output") => BlockState::NotChecked(NotChecked::Text),
        Some(tag) => match Language::from_tag(tag) {
            Some(language) if on(language) => BlockState::NotChecked(NotChecked::Pending),
            Some(_) => BlockState::NotChecked(NotChecked::LanguageOff),
            None => BlockState::UnknownLanguage,
        },
    }
}

/// The language tag `block` is read as: the first word of its info string, or Rust for an untagged
/// fence in a Rust doc comment, as rustdoc reads it. `None` for an untagged block elsewhere.
#[must_use]
pub(crate) fn tag_of(block: &CodeBlock) -> Option<&str> {
    block
        .info
        .as_ref()
        .map(|info| info.parsed().language)
        .filter(|tag| !tag.is_empty())
        .or_else(|| block.default_language())
}

impl BlockState {
    /// The state's name in JSON.
    #[must_use]
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed(_) => "failed",
            Self::Stale => "stale",
            Self::OptedOut { .. } => "opted_out",
            Self::NotChecked(_) => "not_checked",
            Self::UnknownLanguage => "unknown_language",
            Self::ToolMissing => "tool_missing",
        }
    }

    /// Why the block is in this state, when it has a reason to give. `tag` is the block's
    /// language tag.
    #[must_use]
    pub(crate) fn reason(&self, tag: Option<&str>) -> Option<String> {
        match self {
            Self::Passed => None,
            Self::Failed(Failure::SourceMissing) => {
                Some("an excerpt's source is missing".to_owned())
            }
            Self::Failed(Failure::Unformatted) => {
                Some(format!("`{}` would format it differently", tool_of(tag)))
            }
            Self::Failed(Failure::ToolFailed) => Some(format!("`{}` failed", tool_of(tag))),
            Self::Failed(Failure::ToolTimeout) => {
                Some(format!("`{}` passed the time limit", tool_of(tag)))
            }
            Self::Stale => Some("the block differs from its source".to_owned()),
            Self::OptedOut { reason } => reason.clone(),
            Self::NotChecked(NotChecked::Text) => Some("the block is text, not code".to_owned()),
            Self::NotChecked(NotChecked::LanguageOff) => {
                let table = match tag.and_then(Language::from_tag) {
                    Some(Language::Rust) => "rust",
                    Some(Language::Shell) => "shell",
                    Some(Language::Python) => "python",
                    None => "language",
                };
                Some(format!("no `[features.code-blocks.{table}]` table"))
            }
            Self::NotChecked(NotChecked::Pending) => {
                Some("its format and lint steps aren't built yet".to_owned())
            }
            Self::NotChecked(NotChecked::ToolInsideRoot) => Some("tool inside checkout".to_owned()),
            Self::NotChecked(NotChecked::FormatOff) => Some("`formatter = false`".to_owned()),
            Self::NotChecked(NotChecked::FormatSkipped) => {
                Some("the author skipped formatting".to_owned())
            }
            Self::UnknownLanguage => {
                Some(format!("supdoc has no checker for `{}`", tag.unwrap_or("")))
            }
            Self::ToolMissing => Some(format!("`{}` isn't on PATH", tool_of(tag))),
        }
    }
}

/// The name of the formatter for blocks tagged `tag`, or `its tool`.
fn tool_of(tag: Option<&str>) -> &'static str {
    tag.and_then(Language::from_tag)
        .and_then(formatter)
        .map_or("its tool", |tool| tool.name)
}

/// How many blocks are in each state. `blocks` is the sum of the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
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
                BlockState::Failed(_) => &mut counts.failed,
                BlockState::Stale => &mut counts.stale,
                BlockState::OptedOut { .. } => &mut counts.opted_out,
                BlockState::NotChecked(NotChecked::FormatOff) => &mut counts.format_off,
                BlockState::NotChecked(NotChecked::FormatSkipped) => &mut counts.format_skipped,
                BlockState::NotChecked(_) => &mut counts.not_checked,
                BlockState::UnknownLanguage => &mut counts.unknown_language,
                BlockState::ToolMissing => &mut counts.tool_missing,
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
    use super::{BlockState, CodeBlockCounts, Excerpt, Failure, NotChecked, state_of};
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
            (
                Excerpt::SourceMissing,
                BlockState::Failed(Failure::SourceMissing),
            ),
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
    fn a_rust_doc_fence_leading_with_a_known_attribute_is_rust_with_that_attribute() {
        assert_eq!(
            state(&block("src/lib.rs", Some("no_run"))),
            BlockState::NotChecked(NotChecked::Pending)
        );
        assert_eq!(
            state(&block("src/lib.rs", Some("ignore(slow)"))),
            BlockState::OptedOut {
                reason: Some("slow".to_owned())
            }
        );
    }

    #[test]
    fn markdown_keeps_the_first_word_as_the_language_even_when_it_is_an_attribute() {
        assert_eq!(
            state(&block("docs/a.md", Some("no_run"))),
            BlockState::UnknownLanguage
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
            BlockState::Failed(Failure::Unformatted),
            BlockState::OptedOut { reason: None },
            BlockState::NotChecked(NotChecked::Text),
            BlockState::UnknownLanguage,
            BlockState::ToolMissing,
        ];

        let counts = CodeBlockCounts::tally(&states);

        assert_eq!(counts.blocks, 7);
        assert_eq!(
            counts.line(false, true),
            "Code blocks  1 failed · 1 tool missing · 1 unknown language · 1 not checked · 1 opted out · 2 passed"
        );
        assert_eq!(
            CodeBlockCounts::default().line(false, false),
            "Code blocks  0 passed"
        );
    }
}
