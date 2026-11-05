//! The reader of pylint's output: its `--output-format=json2` report, and the comments in a block
//! that switch pylint off for the whole block.
//!
//! Probed against pylint 4.1.3 (`ci/requirements-pylint.txt`): `printf 'x = 1\nif x == None:\n
//! pass\n' | pylint --from-stdin /p/doc.md.1.py --output-format=json2` prints
//!
//! ```text
//! {"messages": [{"type": "convention", "symbol": "invalid-name",
//!   "message": "Module name \"1\" doesn't conform to snake_case naming style",
//!   "messageId": "C0103", "confidence": "HIGH", "module": "doc.md.1", "obj": "",
//!   "line": 1, "column": 0, "endLine": null, "endColumn": null, "path": "doc.md.1.py",
//!   "absolutePath": "/p/doc.md.1.py"},
//!  {"type": "convention", "symbol": "singleton-comparison",
//!   "message": "Comparison 'x == None' should be 'x is None'", "messageId": "C0121", ...,
//!   "line": 2, "column": 3, "endLine": 2, "endColumn": 12, ...}],
//!  "statistics": {"messageTypeCount": {"fatal": 0, "error": 0, ...}, "modulesLinted": 3,
//!   "score": 2.5}}
//! ```
//!
//! and exits 16. Its lines are 1-based and its columns 0-based. A syntax error is the message
//! `syntax-error` (`E0001`), of type `error`.

use std::path::Path;

use crate::finding::{Finding, Location, Severity};

use super::format;
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

/// The code of the run's one warning about messages pylint reports on the project's config.
const NOT_CHECKED: &str = "code-block-not-checked";

/// The paths pylint gives a module it read from stdin under no name of its own.
const STDIN_PATHS: [&str; 3] = ["<stdin>", "-", "stdin"];

/// The `pylint:` comment keywords that take messages: each switches off no more than the
/// messages it names.
const MESSAGE_KEYWORDS: [&str; 5] = [
    "disable",
    "disable-next",
    "disable-msg",
    "enable",
    "enable-msg",
];

/// The `pylint:` comment words that switch every message off by themselves.
const ATOMIC_KEYWORDS: [&str; 2] = ["skip-file", "disable-all"];

/// The verdict on a run pylint failed in without reporting a message: a fatal or usage error.
const FAILED_SILENTLY: &str = "pylint exited with a fatal or usage error and reported no message";

/// The verdict on a run whose stdout isn't pylint's `json2` report.
const NOT_REPORT: &str = "its output wasn't its json2 report, so supdoc can't read what it found; \
                          something else printed to stdout, such as a `print` in a pylint plugin \
                          the project loads";

/// pylint's `--output-format=json2` report.
#[derive(serde::Deserialize)]
struct Report {
    /// Each message, in pylint's order.
    messages: Vec<Entry>,
    /// What the run covered.
    statistics: Statistics,
}

/// One message of pylint's report.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    /// `fatal`, `error`, `warning`, `refactor`, `convention` or `info`.
    #[serde(rename = "type")]
    kind: String,
    /// The rule's name, such as `syntax-error`.
    symbol: String,
    /// The rule's code, such as `E0001`.
    message_id: String,
    /// What pylint says.
    message: String,
    /// The file it is about, relative to where pylint ran (the root): the block's virtual path,
    /// or a config file such as `.pylintrc`.
    path: String,
    /// That file's absolute path.
    absolute_path: String,
    /// Its 1-based line.
    line: usize,
    /// Its 0-based column, counted in characters.
    column: usize,
    /// Its 1-based last line, when pylint says.
    end_line: Option<usize>,
}

/// What a run of pylint covered.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Statistics {
    /// How many modules it linted: none when the project's config left the block out.
    modules_linted: usize,
}

/// A message pylint reported on a file other than the block, such as the project's `.pylintrc`:
/// never one of the block's findings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ConfigNote {
    /// The file, relative to the root; only its name when it is outside the root.
    pub(crate) file: String,
    /// The message's code, such as `E0015`.
    pub(crate) id: String,
}

/// Where a run's block is, so its messages can be told from those about other files.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Block<'run> {
    /// The block's virtual path, as pylint was given it.
    pub(crate) path: &'run Path,
    /// The root pylint ran from.
    pub(crate) root: &'run Path,
}

