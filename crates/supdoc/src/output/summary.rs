//! The summary line: how many links are in each state (07 "Summary line").

use std::io::{self, Write};

use annotate_snippets::renderer::{AnsiColor, Style};

use crate::output::format::TextContext;
use crate::state::LinkState;

/// How many links are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct Summary {
    /// Links whose target still matches the lockfile.
    pub passed: usize,
    /// Links whose target changed since the doc was confirmed.
    pub failed: usize,
    /// Links whose target can't be found.
    pub broken: usize,
    /// Links with no lock entry yet.
    pub new: usize,
    /// Lock entries whose link no longer exists.
    pub obsolete: usize,
}

impl Summary {
    /// Counts `states`.
    #[must_use]
    pub fn tally(states: impl IntoIterator<Item = LinkState>) -> Self {
        let mut summary = Self::default();
        for state in states {
            *summary.slot(state) += 1;
        }
        summary
    }

    /// The count for `state`.
    #[must_use]
    pub fn count(&self, state: LinkState) -> usize {
        match state {
            LinkState::Passed => self.passed,
            LinkState::Failed => self.failed,
            LinkState::Broken => self.broken,
            LinkState::New => self.new,
            LinkState::Obsolete => self.obsolete,
        }
    }

    /// The counter for `state`.
    fn slot(&mut self, state: LinkState) -> &mut usize {
        match state {
            LinkState::Passed => &mut self.passed,
            LinkState::Failed => &mut self.failed,
            LinkState::Broken => &mut self.broken,
            LinkState::New => &mut self.new,
            LinkState::Obsolete => &mut self.obsolete,
        }
    }

    /// `Links  1 failed · 1 broken · 38 passed · 1 obsolete`: worst first, zero counts left out.
    ///
    /// A run with no links at all says `0 passed`, so the line is never empty.
    #[must_use]
    pub fn line(&self, color: bool) -> String {
        const WORST_FIRST: [LinkState; 5] = [
            LinkState::Failed,
            LinkState::Broken,
            LinkState::New,
            LinkState::Passed,
            LinkState::Obsolete,
        ];
        let mut parts: Vec<String> = WORST_FIRST
            .into_iter()
            .filter(|state| self.count(*state) > 0)
            .map(|state| paint(color, state, &format!("{} {state}", self.count(state))))
            .collect();
        if parts.is_empty() {
            parts.push(paint(color, LinkState::Passed, "0 passed"));
        }
        format!("Links  {}", parts.join(" · "))
    }
}

/// Wraps `text` in the color its state is shown in, when color is on.
fn paint(color: bool, state: LinkState, text: &str) -> String {
    if !color {
        return text.to_owned();
    }
    let style = match state {
        LinkState::Passed => Style::new().fg_color(Some(AnsiColor::Green.into())),
        LinkState::Failed | LinkState::Broken | LinkState::New => {
            Style::new().fg_color(Some(AnsiColor::Red.into()))
        }
        LinkState::Obsolete => Style::new().fg_color(Some(AnsiColor::Yellow.into())),
    };
    format!("{style}{text}{style:#}")
}

/// Writes the summary line, unless the verbosity hides it.
///
/// # Errors
///
/// Fails if writing to `out` fails.
pub fn render_summary(
    context: &TextContext,
    summary: &Summary,
    out: &mut dyn Write,
) -> io::Result<()> {
    if !context.verbosity.shows_summary() {
        return Ok(());
    }
    writeln!(out, "{}", summary.line(context.color))
}
