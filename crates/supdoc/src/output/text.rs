//! Text rendering of findings in the style of rustc and Cargo (07 G1).

use std::io::{self, Write};
use std::ops::Range;

use annotate_snippets::renderer::DecorStyle;
use annotate_snippets::{AnnotationKind, Group, Level, Origin, Renderer, Snippet};

use crate::finding::{Finding, Severity};
use crate::output::format::TextContext;

/// Writes one finding followed by a blank line: its code and message, where it is, the source
/// line with the link underlined, and a `help:` line.
///
/// Writes nothing when the verbosity hides the finding's severity. The source excerpt comes from
/// the finding's captured snippet; this function never reads files (07 G11).
///
/// # Errors
///
/// Fails if writing to `out` fails.
pub fn render_finding(
    context: &TextContext,
    finding: &Finding,
    out: &mut dyn Write,
) -> io::Result<()> {
    if !context.verbosity.shows(finding.severity) {
        return Ok(());
    }
    let level = match finding.severity {
        Severity::Error => Level::ERROR,
        Severity::Warning => Level::WARNING,
        Severity::Note => Level::NOTE,
    };
    let message = visible(&finding.message);
    let mut group = Group::with_title(level.primary_title(message.as_str()).id(finding.code));
    if let Some(location) = &finding.location {
        let path = visible(&context.paths.text(&location.file));
        group = match &finding.snippet {
            Some(snippet) => {
                let (text, highlight) = visible_with_span(&snippet.text, &snippet.highlight);
                group.element(
                    Snippet::source(text)
                        .line_start(usize::try_from(snippet.first_line).unwrap_or(1))
                        .path(path)
                        .fold(false)
                        .annotation(AnnotationKind::Primary.span(highlight)),
                )
            }
            None => group.element(
                Origin::path(path)
                    .line(usize::try_from(location.line).unwrap_or(1))
                    .char_column(usize::try_from(location.column).unwrap_or(1)),
            ),
        };
    }
    if let Some(help) = &finding.help {
        group = group.element(Level::HELP.message(visible(help)));
    }
    writeln!(out, "{}\n", renderer(context).render(&[group]))
}

/// The renderer for `context`: styled or plain, Unicode or ASCII, at the terminal's width.
fn renderer(context: &TextContext) -> Renderer {
    let base = if context.color {
        Renderer::styled()
    } else {
        Renderer::plain()
    };
    let decor = if context.unicode {
        DecorStyle::Unicode
    } else {
        DecorStyle::Ascii
    };
    base.decor_style(decor).term_width(context.width)
}

/// `text` with every control character except newline and tab spelled out, so text from
/// untrusted files can't drive the terminal (an ESC becomes the four characters `\x1b`).
#[must_use]
pub fn visible(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        push_visible(&mut shown, character);
    }
    shown
}

/// [`visible`] for `text`, with the byte range `span` moved to cover the same characters in the
/// result. Offsets that aren't in `text` are clamped to its end.
fn visible_with_span(text: &str, span: &Range<usize>) -> (String, Range<usize>) {
    let mut shown = String::with_capacity(text.len());
    let (mut start, mut end) = (None, None);
    for (offset, character) in text.char_indices() {
        if offset >= span.start && start.is_none() {
            start = Some(shown.len());
        }
        if offset >= span.end && end.is_none() {
            end = Some(shown.len());
        }
        push_visible(&mut shown, character);
    }
    let start = start.unwrap_or(shown.len());
    let end = end.unwrap_or(shown.len());
    (shown, start..end)
}

/// Appends `character`, spelled out if it is a control character other than newline or tab.
fn push_visible(shown: &mut String, character: char) {
    if character.is_control() && character != '\n' && character != '\t' {
        shown.push_str(&character.escape_default().to_string());
    } else {
        shown.push(character);
    }
}
