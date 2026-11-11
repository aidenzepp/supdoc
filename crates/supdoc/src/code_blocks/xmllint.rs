//! xmllint: its diagnostics, its formatted output, its version, and the blocks supdoc never
//! gives it.
//!
//! As a linter, xmllint (libxml2) reads the block on stdin and checks only that it is well-formed
//! XML with well-formed namespaces: it always gets exactly [`ARGS`], so it builds no output tree,
//! uses no network, reads no catalog, and never loads or expands an external DTD or entity (no
//! `--loaddtd`, `--valid`, `--noent`, `--dtdattr`, `--xinclude`, `--c14n*` or `--huge`). As a
//! formatter it always gets exactly [`FORMAT_ARGS`], the same but `--format` in place of
//! `--noout`, and indents by two spaces. It reads no config file, and its variables
//! (`XML_CATALOG_FILES`, `XMLLINT_INDENT` and the rest) never reach it (see
//! [`super::tools::ToolEnv`]).
//!
//! xmllint honors nothing in the block that switches its checks off: its man page lists only
//! command-line options (`--nowarning`, `--recover`, which supdoc never passes), and libxml2's
//! parser hands a comment or a processing instruction to the application as content, never reads
//! one as a directive. So no block can silence it.
//!
//! Three kinds of block never reach it (see [`withheld`]), each matched more widely than XML
//! would read it (any ASCII case, any whitespace after `<!`), so a near miss is withheld too:
//! a block whose DOCTYPE names an external identifier (`SYSTEM` or `PUBLIC`), a block that
//! declares an entity (`<!ENTITY`), whose expansion has no bound even without `--noent`, and a
//! block xmllint would read in an encoding other than UTF-8 (an XML declaration naming another
//! encoding, or a NUL character, by which libxml2 detects UTF-16), in which supdoc's own search
//! for the first two can't see what xmllint reads. A plain `<!DOCTYPE name>` is given to it.
//! Six more never reach its format step (see [`format_withheld`]), whose output would change
//! what they mean or how they are written beyond their layout. Every rewrite it does print is
//! read back as XML and kept only when it holds the block's own tree, with nothing changed but
//! whitespace the block already had between its nodes (see [`content`]).
//!
//! supdoc reads xmllint's stderr, never its exit status alone: a namespace error exits 0.

use std::path::Path;

use roxmltree::{Document, Node, NodeType, ParsingOptions};

use super::format::{self, ParseError};
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;

/// Every argument xmllint gets, in order: no output tree, no network, no catalog, the block on
/// stdin.
pub(crate) const ARGS: [&str; 4] = ["--noout", "--nonet", "--nocatalogs", "-"];

/// Every argument xmllint's format step gets, in order: the block indented by two spaces on
/// stdout, no network, no catalog, the block on stdin.
pub(crate) const FORMAT_ARGS: [&str; 4] = ["--format", "--nonet", "--nocatalogs", "-"];

/// The line `xmllint --format` starts its output with when the block has no XML declaration,
/// which supdoc takes off (see [`format_verdict`]).
const DECLARATION: &str = "<?xml version=\"1.0\"?>\n";

/// Why an XML block's format step didn't run when it is tagged `xhtml`.
pub(crate) const XHTML: &str = "xmllint writes an empty element as `<b/>`, which an XHTML page \
                                served as HTML reads as an open tag, so supdoc doesn't format \
                                blocks tagged `xhtml`";

/// Why an XML block's format step didn't run when it holds HTML content that an HTML parser may
/// read.
pub(crate) const FOREIGN: &str = "the block holds `foreignObject` or names the XHTML namespace, \
                                  whose empty elements xmllint writes as `<b/>`, which an HTML \
                                  parser reads as an open tag, so supdoc doesn't format it";

/// Why an XML block's format step didn't run when its DOCTYPE has an internal subset.
pub(crate) const SUBSET: &str = "the block's DOCTYPE has an internal subset (`[`), whose \
                                 declarations xmllint rewrites, so supdoc doesn't format it";

/// Why an XML block's format step didn't run when it starts with a byte order mark.
pub(crate) const BOM: &str = "the block starts with a byte order mark, which xmllint leaves out \
                              of its output, so supdoc doesn't format it";

/// Why an XML block's format step didn't run when xmllint's output holds another tree than the
/// block's (see [`content`]).
pub(crate) const CHANGED: &str = "xmllint would change the block's content, not only the \
                                  whitespace between its nodes, so supdoc doesn't format it";

/// Why an XML block's format step didn't run when supdoc couldn't read the block itself as XML
/// to compare xmllint's output with it (see [`content`]).
pub(crate) const UNREAD: &str = "supdoc couldn't read the block as XML to confirm xmllint keeps \
                                 its content, so supdoc doesn't format it";

/// Why an XML block's format step didn't run when it sets `xml:space`.
pub(crate) const SPACE: &str = "the block sets `xml:space`, whose preserved whitespace xmllint \
                                re-indents, so supdoc doesn't format it";

