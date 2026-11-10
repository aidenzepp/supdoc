//! A custom `{ command = [...] }` formatter or linter: the command the config wrote, run on a
//! block in a trusted checkout.
//!
//! The command runs as written, with no shell: `{path}` in any argument becomes the block's
//! virtual path, spelled relative to the root, and the block arrives on stdin exactly as written.
//! It runs from the root's real path with [`ToolEnv`]'s variables and `NO_COLOR=1`, under the
//! block timeout and output cap every tool gets. Its results are never cached: supdoc can't name
//! the files a command reads.
//!
//! A linter's exit 0 is clean, whatever it printed; any other exit fails the block. With
//! `diagnostics = "gnu"`, each `PATH:LINE[:COL]: MESSAGE` line it printed for the block is one
//! `code-block-lint` at its doc line. Otherwise, or when no line reads so, the block gets one
//! `code-block-lint` at its fence quoting the first lines it printed.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::config::code_blocks::{Diagnostics, Language};
use crate::finding::{Finding, Location, Severity};
use crate::path::Pointer;

use super::block::CodeBlock;
use super::format::{self, Verdict};
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};
use super::rust::LineMap;
use super::tools::{self, CustomTool, Found, Step, ToolEnv};

/// The variables a custom command gets beyond [`ToolEnv`]'s: color off, so its output quotes as
/// plain text.
pub(crate) const VARS: [(&str, &str); 1] = [("NO_COLOR", "1")];

/// The names a GNU line may give the block's file besides its virtual path: one block runs at a
/// time, so any name for stdin is the block.
const STDIN_NAMES: [&str; 3] = ["<stdin>", "-", "stdin"];

/// The most lines of a custom linter's output a fallback finding quotes.
const QUOTED_LINES: usize = 20;

/// The extension of the virtual file a block tagged `tag`, in `language`, is named by: the tag
/// itself where it names a flavor (`tsx`, `mts`, `cts`, `jsx`, `mjs`, `cjs`, `jsonc`, `scss`), else the
/// language's own.
#[must_use]
pub(crate) fn extension(language: Language, tag: &str) -> &'static str {
    match (language, tag) {
        (Language::TypeScript, "tsx") => "tsx",
        (Language::TypeScript, "mts") => "mts",
        (Language::TypeScript, "cts") => "cts",
        (Language::TypeScript, _) => "ts",
        (Language::JavaScript, "jsx") => "jsx",
        (Language::JavaScript, "mjs") => "mjs",
        (Language::JavaScript, "cjs") => "cjs",
        (Language::JavaScript, _) => "js",
        (Language::Json, "jsonc") => "jsonc",
        (Language::Json, _) => "json",
        (Language::Css, "scss") => "scss",
        (Language::Css, _) => "css",
        (Language::Html, _) => "html",
        (Language::Rust, _) => "rs",
        (Language::Shell, _) => "sh",
        (Language::Python, _) => "py",
        (Language::Yaml, _) => "yaml",
        (Language::Toml, _) => "toml",
        (Language::Dockerfile, _) => "dockerfile",
        (Language::Makefile, _) => "mk",
        (Language::Sql, _) => "sql",
        (Language::Markdown, _) => "md",
        (Language::Go, _) => "go",
    }
}

/// The `index`th block (1-based) of `doc`, the doc's root-relative path, as a custom command
/// names it: `<doc>.<index>.<extension>`, beside the doc, relative to the root it runs from. A
/// path that would start with `-` is written `./<path>`, so it never reads as an option.
#[must_use]
pub(crate) fn virtual_path(doc: &str, index: usize, extension: &str) -> String {
    let dot = if doc.starts_with('-') { "./" } else { "" };
    format!("{dot}{doc}.{index}.{extension}")
}

/// `arguments` as the command gets them: each `{path}` in any of them replaced by `path`.
#[must_use]
pub(crate) fn args(arguments: &[String], path: &str) -> Vec<OsString> {
    arguments
        .iter()
        .map(|argument| argument.replace("{path}", path).into())
        .collect()
}

