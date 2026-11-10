//! superhtml: its syntax errors, its version, and the blocks supdoc never gives it.
//!
//! superhtml reads the block on stdin (`fmt --stdin --syntax-only`) and prints it formatted on
//! stdout. It reads no config file, no `.editorconfig` and no environment variable, and its
//! style is fixed: tabs indent. It honors no comment or other in-block directive that switches
//! formatting off; comments are written back as they are. The format step judges its stdout
//! against the block (see [`super::format::judge`]), and `supdoc fix` writes only that stdout, so
//! a doctype or anything else the block holds changes only how superhtml reads it, never what
//! supdoc writes beyond superhtml's own output for it.
//!
//! Two kinds of block never reach it (see [`not_given`]). A block of only whitespace: superhtml
//! prints nothing for it and exits 0, and `fix` would write that nothing over the block. And a
//! block holding text superhtml rewrites though HTML keeps it as written: superhtml 0.7.1 keeps
//! only `<pre>` content verbatim, and re-indents the content of `<textarea>` (the field's value),
//! `<xmp>`, `<listing>`, `<plaintext>` and `<noscript>` (a `<pre>` inside it too), and each line
//! of a `<script>` or `<style>` body, which changes a multi-line template literal or a
//! backslash-continued string, and trims a one-line body's edge whitespace, which a data script
//! keeps. That is a limit of superhtml's formatter, so supdoc withholds the block rather than
//! judge or write its output.

use std::path::Path;

use super::format::ParseError;
use super::runner;
use super::tools::ToolEnv;

/// Why a block's format step didn't run when it holds only whitespace.
pub(crate) const BLANK: &str = "the block holds only whitespace, and superhtml prints nothing for \
                                such input instead of formatting it";

/// Why a block's format step didn't run when superhtml would change the content of an element
/// it holds (see [`Withheld::Rewritten`]).
pub(crate) const REWRITTEN: &str = "the block holds an element whose content superhtml rewrites \
                                    though HTML keeps it as written";

/// Why a block isn't given to superhtml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Withheld {
    /// It is empty or holds only whitespace ([`BLANK`]).
    Blank,
    /// It holds an element superhtml would change the content of ([`REWRITTEN`]).
    Rewritten {
        /// The element's name, lowercase.
        element: &'static str,
        /// The block line (0-based) its start tag is on.
        line: usize,
    },
}

impl Withheld {
    /// The format step's reason for the block.
    #[must_use]
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Blank => BLANK,
            Self::Rewritten { .. } => REWRITTEN,
        }
    }
}

/// The elements whose whole content superhtml re-indents though HTML keeps it as written: a
/// `<textarea>`'s content is the field's value, `<xmp>`, `<listing>` and `<plaintext>` render
/// preformatted, and `<noscript>` renders its content, a `<pre>` in it too, with scripting off.
const VERBATIM: [&str; 5] = ["textarea", "xmp", "listing", "plaintext", "noscript"];

/// The elements whose body superhtml re-indents line by line and trims at its edges: a body of
/// more than one non-blank line can hold a string literal whose inner whitespace is its value,
/// and a one-line body's edge whitespace is a data script's value.
const INDENTED: [&str; 2] = ["script", "style"];

/// Why `input`, an HTML block as its formatter reads it, isn't given to superhtml: it is empty
/// or holds only whitespace ([`Withheld::Blank`]), or it holds, first in the block, a start tag
/// of a [`VERBATIM`] element or of an [`INDENTED`] element whose body superhtml rewrites (see
/// [`body_rewritten`]) ([`Withheld::Rewritten`]). `None` when superhtml may run.
///
/// The scan is a superset of what superhtml parses as those elements: a start tag counts anywhere
/// (in a comment, a string, another element's content), in any case, with or without attributes,
/// closed or not. A body runs from the first `>` after the name to the element's end tag, or to
/// the end of the block when it has none.
#[must_use]
pub(crate) fn not_given(input: &str) -> Option<Withheld> {
    if input.chars().all(char::is_whitespace) {
        return Some(Withheld::Blank);
    }
    input.match_indices('<').find_map(|(at, _)| {
        let after = input.get(at + 1..)?;
        let line = input.get(..at)?.matches('\n').count();
        if let Some(element) = opened(after, &VERBATIM) {
            return Some(Withheld::Rewritten { element, line });
        }
        let element = opened(after, &INDENTED)?;
        let body = after.get(element.len()..)?;
        body_rewritten(body, element).then_some(Withheld::Rewritten { element, line })
    })
}

