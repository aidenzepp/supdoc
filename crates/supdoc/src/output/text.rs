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

/// `text` for a single line of output, with every control, bidirectional-control, format and
/// line-separator character (newline and carriage return included, tab excepted) spelled out, so
/// text from untrusted files can't drive the terminal, reorder or hide what is shown, or fake an
/// extra line (an ESC becomes the six characters `\u{1b}`).
#[must_use]
pub fn visible(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        push_visible(&mut shown, character, false);
    }
    shown
}

/// Like [`visible`] for snippet `text`, which keeps its newlines because the renderer owns line
/// layout, with the byte range `span` moved to cover the same characters in the result. Offsets
/// that aren't in `text` are clamped to its end.
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
        push_visible(&mut shown, character, true);
    }
    let start = start.unwrap_or(shown.len());
    let end = end.unwrap_or(shown.len());
    (shown, start..end)
}

/// Appends `character`, spelled out if [`needs_escape`] says it can't be shown as itself.
/// `keep_newline` lets `\n` through for multi-line text.
fn push_visible(shown: &mut String, character: char, keep_newline: bool) {
    if (character == '\n' && keep_newline) || !needs_escape(character) {
        shown.push(character);
    } else {
        shown.push_str(&character.escape_default().to_string());
    }
}

/// Whether `character` could drive the terminal or mislead a reader: a control character other
/// than tab, a bidirectional override or isolate, an invisible format character, or a line or
/// paragraph separator.
const fn needs_escape(character: char) -> bool {
    matches!(
        character,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{00AD}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2060}'..='\u{2064}'
            | '\u{FEFF}'
            | '\u{2028}'
            | '\u{2029}'
    ) || (character.is_control() && character != '\t')
}