/// Whether `program` names a file by path rather than a tool to look up on `PATH`: it holds a
/// `/`, or, when `backslash` (Windows), a `\\`.
fn names_a_path(program: &str, backslash: bool) -> bool {
    program.contains('/') || (backslash && program.contains('\\'))
}

/// Where `program`, a custom command's first element, is: looked up on `env`'s `PATH` when it
/// holds no path separator (`/`, and on Windows `\\`), else the file it names, relative to `root` unless absolute. Either may be
/// inside the root, which a trusted checkout allows.
///
/// Effect: reads file metadata along `PATH` or at the named file, and resolves symlinks.
#[must_use]
pub(crate) fn find(program: &str, env: &ToolEnv, root: &Path) -> Found {
    if !names_a_path(program, cfg!(windows)) {
        return tools::find(program, env, root);
    }
    let Some(named) = Pointer::new(program).map(|pointer| pointer.resolve_from(root)) else {
        return Found::Missing;
    };
    let Some(real) = tools::is_executable_file(&named)
        .then(|| named.canonicalize().ok())
        .flatten()
    else {
        return Found::Missing;
    };
    if tools::inside(&real, root) {
        Found::InsideRoot(real)
    } else {
        Found::OnPath(real)
    }
}

/// Judges a custom formatter's `result` on a block whose body as written is `input`: its output
/// after exit 0 is read as every formatter's is ([`format::printed`]). A non-zero exit is a tool
/// failure quoting the first lines of its stderr, ANSI escapes removed: a custom formatter can't
/// say a block doesn't parse. A timeout is a timeout.
#[must_use]
pub(crate) fn judge(input: &str, result: Result<Ran, Failure>) -> Verdict {
    match result {
        Ok(ran) if ran.success => format::printed(input, ran.stdout),
        Ok(ran) => {
            let stderr = format::strip_ansi(&String::from_utf8_lossy(&ran.stderr));
            Verdict::ToolFailed(format::exit_detail(stderr.as_bytes()))
        }
        Err(failure) => format::failed(failure),
    }
}

/// How messages name `tool`: "the custom formatter command `<program>`".
#[must_use]
pub(crate) fn named(tool: &CustomTool) -> String {
    format!("the {}", tool.shown())
}

/// The config key that selects `tool`, for help: "`formatter` in
/// `[features.code-blocks.python]`".
fn key(tool: &CustomTool) -> String {
    let key = match tool.step() {
        Step::Format => "formatter",
        Step::Lint => "linter",
    };
    format!(
        "`{key}` in `[features.code-blocks.{}]`",
        tool.language().table()
    )
}

/// The block attribute that turns `tool`'s step off: `format` or `lint`.
const fn attribute(tool: &CustomTool) -> &'static str {
    match tool.step() {
        Step::Format => "format",
        Step::Lint => "lint",
    }
}

/// The finding for `verdict`, from `tool` on the block at `at`, run under `timeout`. `None` when
/// the block is formatted or the formatter printed nothing, and for a verdict [`judge`] never
/// gives.
#[must_use]
pub(crate) fn finding(
    verdict: &Verdict,
    tool: &CustomTool,
    at: &Location,
    timeout: Option<Duration>,
) -> Option<Finding> {
    let named = named(tool);
    let finding = match verdict {
        Verdict::Unformatted(_) => Finding::new(
            format::UNFORMATTED,
            Severity::Error,
            format!("the block isn't formatted as {named} formats it"),
        )
        .with_help(format!(
            "replace the block's body with what {named} prints for it"
        )),
        Verdict::ToolFailed(detail) => Finding::new(
            format::TOOL_FAILED,
            Severity::Error,
            format!("{named} failed on the block: {detail}"),
        )
        .with_help(format!(
            "fix what it reports, change {}, or mark the block `{}(off: <reason>)`",
            key(tool),
            attribute(tool)
        )),
        Verdict::TimedOut => Finding::new(
            format::TOOL_TIMEOUT,
            Severity::Error,
            format!(
                "{named} ran longer than {} on the block",
                timeout.map_or_else(|| "its limit".to_owned(), |limit| format!("{limit:?}"))
            ),
        )
        .with_help(format!(
            "raise the limit with `--code-block-timeout 30s`, or mark the block `{}(off: <reason>)`",
            attribute(tool)
        )),
        Verdict::Formatted
        | Verdict::PrintedNothing
        | Verdict::ParseFailed { .. }
        | Verdict::Refused { .. }
        | Verdict::NotKind { .. }
        | Verdict::FrameReformatted(_)
        | Verdict::Uncompared(_) => return None,
    };
    Some(finding.with_location(at.clone()))
}