/// Why an XML block's format step didn't run when it holds a non-ASCII character without
/// declaring UTF-8.
pub(crate) const ESCAPED: &str = "the block holds a non-ASCII character but no XML declaration \
                                  naming `encoding=\"UTF-8\"`, so xmllint would write each such \
                                  character as a character reference, and supdoc doesn't format \
                                  it";

/// Why an XML block's lint step didn't run when its DOCTYPE names an external identifier.
pub(crate) const EXTERNAL: &str = "the block's DOCTYPE names an external identifier (`SYSTEM` or \
                                   `PUBLIC`), which supdoc doesn't give to xmllint";

/// Why an XML block's lint step didn't run when it declares an entity.
pub(crate) const ENTITY: &str =
    "the block declares an entity (`<!ENTITY`), which supdoc doesn't give to xmllint";

/// Why an XML block's lint step didn't run when xmllint would read it in another encoding.
pub(crate) const ENCODING: &str = "the block declares an encoding other than UTF-8 or holds a NUL \
                                   character, so supdoc can't read it as xmllint would, and \
                                   doesn't give it to xmllint";

/// Why a block isn't given to xmllint (see [`withheld`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Withheld {
    /// Its DOCTYPE names an external identifier: [`EXTERNAL`].
    External,
    /// It declares an entity: [`ENTITY`].
    Entity,
    /// xmllint would read it in an encoding other than UTF-8: [`ENCODING`].
    Encoding,
}

impl Withheld {
    /// Why the block's lint step didn't run, in a step's words.
    #[must_use]
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::External => EXTERNAL,
            Self::Entity => ENTITY,
            Self::Encoding => ENCODING,
        }
    }
}

/// Why `input` is never given to xmllint, checked in this order: it is read in another encoding
/// (an XML declaration whose `encoding` isn't `UTF-8`, or a NUL character), a DOCTYPE names an
/// external identifier (`SYSTEM` or `PUBLIC` between `<!DOCTYPE` and its first `>`, which holds
/// the DOCTYPE's own identifier, since neither a public identifier nor a keyword holds `>` or
/// `[`), or it declares an entity (`<!ENTITY` anywhere, so every entity the internal subset could
/// load or expand is caught). Keywords match in any ASCII case with any whitespace between
/// `<!` and the keyword, which XML itself never allows, so a near miss is withheld too. `None`
/// for any other block, a plain `<!DOCTYPE name>` too.
#[must_use]
pub(crate) fn withheld(input: &str) -> Option<Withheld> {
    let lower = input.to_ascii_lowercase();
    if lower.contains('\0') || declares_another_encoding(&lower) {
        return Some(Withheld::Encoding);
    }
    let names_external = declarations(&lower, "doctype").any(|rest| {
        let head = rest.split_once('>').map_or(rest, |(head, _)| head);
        head.contains("system") || head.contains("public")
    });
    if names_external {
        return Some(Withheld::External);
    }
    declarations(&lower, "entity")
        .next()
        .map(|_| Withheld::Entity)
}

/// The text from each declaration of `keyword` (lowercase) in `lower` to the end: `<!`, any
/// whitespace, then `keyword`.
fn declarations<'a>(lower: &'a str, keyword: &'a str) -> impl Iterator<Item = &'a str> {
    lower.match_indices("<!").filter_map(move |(at, open)| {
        let rest = lower.get(at..)?;
        let after = rest.get(open.len()..)?;
        after.trim_start().starts_with(keyword).then_some(rest)
    })
}

/// Whether some XML declaration in `lower` (from each `<?xml` to its `?>`, or to the end) names an
/// `encoding` that isn't `utf-8`, or one supdoc can't read: `encoding` followed by `=` and a value
/// that isn't quoted and closed. libxml2 honors only the declaration that starts the document;
/// every one is checked.
fn declares_another_encoding(lower: &str) -> bool {
    lower.match_indices("<?xml").any(|(at, _)| {
        let rest = lower.get(at..).unwrap_or_default();
        let declaration = rest.split_once("?>").map_or(rest, |(head, _)| head);
        declaration.split("encoding").skip(1).any(|after| {
            after
                .trim_start()
                .strip_prefix('=')
                .is_some_and(|value| quoted(value.trim_start()) != Some("utf-8"))
        })
    })
}

/// The value `text` starts with when it is quoted with `"` or `'` and closed. `None` otherwise.
fn quoted(text: &str) -> Option<&str> {
    let mut chars = text.chars();
    let quote = chars.next().filter(|c| matches!(c, '"' | '\''))?;
    chars.as_str().split_once(quote).map(|(inner, _)| inner)
}

