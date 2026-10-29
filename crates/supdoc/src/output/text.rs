//! Text rendering of findings in the style of rustc and Cargo (07 G1).

use std::io::{self, Write};

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
    let mut group = Group::with_title(
        level
            .primary_title(finding.message.as_str())
            .id(finding.code),
    );
    if let Some(location) = &finding.location {
        let path = context.paths.text(&location.file);
        group = match &finding.snippet {
            Some(snippet) => group.element(
                Snippet::source(snippet.text.as_str())
                    .line_start(usize::try_from(snippet.first_line).unwrap_or(1))
                    .path(path)
                    .fold(false)
                    .annotation(AnnotationKind::Primary.span(snippet.highlight.clone())),
            ),
            None => group.element(
                Origin::path(path)
                    .line(usize::try_from(location.line).unwrap_or(1))
                    .char_column(usize::try_from(location.column).unwrap_or(1)),
            ),
        };
    }
    if let Some(help) = &finding.help {
        group = group.element(Level::HELP.message(help.as_str()));
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