impl Block<'_> {
    /// Whether `entry` is about the block: its path is the virtual path, or pylint's name for
    /// stdin.
    fn holds(&self, entry: &Entry) -> bool {
        STDIN_PATHS.contains(&entry.path.as_str())
            || Path::new(&entry.absolute_path) == self.path
            || self.root.join(&entry.path) == self.path
    }

    /// `entry`, about another file, as a note naming that file relative to the root.
    fn note(&self, entry: &Entry) -> ConfigNote {
        let path = Path::new(&entry.absolute_path);
        let real = self.root.canonicalize().ok();
        let inside = [Some(self.root), real.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|root| path.strip_prefix(root).ok());
        let file = inside.map_or_else(
            || {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            },
            |inside| inside.to_string_lossy().into_owned(),
        );
        ConfigNote {
            file,
            id: entry.message_id.clone(),
        }
    }
}

impl Entry {
    /// Whether it is about the module name pylint derives from the block's virtual path
    /// (`<doc>.<n>.py`, so a module named `1`), which supdoc chose and no author can change.
    fn names_the_virtual_module(&self) -> bool {
        self.symbol == "invalid-name" && self.message.starts_with("Module name \"")
    }

    /// It as a diagnostic: its column made 1-based (saturating, so a column past any line lands
    /// at the line's end), its end line its line when pylint gives none, and `syntax-error` the
    /// only syntax error.
    fn diagnostic(self) -> Diagnostic {
        Diagnostic {
            line: self.line,
            column: self.column.saturating_add(1),
            end_line: self.end_line.unwrap_or(self.line),
            syntax: self.symbol == "syntax-error",
            rule: self.message_id,
            message: self.message,
        }
    }
}

/// The report in `stdout`. `None` when it isn't pylint's `json2` report.
fn report(stdout: &[u8]) -> Option<Report> {
    serde_json::from_slice(stdout).ok()
}

/// Whether pylint's report in `stdout` says it linted no module, so the run says nothing about
/// the block.
#[must_use]
pub(crate) fn linted_nothing(stdout: &[u8]) -> bool {
    report(stdout).is_some_and(|report| report.statistics.modules_linted == 0)
}

/// Judges a pylint run on the block `block` names, adding each message about another file to
/// `notes`, never to the block's diagnostics. A non-zero exit with no message at all is a tool
/// failure: pylint exits non-zero without one only for a fatal or usage error (`Ran` holds no
/// exit code to read those bits from). Otherwise a `syntax-error` is a parse failure at its line,
/// as a formatter's is; a report with no diagnostics left is clean whatever the exit, since
/// pylint exits non-zero for the virtual module's name alone; a report with diagnostics gives
/// them. stdout that isn't the report, and a report holding a `fatal` message, are tool failures
/// whatever the exit (a project's `exit-zero` makes pylint exit 0 even then), quoting pylint's
/// stderr for a failed run that printed some. pylint checks no fragments, so no run is framed.
#[must_use]
pub(crate) fn judge(
    result: Result<Ran, Failure>,
    block: Option<Block<'_>>,
    notes: &mut Vec<ConfigNote>,
) -> lint::Verdict {
    let ran = match result {
        Ok(ran) => ran,
        Err(failure) => return lint::judge(Err(failure), |_| None),
    };
    let Some(report) = report(&ran.stdout) else {
        let detail = if ran.success || ran.stderr.iter().all(u8::is_ascii_whitespace) {
            NOT_REPORT.to_owned()
        } else {
            format::exit_detail(&ran.stderr)
        };
        return lint::Verdict::Failed(format::Verdict::ToolFailed(detail));
    };
    if report.messages.is_empty() && !ran.success {
        return lint::Verdict::Failed(format::Verdict::ToolFailed(FAILED_SILENTLY.to_owned()));
    }
    if let Some(fatal) = report
        .messages
        .iter()
        .find(|message| message.kind == "fatal")
    {
        return lint::Verdict::Failed(format::Verdict::ToolFailed(format!(
            "pylint reported a fatal error: {}",
            fatal.message
        )));
    }
    let mut found = Vec::new();
    for entry in report.messages {
        match block {
            Some(block) if !block.holds(&entry) => notes.push(block.note(&entry)),
            _ if entry.names_the_virtual_module() => {}
            _ => found.push(entry.diagnostic()),
        }
    }
    if let Some(syntax) = found.iter().find(|found| found.syntax) {
        return lint::Verdict::Failed(format::Verdict::ParseFailed {
            detail: syntax.message.clone(),
            line: syntax.line.checked_sub(1),
        });
    }
    if found.is_empty() {
        lint::Verdict::Clean
    } else {
        lint::Verdict::Diagnostics(found)
    }
}