/// Why `input`, a block tagged `tag`, is never given to xmllint's format step, beyond
/// [`withheld`], checked in this order: it is tagged `xhtml` ([`XHTML`]); it names
/// `foreignObject` or the XHTML namespace URI in any ASCII case anywhere ([`FOREIGN`]); a
/// DOCTYPE holds `[` before its first `>` ([`SUBSET`]), as every internal subset does; it names
/// `xml:space` in any ASCII case anywhere ([`SPACE`]), since xmllint indents an element of only
/// elements inside a preserved region too; it starts with a byte order mark ([`BOM`]); it holds
/// a character outside ASCII and doesn't start with an XML declaration naming `encoding="UTF-8"`
/// ([`ESCAPED`]). `None` for any other block.
#[must_use]
pub(crate) fn format_withheld(tag: &str, input: &str) -> Option<&'static str> {
    let lower = input.to_ascii_lowercase();
    let has_subset = declarations(&lower, "doctype").any(|rest| {
        rest.split_once('>')
            .map_or(rest, |(head, _)| head)
            .contains('[')
    });
    if tag == "xhtml" {
        Some(XHTML)
    } else if lower.contains("foreignobject") || lower.contains("http://www.w3.org/1999/xhtml") {
        Some(FOREIGN)
    } else if has_subset {
        Some(SUBSET)
    } else if lower.contains("xml:space") {
        Some(SPACE)
    } else if input.starts_with('\u{feff}') {
        Some(BOM)
    } else if !input.is_ascii() && !declares_utf_8(&lower) {
        Some(ESCAPED)
    } else {
        None
    }
}

/// Whether `input` starts with an XML declaration: `<?xml` then whitespace.
fn declares(input: &str) -> bool {
    input
        .strip_prefix("<?xml")
        .is_some_and(|rest| rest.starts_with([' ', '\t', '\n', '\r']))
}

/// Whether `lower` starts with an XML declaration (see [`declares`]) whose `encoding` is
/// `"utf-8"` or `'utf-8'`.
fn declares_utf_8(lower: &str) -> bool {
    if !declares(lower) {
        return false;
    }
    let declaration = lower.split_once("?>").map_or(lower, |(head, _)| head);
    declaration.split("encoding").skip(1).any(|after| {
        after
            .trim_start()
            .strip_prefix('=')
            .is_some_and(|value| quoted(value.trim_start()) == Some("utf-8"))
    })
}

/// xmllint's format step reports no syntax error supdoc places: every stderr is a tool failure
/// (see [`format_verdict`]).
#[must_use]
pub(crate) const fn parse_error(_stderr: &str) -> Option<ParseError> {
    None
}

/// Judges `ran`, `xmllint --format` on `input`: any stderr, a namespace error's after exit 0 too,
/// and an unsuccessful exit are a tool failure, never a rewrite. For a block that doesn't start
/// with an XML declaration (see [`declares`]), the output's first line must be exactly the one
/// xmllint adds ([`DECLARATION`]), which is taken off; output that doesn't start with it is a
/// tool failure. What is left is compared with the block as any formatter's output is (see
/// [`format::printed`]), and a rewrite is kept only when it holds the block's content (see
/// [`content`]), and output that isn't XML is a tool failure.
///
/// # Errors
///
/// Why the step didn't run when the rewrite changes the block's content ([`CHANGED`]) or the
/// block can't be read as XML to compare it ([`UNREAD`]).
pub(crate) fn format_verdict(input: &str, ran: &Ran) -> Result<format::Verdict, &'static str> {
    if !ran.success || !ran.stderr.is_empty() {
        return Ok(format::Verdict::ToolFailed(format::exit_detail(
            &ran.stderr,
        )));
    }
    let stdout = if declares(input) {
        ran.stdout.clone()
    } else if let Some(rest) = ran.stdout.strip_prefix(DECLARATION.as_bytes()) {
        rest.to_vec()
    } else {
        return Ok(format::Verdict::ToolFailed(format!(
            "xmllint didn't start its output with `{}`, as it does for a block with no XML \
             declaration",
            DECLARATION.trim_end()
        )));
    };
    let output = match format::printed(input, stdout) {
        format::Verdict::Unformatted(output) => output,
        verdict => return Ok(verdict),
    };
    match content(input, &output) {
        Content::Kept => Ok(format::Verdict::Unformatted(output)),
        Content::Changed => Err(CHANGED),
        Content::InputUnread => Err(UNREAD),
        Content::OutputUnread(error) => Ok(format::Verdict::ToolFailed(format!(
            "xmllint printed output supdoc can't read as XML: {error}"
        ))),
    }
}

/// What reading a block and xmllint's rewrite of it back as XML found (see [`content`]).
#[derive(Debug, PartialEq, Eq)]
enum Content {
    /// The rewrite holds the block's tree.
    Kept,
    /// The rewrite holds another tree than the block's.
    Changed,
    /// The block itself isn't XML supdoc can read.
    InputUnread,
    /// The rewrite isn't XML supdoc can read; the detail says why.
    OutputUnread(String),
}

/// Reads `input` and `output`, xmllint's rewrite of it, as XML (see [`read`]) and compares their
/// trees node by node in document order: the same kind of node with as many children; elements
/// with the same expanded name, the same namespaces in scope, and the same attributes by expanded
/// name and parsed value, in any order; comments and processing instructions with the same text;
/// text nodes with the same parsed text, except that a text node of only XML whitespace may hold
/// other XML whitespace. So no text node is added or taken away, and a character reference
/// written as its character, or an attribute quoted another way, is the same content.
fn content(input: &str, output: &str) -> Content {
    let Ok(before) = read(input) else {
        return Content::InputUnread;
    };
    let after = match read(output) {
        Ok(after) => after,
        Err(error) => return Content::OutputUnread(error.to_string()),
    };
    let mut was = before.root().descendants();
    let mut is = after.root().descendants();
    loop {
        match (was.next(), is.next()) {
            (None, None) => return Content::Kept,
            (Some(was), Some(is)) if same_node(was, is) => {}
            _ => return Content::Changed,
        }
    }
}