/// The names a custom linter's GNU line may give a block whose root-relative virtual path is
/// `path`, besides stdin's: `path` itself, and its absolute spelling under `root` and under the
/// root's real path, where the command runs.
///
/// Effect: resolves `root`'s symlinks.
#[must_use]
pub(crate) fn names(path: &str, root: &Path) -> Vec<String> {
    let relative = path.strip_prefix("./").unwrap_or(path);
    let real = root.canonicalize().ok();
    let mut names = vec![path.to_owned()];
    for root in [Some(root), real.as_deref()].into_iter().flatten() {
        let Some(absolute) =
            Pointer::new(relative).map(|pointer| pointer.resolve_from(root).display().to_string())
        else {
            continue;
        };
        if !names.contains(&absolute) {
            names.push(absolute);
        }
    }
    names
}

/// Judges a custom linter's `result` on a block of `lines` lines whose file it may name as any
/// of `names` or stdin: exit 0 is clean, whatever it printed. A non-zero exit gives the
/// diagnostics `diagnostics` reads from its stdout then its stderr, ANSI escapes removed; with
/// none, or no `diagnostics`, its first [`QUOTED_LINES`] lines of output that aren't blank,
/// stdout then stderr, or that it printed nothing when every line of both is blank. A
/// failed start, output past the cap and a timeout are as for every tool.
#[must_use]
pub(crate) fn lint_verdict(
    result: Result<Ran, Failure>,
    diagnostics: Option<Diagnostics>,
    names: &[String],
    lines: usize,
) -> lint::Verdict {
    let ran = match result {
        Ok(ran) if ran.success => return lint::Verdict::Clean,
        Ok(ran) => ran,
        Err(failure) => return lint::Verdict::Failed(format::failed(failure)),
    };
    let stdout = format::strip_ansi(&String::from_utf8_lossy(&ran.stdout));
    let stderr = format::strip_ansi(&String::from_utf8_lossy(&ran.stderr));
    let output = || stdout.lines().chain(stderr.lines());
    let found: Vec<Diagnostic> = match diagnostics {
        Some(Diagnostics::Gnu) => output()
            .filter_map(|line| gnu(line, names, lines))
            .collect(),
        None => Vec::new(),
    };
    if !found.is_empty() {
        return lint::Verdict::Diagnostics(found);
    }
    let printed: Vec<&str> = output()
        .filter(|line| !line.trim().is_empty())
        .take(QUOTED_LINES)
        .collect();
    lint::Verdict::Unparsed((!printed.is_empty()).then(|| printed.join("\n")))
}

/// The diagnostic `line` gives when it reads as `PATH:LINE[:COL]: MESSAGE` for a block of
/// `lines` lines: `PATH` one of `names` or a name for stdin, `LINE` one of the block's (1-based),
/// `COL` 1-based, and a message that isn't blank. `None` for any other line.
fn gnu(line: &str, names: &[String], lines: usize) -> Option<Diagnostic> {
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse::<usize>().ok())
            .flatten()
    };
    let rest = names
        .iter()
        .map(String::as_str)
        .chain(STDIN_NAMES)
        .find_map(|name| line.strip_prefix(name)?.strip_prefix(':'))?;
    let (at, rest) = rest.split_once(':')?;
    let at = number(at).filter(|at| (1..=lines).contains(at))?;
    let (column, rest) = match rest.split_once(':') {
        Some((column, after)) if let Some(column) = number(column) => (column, after),
        _ => (1, rest),
    };
    let message = rest.strip_prefix(' ')?.trim();
    (!message.is_empty()).then(|| Diagnostic {
        line: at,
        column,
        end_line: at,
        syntax: false,
        rule: String::new(),
        message: message.to_owned(),
    })
}