/// The one of `names` whose start tag `after`, the text after a `<`, opens: the name in any ASCII
/// case, then whitespace, `/`, `>` or the end of the block.
fn opened(after: &str, names: &[&'static str]) -> Option<&'static str> {
    names.iter().copied().find(|name| {
        after
            .get(..name.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(name))
            && after
                .get(name.len()..)
                .and_then(|rest| rest.chars().next())
                .is_none_or(|next| next.is_whitespace() || next == '/' || next == '>')
    })
}

/// Whether superhtml would change the meaning of the body of `element`, a `<script>` or `<style>`
/// whose start tag `rest` follows the name of: the body holds more than one non-blank line, or it
/// is on one line and starts or ends with whitespace. A body of one non-blank line spread over
/// several, the shape superhtml gives a body it moves onto its own line, is still given, so a
/// block `supdoc fix` formatted stays checkable. The body starts after the first `>`, or, in a
/// start tag never closed, right after the name, and ends at the element's end tag
/// ([`end_tag`]), else at the end of the block. A `<script>` body that holds `<!--` runs to the
/// end of the block: there a `</script>` may only close a script the comment opens, not the
/// element.
fn body_rewritten(rest: &str, element: &str) -> bool {
    let body = rest
        .find('>')
        .map_or(rest, |open| rest.get(open + 1..).unwrap_or_default());
    let end = end_tag(body, element).unwrap_or(body.len());
    let closed = body.get(..end).unwrap_or(body);
    let body = if element == "script" && closed.contains("<!--") {
        body
    } else {
        closed
    };
    if body.contains(['\n', '\r']) {
        body.split(['\n', '\r'])
            .filter(|line| !line.trim().is_empty())
            .count()
            > 1
    } else {
        body.starts_with(char::is_whitespace) || body.ends_with(char::is_whitespace)
    }
}

/// Where in `body` the first end tag of `element` starts: `</`, the name in any ASCII case, then
/// a tab, line feed, form feed, space, `/` or `>`, as HTML ends a raw-text body. `None` when it
/// has none.
fn end_tag(body: &str, element: &str) -> Option<usize> {
    body.match_indices("</").map(|(at, _)| at).find(|&at| {
        let name = body.get(at + 2..).unwrap_or_default();
        name.get(..element.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(element))
            && name
                .get(element.len()..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|next| matches!(next, '\t' | '\n' | '\x0C' | ' ' | '/' | '>'))
    })
}

/// What starts each error superhtml prints for a block read on stdin: `<stdin>:<row>:<col>: `.
const ERROR_PREFIX: &str = "<stdin>:";

/// The block line superhtml's first syntax error in `stderr` names. superhtml counts rows from 0
/// (`<stdin>:0:6: missing end tag` is the block's first line), as [`ParseError`] does, so the row
/// is taken as it is. Only a line that starts with `<stdin>:` counts: the source line superhtml
/// quotes under each error is indented. `None` for any other stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    stderr.lines().find_map(|line| {
        let rest = line.strip_prefix(ERROR_PREFIX)?;
        let (row, rest) = rest.split_once(':')?;
        let (col, _) = rest.split_once(':')?;
        if col.is_empty() || !col.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(ParseError {
            line: Some(row.parse::<usize>().ok()?),
        })
    })
}

/// The superhtml versions, as `<major>.<minor>`, supdoc's checks are verified against: supdoc
/// reads superhtml's error wording, and its output may change between minors. A minor is added
/// only after the HTML e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 1] = ["0.7"];