/// Reads `text` as XML for [`content`], with a DOCTYPE allowed.
///
/// # Errors
///
/// roxmltree's error when `text` isn't XML it reads.
fn read(text: &str) -> Result<Document<'_>, roxmltree::Error> {
    Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: true,
            ..ParsingOptions::default()
        },
    )
}

/// Whether `was`, a node of the block, and `is`, the node at its place in xmllint's rewrite, are
/// the same apart from what their children hold (see [`content`]).
fn same_node(was: Node<'_, '_>, is: Node<'_, '_>) -> bool {
    if was.node_type() != is.node_type() || was.children().count() != is.children().count() {
        return false;
    }
    match was.node_type() {
        NodeType::Root => true,
        NodeType::Element => {
            expanded(was) == expanded(is)
                && namespaces(was) == namespaces(is)
                && attributes(was) == attributes(is)
        }
        NodeType::PI => was.pi() == is.pi(),
        NodeType::Comment => was.text() == is.text(),
        NodeType::Text => {
            let (was, is) = (
                was.text().unwrap_or_default(),
                is.text().unwrap_or_default(),
            );
            was == is || (only_space(was) && only_space(is))
        }
    }
}

/// An element's expanded name: its namespace URI, if any, and its local name.
fn expanded<'a>(element: Node<'a, '_>) -> (Option<&'a str>, &'a str) {
    let name = element.tag_name();
    (name.namespace(), name.name())
}

/// The namespaces `element` has in scope, as prefix and URI, sorted.
fn namespaces<'a>(element: Node<'a, '_>) -> Vec<(Option<&'a str>, &'a str)> {
    let mut all: Vec<_> = element
        .namespaces()
        .map(|namespace| (namespace.name(), namespace.uri()))
        .collect();
    all.sort_unstable();
    all
}

/// The attributes of `element`, as namespace URI, local name and parsed value, sorted.
fn attributes<'a>(element: Node<'a, '_>) -> Vec<(Option<&'a str>, &'a str, &'a str)> {
    let mut all: Vec<_> = element
        .attributes()
        .map(|attribute| (attribute.namespace(), attribute.name(), attribute.value()))
        .collect();
    all.sort_unstable();
    all
}

/// Whether `text` holds only XML whitespace (space, tab, line feed, carriage return), at least
/// one.
fn only_space(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
}

/// The diagnostic a line of xmllint's stderr starts: `-:<line>: <kind> : <message>`, where kind
/// ends with `error` or `warning` (`parser error`, `namespace error`, `parser warning`). `None`
/// for any other line.
fn header(line: &str) -> Option<Diagnostic> {
    let rest = line.strip_prefix("-:")?;
    let (number, rest) = rest.split_once(':')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (kind, message) = rest.strip_prefix(' ')?.split_once(" : ")?;
    if !(kind.ends_with("error") || kind.ends_with("warning")) || kind.contains(':') {
        return None;
    }
    let line = number.parse::<usize>().ok()?;
    Some(Diagnostic {
        line,
        column: 1,
        end_line: line,
        syntax: kind.ends_with("error"),
        rule: kind.to_owned(),
        message: message.trim_end().to_owned(),
    })
}

/// The diagnostics in xmllint's `stderr`: one per header line (see [`header`]), the lines after
/// each being the source and caret xmllint quotes under it. `None` when a line comes before the
/// first header, which isn't a diagnostic supdoc reads.
fn diagnostics(stderr: &str) -> Option<Vec<Diagnostic>> {
    let mut found: Vec<Diagnostic> = Vec::new();
    for line in stderr.lines() {
        match header(line) {
            Some(diagnostic) => found.push(diagnostic),
            None if found.is_empty() => return None,
            None => {}
        }
    }
    Some(found)
}

/// Judges an xmllint run by what it wrote, never its exit status alone, since a namespace error
/// exits 0: anything on stdout is a tool failure (`--noout` writes nothing there), and so is
/// stderr supdoc can't read as diagnostics; each diagnostic, error or warning, is a
/// `code-block-lint` at its doc line, its kind as the rule. A run that wrote nothing is clean
/// when it exited successfully and a tool failure when it didn't. xmllint checks no fragments,
/// so no run is framed.
#[must_use]
pub(crate) fn lint_verdict(
    _input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    let failed = |detail| lint::Verdict::Failed(format::Verdict::ToolFailed(detail));
    let ran = match result {
        Ok(ran) => ran,
        Err(failure) => return lint::Verdict::Failed(format::failed(failure)),
    };
    if !ran.stdout.is_empty() {
        return failed(format!(
            "xmllint wrote to stdout, which `--noout` keeps empty: {}",
            format::exit_detail(&ran.stdout)
        ));
    }
    let stderr = String::from_utf8_lossy(&ran.stderr);
    match diagnostics(&stderr) {
        Some(found) if !found.is_empty() => lint::Verdict::Diagnostics(found),
        Some(_) if ran.success => lint::Verdict::Clean,
        _ => failed(format::exit_detail(&ran.stderr)),
    }
}