/// The findings for a custom linter's `verdict` from `tool` on `block`, in the doc whose text is
/// `text`, run under `timeout`: one `code-block-lint` error per diagnostic at its doc line and
/// column (the fence when it can't be placed), or one at the fence quoting what the linter
/// printed, or saying it printed nothing; a tool failure or timeout as a formatter's. Never a
/// fix.
#[must_use]
pub(crate) fn lint_findings(
    verdict: &lint::Verdict,
    tool: &CustomTool,
    block: &CodeBlock,
    text: &str,
    timeout: Option<Duration>,
) -> Vec<Finding> {
    let named = named(tool);
    let off = "or mark the block `lint(off: <reason>)` to skip its lint step";
    match verdict {
        lint::Verdict::Clean => Vec::new(),
        lint::Verdict::Failed(failed) => finding(failed, tool, &block.fence, timeout)
            .into_iter()
            .collect(),
        lint::Verdict::Diagnostics(found) => {
            let map = LineMap::of(block, text);
            found
                .iter()
                .map(|diagnostic| {
                    let at = map
                        .as_ref()
                        .and_then(|map| lint::place(diagnostic, map, text))
                        .unwrap_or_else(|| block.fence.clone());
                    Finding::new(
                        lint::LINT,
                        Severity::Error,
                        format!("{named} reports: {}", diagnostic.message),
                    )
                    .with_help(format!("fix what it reports, {off}"))
                    .with_location(at)
                })
                .collect()
        }
        lint::Verdict::Unparsed(quoted) => {
            let gnu = tool.command().diagnostics().is_some();
            let message = match quoted {
                Some(quoted) if gnu => format!(
                    "{named} failed the block, and no line it printed reads as \
                     `PATH:LINE[:COL]: MESSAGE` for one of the block's lines:\n{quoted}"
                ),
                Some(quoted) => format!("{named} failed the block:\n{quoted}"),
                None => format!(
                    "{named} exited with a non-zero status on the block and printed nothing"
                ),
            };
            let help = if gnu {
                format!("fix what it reports, {off}")
            } else {
                format!(
                    "fix what it reports, add `diagnostics = \"gnu\"` to {} when it prints \
                     `PATH:LINE[:COL]: MESSAGE` lines, {off}",
                    key(tool)
                )
            };
            vec![
                Finding::new(lint::LINT, Severity::Error, message)
                    .with_help(help)
                    .with_location(block.fence.clone()),
            ]
        }
    }
}