/// How a superhtml whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// The version `superhtml version` printed in `output` (`<major>.<minor>.<patch>` alone on its
/// line) when it is one supdoc is verified against (see [`VERIFIED`]).
///
/// # Errors
///
/// How to name the version to the user: the version it printed, or, when its output isn't a
/// version, that.
pub(crate) fn verified(output: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(output);
    let version = text.lines().next().unwrap_or_default().trim_end();
    let parts: Vec<&str> = version.split('.').collect();
    let readable = parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if !readable {
        return Err(UNREADABLE.to_owned());
    }
    let minor = version.rsplit_once('.').map_or(version, |(minor, _)| minor);
    if VERIFIED.contains(&minor) {
        Ok(version.to_owned())
    } else {
        Err(version.to_owned())
    }
}

/// [`verified`] on what `program`, a superhtml binary, prints for its `version` subcommand, run
/// as any tool is (see [`runner::run`]) under `timeout`. superhtml prints its version on stderr,
/// not stdout (`std.debug.print` in its `src/main.zig`), and has no `--version` flag: it prints
/// its usage for one and exits 1.
///
/// Effect: runs the binary.
///
/// # Errors
///
/// How to name the version to the user, as [`verified`] gives it; a run that failed or exited
/// unsuccessfully couldn't be read.
pub(crate) fn version_of(
    program: &Path,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<std::time::Duration>,
) -> Result<String, String> {
    let call = runner::Call {
        program,
        name: "superhtml",
        args: &[std::ffi::OsStr::new("version")],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Neutral,
        cap: runner::CAP,
    };
    match runner::run(&call, env, root) {
        Ok(ran) if ran.success => verified(&ran.stderr),
        _ => Err(UNREADABLE.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{BLANK, REWRITTEN, Withheld, not_given, parse_error, verified};
    use crate::code_blocks::format::ParseError;

    /// [`Withheld::Rewritten`] for `element` on block line `line`.
    const fn rewritten(element: &'static str, line: usize) -> Withheld {
        Withheld::Rewritten { element, line }
    }

    #[test]
    fn a_block_of_only_whitespace_is_never_given_to_superhtml() {
        for input in ["", "\n", "  \n\t\n", "\r\n"] {
            assert_eq!(not_given(input), Some(Withheld::Blank), "{input:?}");
        }
        for input in ["a", "<!-- note -->\n", "<!doctype html>\n", "<p>a</p>\n"] {
            assert_eq!(not_given(input), None, "{input:?}");
        }
        assert_eq!(Withheld::Blank.reason(), BLANK);
        assert_eq!(rewritten("xmp", 0).reason(), REWRITTEN);
    }

    #[test]
    fn a_verbatim_element_start_tag_anywhere_withholds_the_block() {
        for (input, element, line) in [
            ("<textarea>  x  </textarea>", "textarea", 0),
            (
                "<p>a</p>\n<form>\n<TextArea name=\"m\">\nhi</textarea>",
                "textarea",
                2,
            ),
            ("<textarea/>", "textarea", 0),
            ("<textarea", "textarea", 0),
            ("<textarea\nrows=2>", "textarea", 0),
            ("<!-- <xmp> -->", "xmp", 0),
            ("a\nb\n<XMP>a</XMP>", "xmp", 2),
            ("<listing>\n  a\n</listing>", "listing", 0),
            ("<plaintext>", "plaintext", 0),
            ("<noscript><p>a</p></noscript>", "noscript", 0),
            ("<p>a</p>\n<NoScript >\n<pre>  a</pre>", "noscript", 1),
            ("<noscript/>", "noscript", 0),
            ("<noscript", "noscript", 0),
            (
                "<p title='a'>\n<script>let s = '<textarea>';</script>",
                "textarea",
                1,
            ),
        ] {
            assert_eq!(
                not_given(input),
                Some(rewritten(element, line)),
                "{input:?}"
            );
        }
        for input in [
            "<textareas>",
            "<xmpx>a</xmpx>",
            "</textarea>",
            "<p>textarea</p>",
            "<pre>\n  a\n</pre>",
            "<plain>",
            "<noscripts>",
        ] {
            assert_eq!(not_given(input), None, "{input:?}");
        }
    }

    #[test]
    fn a_script_or_style_body_of_two_non_blank_lines_or_with_edge_space_withholds_the_block() {
        for (input, element, line) in [
            ("<script>\nconst s = `a\n  b`;\n</script>", "script", 0),
            (
                "<div>\n<SCRIPT type=\"module\">\nx()\ny()\n</SCRIPT>\n</div>",
                "script",
                1,
            ),
            (
                "<style>\n.a::before { content: \"a\\\n  b\"; }\n</style>",
                "style",
                0,
            ),
            ("<style media=\"print\">a{}\nb{}</style>", "style", 0),
            ("<script>a()\nb()", "script", 0),
            ("<script\na()\nb()", "script", 0),
            ("<script>a()\rb()</script>", "script", 0),
            ("<script>a() </scripts>\nb()</script>", "script", 0),
            ("<script>a() </script\r>\n</script>", "script", 0),
            (
                "<script><!--<script></script>\nb()\n--></script>",
                "script",
                0,
            ),
            (
                "<script>a()</script>\n<style>\na{}\nb{}</style>",
                "style",
                1,
            ),
            ("<script type=\"text/plain\"> x </script>", "script", 0),
            ("<script> a()</script>", "script", 0),
            ("<style>a{}\t</style>", "style", 0),
            ("<script> </script>", "script", 0),
            ("<script> a()", "script", 0),
        ] {
            assert_eq!(
                not_given(input),
                Some(rewritten(element, line)),
                "{input:?}"
            );
        }
        for input in [
            "<script>a()</script>",
            "<script src=\"a.js\"></script>\n<p>b</p>",
            "<style>a{}</style>\n<p>\nb\n</p>",
            "<script>a()</SCRIPT >\n<p>b</p>",
            "<script>a()</script/>\n",
            "<style>a{}</style\n>",
            "<script",
            "<scripts>\na\n</scripts>",
            "<style>a{}<!--</style>\nb",
            "<script></script>",
            "<script>\n\ta()\n</script>",
            "<div>\n\t<script type=\"text/plain\">\n\t\tx\n\t</script>\n</div>",
            "<script>\n\na()\n\n</script>",
            "<script>a()\n</script>",
            "<style>\r\na{}\r\n</style>",
        ] {
            assert_eq!(not_given(input), None, "{input:?}");
        }
    }

    #[test]
    fn a_syntax_error_keeps_superhtmls_zero_based_row() {
        assert_eq!(
            parse_error("<stdin>:0:6: missing end tag\n   <div><p>hi</div>\n         ^\n"),
            Some(ParseError { line: Some(0) })
        );
        assert_eq!(
            parse_error("<stdin>:2:0: missing end tag\n   <div>\n   ^^^^^\n"),
            Some(ParseError { line: Some(2) })
        );
    }

    #[test]
    fn only_an_unindented_stdin_error_line_names_a_row() {
        assert_eq!(
            parse_error("   <stdin>:4:1: quoted source\n<stdin>:1:3: syntax error: x\n"),
            Some(ParseError { line: Some(1) })
        );
        for stderr in [
            "",
            "error: something\n",
            "a.html:1:1: missing end tag\n",
            "<stdin>:x:1: bad\n",
            "<stdin>:1:: bad\n",
        ] {
            assert_eq!(parse_error(stderr), None, "{stderr:?}");
        }
    }

    #[test]
    fn only_a_verified_minor_passes_the_version_gate() {
        assert_eq!(verified(b"0.7.1\n"), Ok("0.7.1".to_owned()));
        assert_eq!(verified(b"0.7.12\n"), Ok("0.7.12".to_owned()));
        assert_eq!(verified(b"0.8.0\n"), Err("0.8.0".to_owned()));
        assert_eq!(verified(b"0.6.4\n"), Err("0.6.4".to_owned()));
        assert_eq!(verified(b"1.7.0\n"), Err("1.7.0".to_owned()));
        for unreadable in [
            &b"superhtml 0.7.1\n"[..],
            b"",
            b"0.7",
            b"0.7.1-dev\n",
            b"v0.7.1\n",
        ] {
            assert_eq!(
                verified(unreadable),
                Err("(whose version supdoc couldn't read)".to_owned()),
                "{:?}",
                String::from_utf8_lossy(unreadable)
            );
        }
    }
}