/// The libxml2 versions, as `<major>.<minor>`, supdoc's checks are verified against: supdoc reads
/// xmllint's diagnostic wording, and libxml2 changes it and its loading defaults between minors.
/// A minor is added only after the XML e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 2] = ["2.9", "2.15"];

/// What `xmllint --version` prints before libxml2's version number.
const VERSION_MARK: &str = "using libxml version ";

/// How an xmllint whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// The version `xmllint --version` printed in `stderr` when it is one supdoc is verified against
/// (see [`VERIFIED`]). Its first line is `<program>: using libxml version <number>`, the number
/// being `major * 10000 + minor * 100 + patch` (`20913` is 2.9.13), with anything after its
/// digits (such as `-GITv2.9.13`) ignored.
///
/// # Errors
///
/// How to name the version to the user: `<major>.<minor>.<patch>`, or, when its output isn't a
/// version, that.
pub(crate) fn verified(stderr: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(stderr);
    let first = text.lines().next().unwrap_or_default();
    let digits: String = first
        .split_once(VERSION_MARK)
        .map(|(_, rest)| rest.chars().take_while(char::is_ascii_digit).collect())
        .unwrap_or_default();
    let number = (digits.len() >= 5)
        .then(|| digits.parse::<u64>().ok())
        .flatten()
        .ok_or_else(|| UNREADABLE.to_owned())?;
    let (major, minor, patch) = (number / 10000, number / 100 % 100, number % 100);
    let version = format!("{major}.{minor}.{patch}");
    if VERIFIED.contains(&format!("{major}.{minor}").as_str()) {
        Ok(version)
    } else {
        Err(version)
    }
}