/// Judges a pylint run as [`judge`] does, taking every message as the block's: the cartridge's
/// reader, for a caller that doesn't know the block's virtual path.
#[must_use]
pub(crate) fn lint_verdict(
    _input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    judge(result, None, &mut Vec::new())
}

/// The warning, at `at`, for the messages pylint reported on files other than the block, each
/// named once, in order. `None` when there are none. Its message is the same for every block
/// whose run gave the same notes, so the run gives it once.
#[must_use]
pub(crate) fn config_warning(notes: &[ConfigNote], at: &Location) -> Option<Finding> {
    let mut notes = notes.to_vec();
    notes.sort();
    notes.dedup();
    if notes.is_empty() {
        return None;
    }
    let named = notes
        .iter()
        .map(|note| format!("`{}` in `{}`", note.id, note.file))
        .collect::<Vec<_>>()
        .join(", ");
    Some(
        Finding::new(
            NOT_CHECKED,
            Severity::Warning,
            format!(
                "pylint reported {named}, about the project's pylint config rather than a block, \
                 so its Python blocks may not be checked as the config intends"
            ),
        )
        .with_help(
            "fix that file, which pylint reads from the root as its own command line would; \
             supdoc reports it once, not on each block",
        )
        .with_location(at.clone()),
    )
}

/// Whether `block` holds a pylint comment that may switch off more than single named messages,
/// so supdoc can't tell it from one that switches pylint off for the whole block: a `pylint:`
/// comment whose words are not all keywords that take messages, message codes (`C0103`) and
/// hyphenated message names (`unused-import`). `skip-file`, `disable-all`, `all`, a category
/// letter (`C`), a checker name and any unknown word all count. supdoc's own `lint(off: ...)` is
/// the way to switch the step off.
///
/// The scan is conservative by allowlist, so a near miss counts as switching the step off, never
/// as a pass: it reads text, not Python, so such a comment inside a string counts, as does one
/// scoped to a line or naming `enable`; a hyphenless name such as `fixme` counts too. It finds a
/// comment wherever a `#` starts it and reads up to a `;` or the next `#`, as pylint does.
#[must_use]
pub(crate) fn switches_lint_off(block: &str) -> bool {
    block.split('\n').any(|line| {
        line.match_indices('#').any(|(at, _)| {
            let comment = line.get(at + 1..).unwrap_or_default();
            pragmas(comment).any(|words| {
                words
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                    .filter(|word| !word.is_empty())
                    .any(|word| !names_messages(word))
            })
        })
    })
}

/// Whether `word`, in a `pylint:` comment, is a keyword that takes messages, a message code or a
/// hyphenated message name: none switches off more than the messages it names. A word holding
/// `skip-file` or `disable-all` anywhere, in any case, never is: pylint reads either at the start
/// of a word as the keyword, and supdoc reads it anywhere, so it can only lose coverage.
fn names_messages(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    if ATOMIC_KEYWORDS.iter().any(|atomic| lower.contains(atomic)) {
        return false;
    }
    MESSAGE_KEYWORDS.contains(&word) || is_message_id(word) || word.contains('-')
}

/// Whether `word` is a message code: a category letter and four digits, as `C0103`.
fn is_message_id(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| "CRWEFI".contains(c))
        && word.len() == 5
        && chars.all(|c| c.is_ascii_digit())
}

