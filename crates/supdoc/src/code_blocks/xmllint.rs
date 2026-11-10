//! xmllint: its diagnostics, its version, and the blocks supdoc never gives it.
//!
//! xmllint (libxml2) reads the block on stdin and checks only that it is well-formed XML with
//! well-formed namespaces: it always gets exactly [`ARGS`], so it builds no output tree, uses no
//! network, reads no catalog, and never loads or expands an external DTD or entity (no
//! `--loaddtd`, `--valid`, `--noent`, `--dtdattr`, `--xinclude`, `--c14n*` or `--huge`). It reads
//! no config file, and its variables (`XML_CATALOG_FILES`, `XMLLINT_INDENT` and the rest) never
//! reach it (see [`super::tools::ToolEnv`]).
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
//!
//! supdoc reads xmllint's stderr, never its exit status alone: a namespace error exits 0.

use std::path::Path;

use super::format;
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;

/// Every argument xmllint gets, in order: no output tree, no network, no catalog, the block on
/// stdin.
pub(crate) const ARGS: [&str; 4] = ["--noout", "--nonet", "--nocatalogs", "-"];

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