/// The run's one `code-block-tool-missing` warning, with `code`, at `at`, for `tool` whose program
/// [`find`] didn't find, naming the program and the config key that chose it.
#[must_use]
pub(crate) fn missing(code: &'static str, tool: &CustomTool, at: &Location) -> Finding {
    let program = tool.command().program();
    let shown = program.escape_debug();
    let help = if names_a_path(program, cfg!(windows)) {
        format!("make `{shown}` an executable file")
    } else {
        format!("put `{shown}` on PATH")
    };
    Finding::new(
        code,
        Severity::Warning,
        format!(
            "{} for {} {}, so its blocks aren't checked",
            named(tool),
            key(tool),
            tool.missing()
        ),
    )
    .with_help(format!("{help}, or change {}", key(tool)))
    .with_location(at.clone())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        Found, Verdict, args, extension, find, judge, lint_verdict, names, names_a_path,
        virtual_path,
    };
    use crate::code_blocks::lint::{self, Diagnostic};
    use crate::code_blocks::runner::{Failure, Ran};
    use crate::code_blocks::tools::ToolEnv;
    use crate::config::code_blocks::{Diagnostics, Language};

    /// A run that exited as `success` says, printing `stdout` and `stderr`.
    fn ran(success: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_program_is_a_path_by_slash_always_and_by_backslash_only_on_windows() {
        assert!(names_a_path("bin/x", false));
        assert!(names_a_path("bin/x", true));
        assert!(!names_a_path("..\\x", false));
        assert!(names_a_path("..\\x", true));
        assert!(!names_a_path("x", true));
    }

    #[test]
    fn each_language_names_its_virtual_file_by_extension_and_flavor_tag() {
        let cases = [
            (Language::Rust, "rust", "rs"),
            (Language::Shell, "bash", "sh"),
            (Language::Python, "py", "py"),
            (Language::TypeScript, "typescript", "ts"),
            (Language::TypeScript, "tsx", "tsx"),
            (Language::TypeScript, "mts", "mts"),
            (Language::TypeScript, "cts", "cts"),
            (Language::JavaScript, "javascript", "js"),
            (Language::JavaScript, "jsx", "jsx"),
            (Language::JavaScript, "mjs", "mjs"),
            (Language::JavaScript, "cjs", "cjs"),
            (Language::Json, "json", "json"),
            (Language::Json, "jsonc", "jsonc"),
            (Language::Css, "css", "css"),
            (Language::Css, "scss", "scss"),
            (Language::Html, "htm", "html"),
            (Language::Yaml, "yml", "yaml"),
            (Language::Toml, "toml", "toml"),
            (Language::Dockerfile, "Dockerfile", "dockerfile"),
            (Language::Dockerfile, "containerfile", "dockerfile"),
            (Language::Makefile, "Makefile", "mk"),
            (Language::Sql, "pgsql", "sql"),
            (Language::Markdown, "markdown", "md"),
            (Language::Go, "golang", "go"),
        ];

        for (language, tag, expected) in cases {
            assert_eq!(
                extension(language, tag),
                expected,
                "a `{tag}` block should be named `.{expected}`"
            );
        }
    }

    #[test]
    fn path_is_replaced_in_every_argument_and_nothing_else_changes() {
        let written = ["--stdin-filename", "{path}", "--x={path}", "{other}"].map(str::to_owned);

        let given = args(&written, &virtual_path("docs/guide.md", 3, "py"));

        assert_eq!(
            given,
            [
                "--stdin-filename",
                "docs/guide.md.3.py",
                "--x=docs/guide.md.3.py",
                "{other}"
            ]
            .map(std::ffi::OsString::from),
            "only `{{path}}` should be replaced, by the root-relative virtual path"
        );
    }

    #[test]
    fn the_formatters_output_decides_the_verdict() {
        let block = "x = 1\n";
        let cases = [
            (Ok(ran(true, block, "")), Verdict::Formatted),
            (
                Ok(ran(true, "x=1\n", "")),
                Verdict::Unformatted("x=1\n".to_owned()),
            ),
            (Ok(ran(true, "x = 1", "")), Verdict::Formatted),
            (Ok(ran(true, "x = 1\r\n", "")), Verdict::Formatted),
            (
                Ok(ran(true, "x = 1\r\ny = 2\r\n", "")),
                Verdict::Unformatted("x = 1\ny = 2\n".to_owned()),
            ),
            (Ok(ran(true, " \n\t\n", "")), Verdict::PrintedNothing),
            (Err(Failure::TimedOut), Verdict::TimedOut),
        ];

        for (result, expected) in cases {
            assert_eq!(judge(block, result), expected);
        }
    }

    #[test]
    fn a_virtual_path_that_would_read_as_an_option_starts_with_dot_slash() {
        assert_eq!(
            (
                virtual_path("-x.md", 1, "py"),
                virtual_path("docs/-x.md", 2, "py")
            ),
            ("./-x.md.1.py".to_owned(), "docs/-x.md.2.py".to_owned()),
            "only a path that starts with `-` needs `./`"
        );
    }

    #[test]
    fn a_non_zero_exit_quotes_five_stderr_lines_without_ansi() {
        let stderr = "\u{1b}[31merror\u{1b}[0m: one\ntwo\nthree\nfour\nfive\nsix\n";

        let verdict = judge("x = 1\n", Ok(ran(false, "", stderr)));

        assert_eq!(
            verdict,
            Verdict::ToolFailed("error: one\ntwo\nthree\nfour\nfive".to_owned()),
            "a failed custom formatter should quote at most five plain lines"
        );
    }

    #[test]
    fn a_program_with_a_slash_is_the_file_it_names_under_the_root() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let scripts = root.path().join("scripts");
        std::fs::create_dir(&scripts).expect("the scripts dir should be creatable");
        let script = scripts.join("fmt.sh");
        std::fs::write(&script, "#!/bin/sh\ncat\n").expect("the script should be writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("the script should be made executable");
        }
        let env = ToolEnv::from_vars(|_| None);

        let found = find("scripts/fmt.sh", &env, root.path());
        let missing = find("scripts/none.sh", &env, root.path());
        let bare = find("supdoc-no-such-program", &env, Path::new("/"));

        assert_eq!(
            (found, missing, bare),
            (
                Found::InsideRoot(script.canonicalize().expect("the script should resolve")),
                Found::Missing,
                Found::Missing
            ),
            "a path resolves under the root, and a bare name is looked up on PATH"
        );
    }

    /// The names a GNU line may give `docs/a.md.1.py` run from `/repo`.
    fn block_names() -> Vec<String> {
        vec![
            "docs/a.md.1.py".to_owned(),
            "/repo/docs/a.md.1.py".to_owned(),
        ]
    }

    /// The verdict on a GNU linter that failed printing `stdout` and `stderr`, on a block of
    /// three lines.
    fn gnu_verdict(stdout: &str, stderr: &str) -> lint::Verdict {
        lint_verdict(
            Ok(ran(false, stdout, stderr)),
            Some(Diagnostics::Gnu),
            &block_names(),
            3,
        )
    }

    /// A diagnostic at `line`, `column`, saying `message`.
    fn said(line: usize, column: usize, message: &str) -> Diagnostic {
        Diagnostic {
            line,
            column,
            end_line: line,
            syntax: false,
            rule: String::new(),
            message: message.to_owned(),
        }
    }

    #[test]
    fn a_gnu_line_naming_the_block_is_a_diagnostic_at_its_line_and_column() {
        let cases = [
            ("docs/a.md.1.py:2:5: error: x", said(2, 5, "error: x")),
            (
                "/repo/docs/a.md.1.py:3:1: warning: y",
                said(3, 1, "warning: y"),
            ),
            ("<stdin>:1:1: error: x", said(1, 1, "error: x")),
            ("-:2:8: note: z", said(2, 8, "note: z")),
            ("stdin:1:2: E1 bad", said(1, 2, "E1 bad")),
            ("docs/a.md.1.py:2: no column", said(2, 1, "no column")),
            (
                "\u{1b}[1mdocs/a.md.1.py\u{1b}[0m:2:5: colored",
                said(2, 5, "colored"),
            ),
        ];

        for (line, expected) in cases {
            assert_eq!(
                gnu_verdict(line, ""),
                lint::Verdict::Diagnostics(vec![expected]),
                "`{line}` should read as one diagnostic"
            );
        }
    }

    #[test]
    fn a_line_off_the_block_or_for_another_file_or_malformed_isnt_a_diagnostic() {
        let lines = [
            "docs/a.md.1.py:4:1: past the last line",
            "docs/a.md.1.py:0:1: line zero",
            "docs/b.md.1.py:1:1: another block",
            "other.py:1:1: another file",
            "docs/a.md.1.py:1:1:no space",
            "docs/a.md.1.py:1:1: ",
            "docs/a.md.1.py:x:1: not a number",
            "docs/a.md.1.py 1:1: no colon",
            "Found 1 error.",
        ];

        for line in lines {
            assert_eq!(
                gnu_verdict(line, ""),
                lint::Verdict::Unparsed(Some(line.to_owned())),
                "`{line}` shouldn't read as a diagnostic, so it is quoted"
            );
        }
    }

    #[test]
    fn gnu_lines_are_read_from_stdout_then_stderr() {
        assert_eq!(
            gnu_verdict("noise\n-:2:1: out\n", "<stdin>:1:1: err\n"),
            lint::Verdict::Diagnostics(vec![said(2, 1, "out"), said(1, 1, "err")]),
        );
    }

    #[test]
    fn a_linter_that_exits_zero_is_clean_whatever_it_printed() {
        let verdict = lint_verdict(
            Ok(ran(true, "docs/a.md.1.py:1:1: error: x\n", "warn\n")),
            Some(Diagnostics::Gnu),
            &block_names(),
            3,
        );

        assert_eq!(verdict, lint::Verdict::Clean);
    }

    #[test]
    fn without_gnu_a_failed_linter_quotes_twenty_plain_lines_stdout_first() {
        let stdout = (1..=15)
            .map(|n| format!("out {n}\n"))
            .collect::<Vec<_>>()
            .concat();
        let stderr = (1..=10)
            .map(|n| format!("\u{1b}[31merr {n}\u{1b}[0m\n"))
            .collect::<Vec<_>>()
            .concat();

        let verdict = lint_verdict(Ok(ran(false, &stdout, &stderr)), None, &block_names(), 3);

        let expected: Vec<String> = (1..=15)
            .map(|n| format!("out {n}"))
            .chain((1..=5).map(|n| format!("err {n}")))
            .collect();
        assert_eq!(verdict, lint::Verdict::Unparsed(Some(expected.join("\n"))));
    }

    #[test]
    fn without_gnu_even_a_gnu_line_is_quoted() {
        let verdict = lint_verdict(
            Ok(ran(false, "<stdin>:1:1: error: x\n", "")),
            None,
            &block_names(),
            3,
        );

        assert_eq!(
            verdict,
            lint::Verdict::Unparsed(Some("<stdin>:1:1: error: x".to_owned()))
        );
    }

    #[test]
    fn blank_stdout_lines_never_hide_the_stderr_reason_from_the_quote() {
        let blank = "\n".repeat(25);
        let progress = "\u{1b}[2K\r\n".repeat(25);

        for (stdout, diagnostics) in [(blank, None), (progress, Some(Diagnostics::Gnu))] {
            let verdict = lint_verdict(
                Ok(ran(false, &stdout, "Error: real reason\n")),
                diagnostics,
                &block_names(),
                3,
            );

            assert_eq!(
                verdict,
                lint::Verdict::Unparsed(Some("Error: real reason".to_owned())),
                "lines blank once ANSI is removed shouldn't push stderr out: {stdout:?}"
            );
        }
    }

    #[test]
    fn a_failed_linter_that_printed_nothing_says_so() {
        assert_eq!(gnu_verdict("", " \n"), lint::Verdict::Unparsed(None));
        assert_eq!(
            gnu_verdict(&"\u{1b}[2K\r\n".repeat(25), "\n"),
            lint::Verdict::Unparsed(None),
            "lines blank once ANSI is removed are nothing printed"
        );
    }

    #[test]
    fn a_block_is_named_by_its_path_and_its_absolute_spellings() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let real = root
            .path()
            .canonicalize()
            .expect("the temp dir should resolve");

        let given = names("./-x.md.1.py", root.path());

        let mut expected = vec![
            "./-x.md.1.py".to_owned(),
            root.path().join("-x.md.1.py").display().to_string(),
        ];
        let real = real.join("-x.md.1.py").display().to_string();
        if !expected.contains(&real) {
            expected.push(real);
        }
        assert_eq!(given, expected);
    }
}
