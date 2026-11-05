//! A custom `{ command = [...] }` formatter: the command the config wrote, run on a block in a
//! trusted checkout.
//!
//! The command runs as written, with no shell: `{path}` in any argument becomes the block's
//! virtual path, spelled relative to the root, and the block arrives on stdin exactly as written.
//! It runs from the root's real path with [`ToolEnv`]'s variables and `NO_COLOR=1`, under the
//! block timeout and output cap every tool gets. Its results are never cached: supdoc can't name
//! the files a command reads.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::config::code_blocks::Language;
use crate::finding::{Finding, Location, Severity};

use super::format::{self, Verdict};
use super::runner::{Failure, Ran};
use super::tools::{self, CustomTool, Found, Step, ToolEnv};

/// The variables a custom command gets beyond [`ToolEnv`]'s: color off, so its output quotes as
/// plain text.
pub(crate) const VARS: [(&str, &str); 1] = [("NO_COLOR", "1")];

/// The extension of the virtual file a block tagged `tag`, in `language`, is named by: the tag
/// itself where it names a flavor (`tsx`, `mts`, `cts`, `jsx`, `mjs`, `cjs`, `jsonc`), else the
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
        (Language::Rust, _) => "rs",
        (Language::Shell, _) => "sh",
        (Language::Python, _) => "py",
        (Language::Yaml, _) => "yaml",
        (Language::Toml, _) => "toml",
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

/// Where `program`, a custom command's first element, is: looked up on `env`'s `PATH` when it
/// holds no `/`, else the file it names, relative to `root` unless absolute. Either may be
/// inside the root, which a trusted checkout allows.
///
/// Effect: reads file metadata along `PATH` or at the named file, and resolves symlinks.
#[must_use]
pub(crate) fn find(program: &str, env: &ToolEnv, root: &Path) -> Found {
    if !program.contains('/') {
        return tools::find(program, env, root);
    }
    let named = root.join(program);
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
            "fix what it reports, change {}, or mark the block `format(off: <why>)`",
            key(tool)
        )),
        Verdict::TimedOut => Finding::new(
            format::TOOL_TIMEOUT,
            Severity::Error,
            format!(
                "{named} ran longer than {} on the block",
                timeout.map_or_else(|| "its limit".to_owned(), |limit| format!("{limit:?}"))
            ),
        )
        .with_help(
            "raise the limit with `--code-block-timeout 30s`, or mark the block \
             `format(off: <why>)`",
        ),
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

/// The run's one `code-block-tool-missing` warning, with `code`, at `at`, for `tool` whose program
/// [`find`] didn't find, naming the program and the config key that chose it.
#[must_use]
pub(crate) fn missing(code: &'static str, tool: &CustomTool, at: &Location) -> Finding {
    let program = tool.command().program();
    let shown = program.escape_debug();
    let help = if program.contains('/') {
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

    use super::{Found, Verdict, args, extension, find, judge, virtual_path};
    use crate::code_blocks::runner::{Failure, Ran};
    use crate::code_blocks::tools::ToolEnv;
    use crate::config::code_blocks::Language;

    /// A run that exited as `success` says, printing `stdout` and `stderr`.
    fn ran(success: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
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
            (Language::Yaml, "yml", "yaml"),
            (Language::Toml, "toml", "toml"),
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
}
