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
    let message = glyphs(context, visible(&finding.message));
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
        group = group.element(Level::HELP.message(glyphs(context, visible(help))));
    }
    writeln!(out, "{}\n", renderer(context).render(&[group]))
}

/// `text` with each `→` spelled `->` when `context` draws in ASCII (07 G12). Finding messages
/// embed link keys, whose `Display` is Unicode, so the swap happens here at the output layer.
fn glyphs(context: &TextContext, text: String) -> String {
    if context.unicode {
        text
    } else {
        text.replace('→', "->")
    }
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
/// than tab, any format character (Unicode general category Cf, which covers the bidirectional
/// marks, overrides and isolates, the zero-width and invisible operators, and the tag
/// characters), or a line or paragraph separator.
const fn needs_escape(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E007F}'
    ) || (character.is_control() && character != '\t')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::fixtures::{context, finding_on};
    use crate::output::format::Verbosity;

    fn rendered(finding: &Finding) -> String {
        let mut bytes = Vec::new();
        render_finding(&context(Verbosity::Normal), finding, &mut bytes)
            .expect("writing to a buffer should succeed");
        String::from_utf8(bytes).expect("output should be UTF-8")
    }

    #[test]
    fn format_characters_are_escaped_in_message_path_and_snippet() {
        let samples = [
            ('\u{061C}', "\\u{61c}"),
            ('\u{E0041}', "\\u{e0041}"),
            ('\u{E0001}', "\\u{e0001}"),
            ('\u{180E}', "\\u{180e}"),
            ('\u{206A}', "\\u{206a}"),
            ('\u{FFF9}', "\\u{fff9}"),
            ('\u{0600}', "\\u{600}"),
            ('\u{110BD}', "\\u{110bd}"),
            ('\u{1D173}', "\\u{1d173}"),
        ];
        for (character, escaped) in samples {
            let text = format!("see a{character}b [x](a.rs#b)\n");
            let finding = finding_on(
                &format!("docs/a{character}.md"),
                &text,
                "[x](a.rs#b)",
                "link/failed",
                Severity::Error,
                &format!("bad{character}message"),
                "help",
            );

            let with_snippet = rendered(&finding);
            let without_snippet = rendered(&Finding {
                snippet: None,
                ..finding
            });

            for output in [&with_snippet, &without_snippet] {
                assert!(!output.contains(character), "{escaped}: {output:?}");
                assert!(
                    output.contains(&format!("bad{escaped}message")),
                    "{escaped} in message: {output}"
                );
                assert!(
                    output.contains(&format!("docs/a{escaped}.md")),
                    "{escaped} in path: {output}"
                );
            }
            assert!(
                with_snippet.contains(&format!("see a{escaped}b [x](a.rs#b)")),
                "{escaped} in snippet: {with_snippet}"
            );
        }
    }

    #[test]
    fn ordinary_text_is_not_escaped() {
        assert_eq!(
            visible("caf\u{e9} \u{4e2d}\u{6587}\ttab"),
            "caf\u{e9} \u{4e2d}\u{6587}\ttab"
        );
    }
}