/// [`verified`] on what `program`, an xmllint binary, prints on stderr for `--version`, run as any
/// tool is (see [`runner::run`]) under `timeout`.
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
        name: "xmllint",
        args: &[std::ffi::OsStr::new("--version")],
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
    use super::*;

    /// A finished run that wrote `stdout` and `stderr` and exited `success`fully or not.
    fn ran(success: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_doctype_naming_system_or_public_in_any_case_or_spacing_is_withheld() {
        let blocks = [
            "<!DOCTYPE a SYSTEM \"a.dtd\">\n<a/>",
            "<!DOCTYPE a PUBLIC \"-//X//DTD A//EN\" \"http://x/a.dtd\">\n<a/>",
            "<!doctype a system \"a.dtd\"><a/>",
            "<! DocType\ta\n  SyStEm 'a.dtd'><a/>",
            "<!\u{a0}DOCTYPE a PUBLIC 'p' 's'><a/>",
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \"x\">\n<html/>",
            "<!DOCTYPE a [<!NOTATION n SYSTEM \"n\">]><a/>",
        ];
        for block in blocks {
            assert_eq!(withheld(block), Some(Withheld::External), "block {block:?}");
        }
    }

    #[test]
    fn an_entity_declaration_in_any_case_or_spacing_is_withheld_even_internal() {
        let blocks = [
            "<!DOCTYPE a [<!ENTITY e \"x\">]>\n<a>&e;</a>",
            "<!DOCTYPE a [<!entity % p \"x\">]><a/>",
            "<!DOCTYPE a [<! Entity e 'x'>]><a/>",
            "<a><!ENTITY e 'x'></a>",
        ];
        for block in blocks {
            assert_eq!(withheld(block), Some(Withheld::Entity), "block {block:?}");
        }
    }

    #[test]
    fn another_declared_encoding_or_a_nul_is_withheld_and_utf_8_is_not() {
        let withheld_blocks = [
            "<?xml version=\"1.0\" encoding=\"UTF-7\"?>+ADw-+ACE-DOCTYPE a+AD4-<a/>",
            "<?xml version='1.0' encoding = 'UTF-16'?><a/>",
            "<?XML version=\"1.0\" Encoding=\"ISO-8859-1\"?><a/>",
            "\u{feff}<?xml version=\"1.0\" encoding=\"IBM037\"?><a/>",
            "  <?xml version=\"1.0\" encoding=\"latin1\"?><a/>",
            "<?xml version=\"1.0\" encoding=\"UTF-8?><a/>",
            "<?xml version=\"1.0\" encoding=UTF-8?><a/>",
            "<\0?\0x\0m\0l\0",
            "<a>\0</a>",
        ];
        for block in withheld_blocks {
            assert_eq!(withheld(block), Some(Withheld::Encoding), "block {block:?}");
        }
        let given = [
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a/>",
            "<?xml version='1.0' encoding='utf-8'?><a/>",
            "<?xml version=\"1.0\"?><a encoding=\"base64\">x</a>",
            "<a encoding=\"base64\">x</a>",
        ];
        for block in given {
            assert_eq!(withheld(block), None, "block {block:?}");
        }
    }

    #[test]
    fn a_plain_doctype_and_ordinary_xml_are_given() {
        let blocks = [
            "<!DOCTYPE a>\n<a/>",
            "<!DOCTYPE html>\n<html><body>system</body></html>",
            "<a><b>public entity</b><!-- note --></a>",
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><path d=\"M0 0\"/></svg>",
            "",
        ];
        for block in blocks {
            assert_eq!(withheld(block), None, "block {block:?}");
        }
    }

    #[test]
    fn each_reason_names_its_kind() {
        assert_eq!(Withheld::External.reason(), EXTERNAL);
        assert_eq!(Withheld::Entity.reason(), ENTITY);
        assert_eq!(Withheld::Encoding.reason(), ENCODING);
    }

    #[test]
    fn an_xhtml_tag_xml_space_or_unescaped_non_ascii_withholds_only_the_format_step() {
        assert_eq!(format_withheld("xhtml", "<html/>\n"), Some(XHTML));
        for tag in ["xml", "svg", "xsd", "xsl", "xslt"] {
            assert_eq!(format_withheld(tag, "<a><b/></a>\n"), None, "tag {tag}");
        }
        for block in [
            "<svg><foreignObject><p>x</p></foreignObject></svg>\n",
            "<svg><FOREIGNOBJECT/></svg>\n",
            "<a xmlns:h=\"http://www.w3.org/1999/xhtml\"><h:br></h:br></a>\n",
        ] {
            assert_eq!(
                format_withheld("svg", block),
                Some(FOREIGN),
                "block {block:?}"
            );
        }
        for block in [
            "<!DOCTYPE a [<!ELEMENT a ANY>]>\n<a/>\n",
            "<! doctype a[]><a/>\n",
        ] {
            assert_eq!(
                format_withheld("xml", block),
                Some(SUBSET),
                "block {block:?}"
            );
        }
        assert_eq!(format_withheld("xml", "<!DOCTYPE a>\n<a>[x]</a>\n"), None);
        assert_eq!(
            format_withheld(
                "xml",
                "\u{feff}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a/>\n"
            ),
            Some(BOM)
        );
        for block in [
            "<a xml:space=\"preserve\"><b/></a>\n",
            "<a><b XML:Space='default'/></a>\n",
        ] {
            assert_eq!(
                format_withheld("xml", block),
                Some(SPACE),
                "block {block:?}"
            );
        }
        for block in [
            "<a>\u{e9}</a>\n",
            "<a b=\"\u{2713}\"/>\n",
            "<?xml version=\"1.0\"?>\n<a>\u{e9}</a>\n",
            "<a/>\n<?xml version=\"1.0\" encoding=\"UTF-8\"?><!-- \u{e9} -->\n",
        ] {
            assert_eq!(
                format_withheld("xml", block),
                Some(ESCAPED),
                "block {block:?}"
            );
        }
        for block in [
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a>\u{e9}</a>\n",
            "<?xml version='1.0' encoding='utf-8'?><a>\u{e9}</a>\n",
            "<a>&#xE9;</a>\n",
        ] {
            assert_eq!(format_withheld("xml", block), None, "block {block:?}");
        }
    }

    #[test]
    fn the_added_declaration_line_comes_off_only_when_the_block_had_none() {
        let declared = "<?xml version=\"1.0\"?>\n";
        let judged = |input: &str, stdout: &str| format_verdict(input, &ran(true, stdout, ""));

        assert_eq!(
            judged(
                "<a>\n  <b/>\n</a>\n",
                &format!("{declared}<a>\n  <b/>\n</a>\n")
            ),
            Ok(format::Verdict::Formatted)
        );
        assert_eq!(
            judged(
                "<a>\n<b/>\n</a>\n",
                &format!("{declared}<a>\n  <b/>\n</a>\n")
            ),
            Ok(format::Verdict::Unformatted(
                "<a>\n  <b/>\n</a>\n".to_owned()
            ))
        );
        assert_eq!(
            judged(&format!("{declared}<a/>\n"), &format!("{declared}<a/>\n")),
            Ok(format::Verdict::Formatted),
            "a block's own declaration is kept and compared"
        );
        assert_eq!(
            judged(
                "<?xml version=\"1.0\"  ?>\n<a/>\n",
                &format!("{declared}<a/>\n")
            ),
            Ok(format::Verdict::Unformatted(format!("{declared}<a/>\n"))),
            "a block's own declaration is never taken off, even when xmllint rewrites it"
        );
        assert_eq!(
            judged("<a/>\n", &format!("{declared}{declared}<a/>\n")),
            Ok(format::Verdict::Unformatted(format!("{declared}<a/>\n"))),
            "only one line comes off"
        );
        assert_eq!(
            judged(
                "<?xml-stylesheet href=\"s\"?>\n<a/>\n",
                &format!("{declared}<?xml-stylesheet href=\"s\"?>\n<a/>\n")
            ),
            Ok(format::Verdict::Formatted),
            "a processing instruction named like the declaration isn't one"
        );
        for stdout in [
            "<a/>\n",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<a/>\n",
            "",
        ] {
            assert!(
                matches!(
                    judged("<a/>\n", stdout),
                    Ok(format::Verdict::ToolFailed(detail)) if detail.contains("didn't start its output")
                ),
                "stdout {stdout:?}"
            );
        }
    }

    #[test]
    fn any_stderr_or_an_unsuccessful_exit_is_a_tool_failure_never_a_rewrite() {
        let output = "<?xml version=\"1.0\"?>\n<x:a/>\n";
        let cases = [
            (
                true,
                "-:1: namespace error : Namespace prefix x on a is not defined\n",
            ),
            (false, "-:1: parser error : Document is empty\n"),
            (false, ""),
        ];
        for (success, stderr) in cases {
            let verdict = format_verdict("<x:a/>\n", &ran(success, output, stderr));
            assert!(
                matches!(verdict, Ok(format::Verdict::ToolFailed(_))),
                "exit {success}, stderr {stderr:?}: got {verdict:?}"
            );
        }
        assert_eq!(parse_error("-:1: parser error : x\n"), None);
    }

    #[test]
    fn a_rewrite_that_only_changes_whitespace_between_nodes_keeps_the_content() {
        let kept = [
            ("<a>\n<b>x</b>\n</a>", "<a>\n  <b>x</b>\n</a>\n"),
            ("<a></a>", "<a/>"),
            ("<a b=\"1\" c=\"2\"/>", "<a c=\"2\" b=\"1\"/>"),
            ("<a b='x\"y'>&#65;&quot;</a>", "<a b=\"x&quot;y\">A\"</a>"),
            ("<a b=\"x\ty\"/>", "<a b=\"x y\"/>"),
            (
                "<a>\r\n<!-- c -->\n<?p d?></a>",
                "<a>\n  <!-- c -->\n  <?p d?></a>",
            ),
            ("<a><![CDATA[<x>]]></a>", "<a>&lt;x&gt;</a>"),
            (
                "<p:a xmlns:p=\"u\">\n<p:b/>\n</p:a>",
                "<p:a xmlns:p=\"u\">\n  <p:b/>\n</p:a>",
            ),
            ("<!DOCTYPE a>\n<a/>", "<!DOCTYPE a>\n<a/>\n"),
        ];
        for (input, output) in kept {
            assert_eq!(
                content(input, output),
                Content::Kept,
                "{input:?} -> {output:?}"
            );
        }
    }

    #[test]
    fn a_rewrite_that_adds_removes_or_changes_a_node_changes_the_content() {
        let changed = [
            (
                "<a><b/></a>",
                "<a>\n  <b/>\n</a>",
                "a whitespace text node added",
            ),
            (
                "<text><tspan>Hel</tspan><tspan>lo</tspan></text>",
                "<text>\n  <tspan>Hel</tspan>\n  <tspan>lo</tspan>\n</text>",
                "whitespace added between inline elements",
            ),
            (
                "<a>\n  <![CDATA[x]]>\n</a>",
                "<a><![CDATA[x]]></a>",
                "whitespace beside CDATA dropped",
            ),
            (
                "<a>\n<!-- c -->\n  t\n</a>",
                "<a><!-- c -->\n  t\n</a>",
                "a whitespace node removed",
            ),
            ("<a> </a>", "<a>x</a>", "whitespace turned into text"),
            ("<a>x</a>", "<a> </a>", "text turned into whitespace"),
            ("<a>x y</a>", "<a>x  y</a>", "text changed"),
            ("<a b=\"1\"/>", "<a b=\"2\"/>", "an attribute value changed"),
            (
                "<a b=\"x&#9;y\"/>",
                "<a b=\"x\ty\"/>",
                "a tab reference written as a tab, which an attribute reads as a space",
            ),
            ("<a b=\"1\"/>", "<a/>", "an attribute removed"),
            ("<a/>", "<a b=\"1\"/>", "an attribute added"),
            ("<a><b/></a>", "<a><c/></a>", "an element renamed"),
            (
                "<a><b/><c/></a>",
                "<a><b><c/></b></a>",
                "an element moved under another",
            ),
            ("<a><b/></a>", "<a><b/><b/></a>", "an element added"),
            (
                "<a xmlns=\"u\"/>",
                "<a xmlns=\"v\"/>",
                "a namespace changed",
            ),
            (
                "<a xmlns:p=\"u\" b=\"p:c\"/>",
                "<a xmlns:p=\"v\" b=\"p:c\"/>",
                "a prefix bound to another namespace",
            ),
            ("<a><!-- c --></a>", "<a><!--c--></a>", "a comment changed"),
            (
                "<a><!-- c --></a>",
                "<a> c </a>",
                "a comment turned into text",
            ),
            (
                "<a><?p d?></a>",
                "<a><?p e?></a>",
                "a processing instruction changed",
            ),
            (
                "<!-- c -->\n<a/>",
                "<a/>",
                "a comment outside the root removed",
            ),
        ];
        for (input, output, what) in changed {
            assert_eq!(
                content(input, output),
                Content::Changed,
                "{what}: {input:?} -> {output:?}"
            );
        }
    }

    #[test]
    fn a_block_or_rewrite_supdoc_cant_read_as_xml_is_never_kept() {
        assert_eq!(content("<a>", "<a/>"), Content::InputUnread);
        assert!(matches!(content("<a/>", "<a>"), Content::OutputUnread(_)));
        let judged = |stdout: &str| format_verdict("<a/>\n", &ran(true, stdout, ""));
        assert_eq!(
            judged("<?xml version=\"1.0\"?>\n<a>\n  <b/>\n</a>\n"),
            Err(CHANGED)
        );
        assert_eq!(
            format_verdict("<a>\n", &ran(true, "<?xml version=\"1.0\"?>\n<a/>\n", "")),
            Err(UNREAD)
        );
        assert!(matches!(
            judged("<?xml version=\"1.0\"?>\n<a>\n"),
            Ok(format::Verdict::ToolFailed(detail)) if detail.contains("can't read as XML")
        ));
    }

    #[test]
    fn each_header_is_a_diagnostic_at_its_line_whatever_the_exit_status() {
        let stderr = "-:1: parser error : Opening and ending tag mismatch: b line 1 and a\n\
                      <a><b></a>\n          ^\n\
                      -:3: namespace error : Namespace prefix q on c is not defined\n\
                      <q:c/>\n    ^\n\
                      -:4: parser warning : Unsupported version '1.1'\n\
                      Bytes: 0xFF 0xFE\n";
        for success in [true, false] {
            let lint::Verdict::Diagnostics(found) =
                lint_verdict("", false, Ok(ran(success, "", stderr)))
            else {
                panic!("exit {success}: expected diagnostics");
            };
            let read: Vec<(usize, &str, bool, &str)> = found
                .iter()
                .map(|found| {
                    (
                        found.line,
                        found.rule.as_str(),
                        found.syntax,
                        found.message.as_str(),
                    )
                })
                .collect();
            assert_eq!(
                read,
                [
                    (
                        1,
                        "parser error",
                        true,
                        "Opening and ending tag mismatch: b line 1 and a"
                    ),
                    (
                        3,
                        "namespace error",
                        true,
                        "Namespace prefix q on c is not defined"
                    ),
                    (4, "parser warning", false, "Unsupported version '1.1'"),
                ],
                "exit {success}"
            );
        }
    }

    #[test]
    fn nothing_written_is_clean_only_on_success() {
        assert_eq!(
            lint_verdict("", false, Ok(ran(true, "", ""))),
            lint::Verdict::Clean
        );
        assert!(matches!(
            lint_verdict("", false, Ok(ran(false, "", ""))),
            lint::Verdict::Failed(format::Verdict::ToolFailed(_))
        ));
    }

    #[test]
    fn anything_on_stdout_is_a_tool_failure_even_with_diagnostics_or_success() {
        for stderr in ["", "-:1: parser error : Document is empty\n\n^\n"] {
            let verdict = lint_verdict("", false, Ok(ran(true, "<a/>\n", stderr)));
            assert!(
                matches!(
                    &verdict,
                    lint::Verdict::Failed(format::Verdict::ToolFailed(detail))
                        if detail.contains("stdout")
                ),
                "stderr {stderr:?}: got {verdict:?}"
            );
        }
    }

    #[test]
    fn a_stderr_line_before_any_header_is_a_tool_failure_whatever_follows() {
        let cases = [
            "warning: failed to load external entity \"a.dtd\"\n",
            "I/O error : Attempt to load network entity http://x/a.dtd\n-:1: parser error : x\n",
            "-:x: parser error : not a line number\n",
            "-:1: parser note : neither an error nor a warning\n",
            "-:1:parser error : no space\n",
        ];
        for stderr in cases {
            for success in [true, false] {
                let verdict = lint_verdict("", false, Ok(ran(success, "", stderr)));
                assert!(
                    matches!(
                        verdict,
                        lint::Verdict::Failed(format::Verdict::ToolFailed(_))
                    ),
                    "stderr {stderr:?}, exit {success}: got {verdict:?}"
                );
            }
        }
    }

    #[test]
    fn only_a_verified_minor_read_from_the_version_number_passes() {
        assert_eq!(
            verified(b"/usr/bin/xmllint: using libxml version 20913\n   compiled with: Threads\n"),
            Ok("2.9.13".to_owned())
        );
        assert_eq!(
            verified(b"xmllint: using libxml version 20914-GITv2.9.14\n"),
            Ok("2.9.14".to_owned())
        );
        assert_eq!(
            verified(b"xmllint: using libxml version 21502\n"),
            Ok("2.15.2".to_owned())
        );
        assert_eq!(
            verified(b"xmllint: using libxml version 21600\n"),
            Err("2.16.0".to_owned())
        );
        assert_eq!(
            verified(b"xmllint: using libxml version 21000\n"),
            Err("2.10.0".to_owned())
        );
        for unreadable in [
            &b""[..],
            b"xmllint: using libxml version \n",
            b"xmllint: using libxml version 2091\n",
            b"compiled with: Threads\nxmllint: using libxml version 20913\n",
            b"libxml 2.9.13\n",
        ] {
            assert_eq!(
                verified(unreadable),
                Err(UNREADABLE.to_owned()),
                "output {:?}",
                String::from_utf8_lossy(unreadable)
            );
        }
    }
}