/// The text after each `pylint:` in `comment`, with any space before the colon, up to the next
/// `;` or `#`.
fn pragmas(comment: &str) -> impl Iterator<Item = &str> {
    comment
        .match_indices("pylint")
        .filter_map(move |(at, name)| {
            let rest = comment
                .get(at + name.len()..)?
                .trim_start()
                .strip_prefix(':')?;
            Some(rest.split([';', '#']).next().unwrap_or_default())
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        Block, ConfigNote, FAILED_SILENTLY, NOT_REPORT, config_warning, judge, lint_verdict,
        linted_nothing, switches_lint_off,
    };
    use crate::code_blocks::format;
    use crate::code_blocks::lint::{Diagnostic, Verdict};
    use crate::code_blocks::runner::Ran;
    use crate::finding::Location;

    /// The diagnostics in pylint's `json2` report in `stdout`, without the message about the
    /// virtual module's name; every message is taken as the block's. `None` when it isn't that
    /// report, or when the report holds a `fatal` message: pylint itself failed.
    fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
        let report = super::report(stdout)?;
        if report
            .messages
            .iter()
            .any(|message| message.kind == "fatal")
        {
            return None;
        }
        Some(
            report
                .messages
                .into_iter()
                .filter(|message| !message.names_the_virtual_module())
                .map(super::Entry::diagnostic)
                .collect(),
        )
    }

    /// A pylint `json2` report holding `messages` that linted `modules` modules.
    fn report(messages: &str, modules: usize) -> Vec<u8> {
        format!(
            r#"{{"messages": [{messages}], "statistics": {{"messageTypeCount": {{"fatal": 0}}, "modulesLinted": {modules}, "score": 0}}}}"#
        )
        .into_bytes()
    }

    /// A message as pylint 4.1.3 prints it.
    fn message(kind: &str, symbol: &str, id: &str, text: &str, at: (usize, usize)) -> String {
        format!(
            r#"{{"type": "{kind}", "symbol": "{symbol}", "message": "{text}", "messageId": "{id}", "confidence": "HIGH", "module": "doc.md.1", "obj": "", "line": {}, "column": {}, "endLine": null, "endColumn": null, "path": "doc.md.1.py", "absolutePath": "/p/doc.md.1.py"}}"#,
            at.0, at.1
        )
    }

    /// The message pylint gives every block about its virtual module's name.
    fn module_name() -> String {
        message(
            "convention",
            "invalid-name",
            "C0103",
            r#"Module name \"1\" doesn't conform to snake_case naming style"#,
            (1, 0),
        )
    }

    /// A finished run that exited `code` and printed `stdout`.
    fn ran(code: i32, stdout: Vec<u8>) -> Ran {
        Ran {
            success: code == 0,
            stdout,
            stderr: Vec::new(),
        }
    }

    #[test]
    fn each_message_is_a_diagnostic_with_its_column_made_one_based() {
        let stdout = report(
            &format!(
                r#"{{"type": "convention", "symbol": "singleton-comparison", "message": "Comparison 'x == None' should be 'x is None'", "messageId": "C0121", "confidence": "UNDEFINED", "module": "doc.md.1", "obj": "", "line": 3, "column": 3, "endLine": 4, "endColumn": 12, "path": "doc.md.1.py", "absolutePath": "/p/doc.md.1.py"}}, {}"#,
                message(
                    "convention",
                    "invalid-name",
                    "C0103",
                    r#"Constant name \"x\" doesn't conform to UPPER_CASE naming style"#,
                    (2, 0)
                )
            ),
            3,
        );

        assert_eq!(
            diagnostics(&stdout),
            Some(vec![
                Diagnostic {
                    line: 3,
                    column: 4,
                    end_line: 4,
                    syntax: false,
                    rule: "C0121".to_owned(),
                    message: "Comparison 'x == None' should be 'x is None'".to_owned(),
                },
                Diagnostic {
                    line: 2,
                    column: 1,
                    end_line: 2,
                    syntax: false,
                    rule: "C0103".to_owned(),
                    message: "Constant name \"x\" doesn't conform to UPPER_CASE naming style"
                        .to_owned(),
                },
            ]),
            "a constant's invalid name is kept; only the module's is dropped"
        );
    }

    #[test]
    fn a_syntax_error_is_the_only_diagnostic_read_as_one() {
        let stdout = report(
            &message(
                "error",
                "syntax-error",
                "E0001",
                "Parsing failed: 'invalid syntax (doc.md.1, line 1)'",
                (1, 7),
            ),
            3,
        );

        let found = diagnostics(&stdout).expect("the report should be read");

        let [syntax] = found.as_slice() else {
            panic!("one diagnostic should be read: {found:?}");
        };
        assert!(syntax.syntax, "{found:?}");
        assert_eq!((syntax.line, syntax.column), (1, 8), "{found:?}");
        assert_eq!(
            lint_verdict("", false, Ok(ran(2, stdout))),
            Verdict::Failed(format::Verdict::ParseFailed {
                detail: "Parsing failed: 'invalid syntax (doc.md.1, line 1)'".to_owned(),
                line: Some(0),
            }),
            "a syntax error is a parse failure at its 0-based body line"
        );
    }

    #[test]
    fn the_virtual_module_name_alone_is_clean_though_pylint_exits_non_zero() {
        let stdout = report(&module_name(), 3);

        assert_eq!(diagnostics(&stdout), Some(Vec::new()));
        assert_eq!(lint_verdict("", false, Ok(ran(16, stdout))), Verdict::Clean);
    }

    #[test]
    fn a_diagnostic_after_a_non_zero_exit_is_reported() {
        let stdout = report(
            &message(
                "warning",
                "unused-argument",
                "W0613",
                "Unused argument 'a'",
                (1, 6),
            ),
            3,
        );

        let verdict = lint_verdict("", false, Ok(ran(4, stdout)));

        assert!(
            matches!(&verdict, Verdict::Diagnostics(found) if found.first().is_some_and(|found| found.rule == "W0613")),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_fatal_message_or_output_that_isnt_the_report_is_a_tool_failure() {
        let fatal = report(
            &message("fatal", "astroid-error", "F0002", "astroid crashed", (1, 0)),
            3,
        );

        assert_eq!(diagnostics(&fatal), None);
        assert_eq!(diagnostics(b"usage: pylint"), None);
        assert!(
            matches!(
                lint_verdict("", false, Ok(ran(1, fatal))),
                Verdict::Failed(_)
            ),
            "a fatal message means pylint failed"
        );
    }

    #[test]
    fn a_report_that_linted_no_module_linted_nothing() {
        assert!(linted_nothing(&report("", 0)));
        assert!(!linted_nothing(&report(&module_name(), 3)));
        assert!(
            !linted_nothing(b"not json"),
            "unreadable output is judged, not skipped"
        );
    }

    #[test]
    fn a_whole_block_disable_switches_lint_off() {
        for block in [
            "# pylint: skip-file\nx = 1\n",
            "x = 1  # pylint: disable=all\n",
            "#pylint:disable=all",
            "# pylint : disable = unused-import, ALL\n",
            "# pylint: disable-all\n",
            "# noqa # pylint: skip-file\n",
            "s = '# pylint: disable=all'\n",
            "# pylint: disable=C,R,W,E,F\n",
            "x = 1  # pylint: disable=W\n",
            "# pylint: disable=typecheck\n",
            "# pylint: disable=fixme\n",
            "# pylint: disable-next=all\n",
            "# pylint: disable=unused-import; then # pylint: disable=all\n",
            "x = 1  # pylint: skip-file-x\n",
            "# pylint: skip-file-\n",
            "# pylint: disable-all-x\n",
            "# pylint: skip-file-unused-import\n",
            "# pylint: disable=unused-import,skip-file-x\n",
            "# pylint: disable=xskip-filex\n",
            "# pylint: disable=SKIP-FILE-x\n",
        ] {
            assert!(switches_lint_off(block), "{block:?} should switch lint off");
        }
    }

    #[test]
    fn a_rule_disable_or_a_mention_of_pylint_leaves_lint_on() {
        for block in [
            "x = 1  # pylint: disable=invalid-name\n",
            "import os  # pylint: disable=unused-import\n",
            "x = 1  # pylint: disable=C0103, W0611\n",
            "x = 1  # pylint: disable=unused-import; supdoc reads up to the semicolon\n",
            "# run pylint on all files\n",
            "# pylint disable=all\n",
            "all_files = 1  # pylint: disable=unused-variable\n",
            "x = 1\n",
        ] {
            assert!(!switches_lint_off(block), "{block:?} should leave lint on");
        }
    }

    /// A message about `path` (absolute `absolute`), as pylint 4.1.3 prints one about a config
    /// file.
    fn message_on(id: &str, path: &str, absolute: &str) -> String {
        format!(
            r#"{{"type": "error", "symbol": "unrecognized-option", "message": "Unrecognized option found: bogus", "messageId": "{id}", "confidence": "UNDEFINED", "module": "{absolute}", "obj": "", "line": 1, "column": 0, "endLine": null, "endColumn": null, "path": "{path}", "absolutePath": "{absolute}"}}"#
        )
    }

    /// The block `/p/doc.md.1.py` of the root `/p`.
    fn block() -> Block<'static> {
        Block {
            path: Path::new("/p/doc.md.1.py"),
            root: Path::new("/p"),
        }
    }

    #[test]
    fn a_message_about_a_config_file_is_a_note_never_a_block_diagnostic() {
        let stdout = report(
            &format!(
                "{}, {}",
                message_on("E0015", ".pylintrc", "/p/.pylintrc"),
                message(
                    "warning",
                    "unused-argument",
                    "W0613",
                    "Unused argument 'a'",
                    (1, 6)
                )
            ),
            3,
        );
        let mut notes = Vec::new();

        let verdict = judge(Ok(ran(6, stdout)), Some(block()), &mut notes);

        assert!(
            matches!(&verdict, Verdict::Diagnostics(found) if found.len() == 1 && found.first().is_some_and(|found| found.rule == "W0613")),
            "only the block's message is its diagnostic: {verdict:?}"
        );
        assert_eq!(
            notes,
            [ConfigNote {
                file: ".pylintrc".to_owned(),
                id: "E0015".to_owned(),
            }]
        );
    }

    #[test]
    fn a_config_message_alone_leaves_the_block_clean() {
        let stdout = report(&message_on("E0015", ".pylintrc", "/p/.pylintrc"), 3);
        let mut notes = Vec::new();

        assert_eq!(
            judge(Ok(ran(2, stdout)), Some(block()), &mut notes),
            Verdict::Clean
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn the_config_warning_names_each_file_and_code_once() {
        let note = ConfigNote {
            file: ".pylintrc".to_owned(),
            id: "E0015".to_owned(),
        };
        let at = Location {
            file: "docs/a.md"
                .parse()
                .expect("the fixture path should be a source path"),
            range: 0..0,
            line: 3,
            column: 1,
        };

        let warning = config_warning(&[note.clone(), note], &at).expect("a note gives a warning");

        assert_eq!(warning.code, "code-block-not-checked");
        assert_eq!(
            warning.message.matches("`E0015` in `.pylintrc`").count(),
            1,
            "{}",
            warning.message
        );
        assert_eq!(config_warning(&[], &at), None);
    }

    #[test]
    fn a_column_past_any_line_reads_without_overflow() {
        let stdout = report(
            &message(
                "convention",
                "line-too-long",
                "C0301",
                "Line too long",
                (1, usize::MAX),
            ),
            3,
        );

        let found = diagnostics(&stdout).expect("the report should be read");

        assert_eq!(found.first().map(|found| found.column), Some(usize::MAX));
    }

    #[test]
    fn stdout_that_isnt_the_report_fails_whatever_the_exit() {
        let report = String::from_utf8(report(
            &message(
                "warning",
                "unused-argument",
                "W0613",
                "Unused argument 'a'",
                (1, 6),
            ),
            3,
        ))
        .expect("the report is UTF-8");
        for stdout in [
            format!("plugin loaded\n{report}"),
            "not json".to_owned(),
            "{}".to_owned(),
            r#"{"messages": []}"#.to_owned(),
            format!("{}{}", "[".repeat(100_000), "]".repeat(100_000)),
        ] {
            for code in [0, 4] {
                assert_eq!(
                    lint_verdict("", false, Ok(ran(code, stdout.clone().into_bytes()))),
                    Verdict::Failed(format::Verdict::ToolFailed(NOT_REPORT.to_owned())),
                    "exit {code}: {}",
                    stdout.get(..40).unwrap_or(&stdout)
                );
            }
        }
        let failed = Ran {
            success: false,
            stdout: Vec::new(),
            stderr: b"usage: pylint [options]".to_vec(),
        };
        assert_eq!(
            lint_verdict("", false, Ok(failed)),
            Verdict::Failed(format::Verdict::ToolFailed(
                "usage: pylint [options]".to_owned()
            )),
            "a failed run's stderr is quoted"
        );
    }

    #[test]
    fn a_fatal_message_fails_even_when_pylint_exits_zero() {
        let fatal = report(
            &message("fatal", "astroid-error", "F0002", "astroid crashed", (1, 0)),
            3,
        );

        assert_eq!(
            lint_verdict("", false, Ok(ran(0, fatal))),
            Verdict::Failed(format::Verdict::ToolFailed(
                "pylint reported a fatal error: astroid crashed".to_owned()
            ))
        );
    }

    #[test]
    fn a_non_zero_exit_with_no_message_is_a_tool_failure() {
        for code in [1, 32] {
            assert_eq!(
                lint_verdict("", false, Ok(ran(code, report("", 1)))),
                Verdict::Failed(format::Verdict::ToolFailed(FAILED_SILENTLY.to_owned())),
                "exit {code}"
            );
        }
        assert_eq!(
            lint_verdict("", false, Ok(ran(0, report("", 1)))),
            Verdict::Clean
        );
    }
}
