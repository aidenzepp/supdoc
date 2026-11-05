//! The reader of prettier's output: its syntax-error report, and the comments in a block that
//! leave part of it unformatted.
//!
//! Probed against prettier 3.9.10 (`ci/node-tools`): `printf 'const x = {\n  a: 1,,\n};\n' |
//! prettier --ignore-path= --stdin-filepath /p/doc.md.1.ts` exits 2 and prints to stderr
//!
//! ```text
//! [error] /p/doc.md.1.ts: SyntaxError: Property assignment expected. (2:8)
//! [error]   1 | const x = {
//! [error] > 2 |   a: 1,,
//! [error]     |        ^
//! [error]   3 | };
//! ```
//!
//! Its lines and columns are 1-based. Without `--ignore-path=`, a stdin path that the root's
//! `.gitignore` or `.prettierignore` covers is printed back unchanged with exit 0, which would
//! pass a misformatted block; an empty `--ignore-path` reads neither file. A config's
//! `requirePragma: true` prints back a block without `@format`, and its `checkIgnorePragma: true`
//! one whose first docblock holds `@noprettier` or `@noformat`, the same way;
//! `--no-require-pragma` and `--no-check-ignore-pragma` override both. A config's `rangeStart` or
//! `rangeEnd` (`rangeEnd: 0`) leaves the block as written too, and an `overrides` entry setting
//! `parser` (`markdown` for `*.md.*.ts`) reads it as another language, which passed `const a=1;`
//! unchanged; `--range-start=0 --range-end=Infinity` and a `--parser` on the command line override
//! them. `typescript` formats a `.tsx` path's JSX, and `babel` a `.jsx` path's. A stdin path with
//! a `node_modules` segment is printed back unchanged even under `--ignore-path=`;
//! `--with-node-modules` formats it. A config's `insertPragma: true` adds `/** @format */` to
//! every block, so each one fails; `--no-insert-pragma` overrides it.
//!
//! prettier warns `Ignored unknown option` for a flag it doesn't know and goes on, so a release
//! older than [`FLOOR`] would take `--no-check-ignore-pragma` as nothing (3.5.3 does) and could
//! print a block back as a pass. supdoc reads `prettier --version` once per run and runs no older
//! one.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use super::format::{self, ParseError};
use super::runner;
use super::tools::ToolEnv;

/// The first prettier release that accepts every flag supdoc gives it: `--no-check-ignore-pragma`
/// is the newest (probed: 3.5.3 ignores it as unknown, 3.6.0 takes it).
pub(crate) const FLOOR: [u64; 3] = [3, 6, 0];

/// The most characters of what `prettier --version` printed that a warning quotes.
const QUOTED: usize = 60;

/// The marker of prettier's syntax-error line.
const SYNTAX_ERROR: &str = "SyntaxError: ";

/// The comment that keeps prettier from formatting the node after it, and starts the
/// `prettier-ignore-start` and `prettier-ignore-end` ranges.
const IGNORE: &str = "prettier-ignore";

/// The parser prettier reads a block by, from its virtual path's `extension` (see
/// [`super::biome::extension`]): `typescript` for a TypeScript flavor, else `babel`.
#[must_use]
pub(crate) fn parser(extension: &str) -> &'static str {
    match extension {
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        _ => "babel",
    }
}

/// Why supdoc won't run the prettier it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unverified {
    /// `--version` printed a plain release older than [`FLOOR`]: that release.
    Older(String),
    /// supdoc couldn't read the release: what happened, as "it printed `...`" or "it passed the
    /// time limit", ready to follow "supdoc couldn't read prettier's version: ".
    Unreadable(String),
}

/// The version `prettier --version` printed in `stdout`, a plain `<major>.<minor>.<patch>`, when
/// it is [`FLOOR`] or later.
///
/// # Errors
///
/// [`Unverified::Older`] for an older plain release; [`Unverified::Unreadable`] quoting the
/// output's first line, ANSI-stripped and cut to [`QUOTED`] characters, when it isn't one.
pub(crate) fn verified(stdout: &[u8]) -> Result<String, Unverified> {
    let text = String::from_utf8_lossy(stdout);
    let version = text.trim();
    let parts: Vec<u64> = version
        .split('.')
        .map(|part| {
            part.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| part.parse::<u64>().ok())
                .flatten()
        })
        .collect::<Option<_>>()
        .filter(|parts: &Vec<u64>| parts.len() == 3)
        .ok_or_else(|| Unverified::Unreadable(printed(&text)))?;
    if parts.as_slice() >= FLOOR.as_slice() {
        Ok(version.to_owned())
    } else {
        Err(Unverified::Older(version.to_owned()))
    }
}

/// What `text`, `--version`'s output, said, for [`Unverified::Unreadable`]: "it printed
/// nothing", or "it printed `<first line>`", ANSI-stripped, without control characters or
/// backticks, cut to [`QUOTED`] characters with `...`.
fn printed(text: &str) -> String {
    let plain = format::strip_ansi(text);
    let line = plain.trim().lines().next().unwrap_or_default();
    let clean: String = line
        .chars()
        .filter(|c| !c.is_control() && *c != '`')
        .collect();
    if clean.is_empty() {
        return "it printed nothing".to_owned();
    }
    let mut quoted: String = clean.chars().take(QUOTED).collect();
    if clean.chars().nth(QUOTED).is_some() {
        quoted.push_str("...");
    }
    format!("it printed `{quoted}`")
}

/// [`verified`] on what `program`, a prettier binary, prints for `--version`, run from `root` as
/// prettier's steps are (see [`runner::run`]) under `timeout`.
///
/// Effect: runs the binary.
///
/// # Errors
///
/// As [`verified`]'s, and [`Unverified::Unreadable`] naming how the run failed when it
/// couldn't start, passed the time limit or its output cap, or exited unsuccessfully.
pub(crate) fn version_of(
    program: &Path,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<Duration>,
) -> Result<String, Unverified> {
    let call = runner::Call {
        program,
        name: "prettier",
        args: &[OsStr::new("--version")],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Root,
        cap: runner::CAP,
    };
    let failed = |how: &str| Err(Unverified::Unreadable(how.to_owned()));
    match runner::run(&call, env, root) {
        Ok(ran) if ran.success => verified(&ran.stdout),
        Ok(_) => failed("it exited unsuccessfully"),
        Err(runner::Failure::TimedOut) => failed("it passed the time limit"),
        Err(runner::Failure::OverCap { .. }) => failed("it printed more than supdoc reads"),
        Err(runner::Failure::Spawn(_)) => failed("it couldn't start"),
    }
}

/// prettier's syntax-error report in `stderr`: the first line holding `SyntaxError: `, at the
/// `(L:C)` that ends it. `None` for any other stderr, and for that line without its position.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let line = stderr.lines().find(|line| line.contains(SYNTAX_ERROR))?;
    let (_, position) = line.trim_end().strip_suffix(')')?.rsplit_once('(')?;
    let (line, column) = position.split_once(':')?;
    let (line, _column) = (line.parse::<usize>().ok()?, column.parse::<usize>().ok()?);
    Some(ParseError {
        line: Some(line.checked_sub(1)?),
    })
}

/// Whether `block` holds `prettier-ignore` in any case, so prettier may leave part of it as
/// written and supdoc can't confirm it formats the block. supdoc's own `format(off: ...)` is the
/// way to switch the step off.
///
/// The scan reads text, not code, so the words inside a string count too, as does
/// `prettier-ignore-start`: a near miss counts as switching the step off, never as a pass.
#[must_use]
pub(crate) fn switches_format_off(block: &str) -> bool {
    block.to_ascii_lowercase().contains(IGNORE)
}

#[cfg(test)]
mod tests {
    use super::{Unverified, parse_error, parser, switches_format_off, verified};
    use crate::code_blocks::format::ParseError;

    #[test]
    fn a_typescript_flavor_is_read_by_typescript_and_any_other_by_babel() {
        for (extension, expected) in [
            ("ts", "typescript"),
            ("tsx", "typescript"),
            ("mts", "typescript"),
            ("cts", "typescript"),
            ("js", "babel"),
            ("jsx", "babel"),
            ("mjs", "babel"),
            ("cjs", "babel"),
        ] {
            assert_eq!(parser(extension), expected, "{extension}");
        }
    }

    #[test]
    fn a_plain_version_at_or_past_the_floor_is_verified() {
        for stdout in ["3.6.0\n", "3.9.10\n", "4.0.0", "10.0.0\n"] {
            assert_eq!(
                verified(stdout.as_bytes()),
                Ok(stdout.trim().to_owned()),
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn an_older_version_is_named_as_older() {
        for stdout in ["3.5.3\n", "2.8.8\n", "3.0.0"] {
            assert_eq!(
                verified(stdout.as_bytes()),
                Err(Unverified::Older(stdout.trim().to_owned())),
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn output_that_isnt_a_plain_version_is_unreadable_quoting_its_first_line() {
        for (stdout, said) in [
            ("", "it printed nothing"),
            ("  \n", "it printed nothing"),
            ("garbage\n", "it printed `garbage`"),
            ("3.6\n", "it printed `3.6`"),
            ("3.6.0-beta.1\n", "it printed `3.6.0-beta.1`"),
            ("v3.9.10\n", "it printed `v3.9.10`"),
            ("3.9.10\nmore\n", "it printed `3.9.10`"),
            ("\u{1b}[31mred`tick`\u{7}\n", "it printed `redtick`"),
        ] {
            assert_eq!(
                verified(stdout.as_bytes()),
                Err(Unverified::Unreadable(said.to_owned())),
                "{stdout:?}"
            );
        }
    }

    #[test]
    fn a_long_unreadable_output_is_cut_with_an_ellipsis() {
        let long = "x".repeat(100);

        assert_eq!(
            verified(long.as_bytes()),
            Err(Unverified::Unreadable(format!(
                "it printed `{}...`",
                "x".repeat(60)
            )))
        );
    }

    #[test]
    fn a_syntax_error_is_a_parse_error_at_its_line() {
        let stderr = "[error] /p/a.md.1.ts: SyntaxError: Property assignment expected. (2:8)\n\
                      [error]   1 | const x = {\n\
                      [error] > 2 |   a: 1,,\n";

        assert_eq!(parse_error(stderr), Some(ParseError { line: Some(1) }));
    }

    #[test]
    fn a_doc_path_holding_digits_and_colons_never_moves_the_line() {
        let stderr = "[error] /p/v2:3/a.md.1.js: SyntaxError: Unexpected token (4:1)\n";

        assert_eq!(parse_error(stderr), Some(ParseError { line: Some(3) }));
    }

    #[test]
    fn stderr_without_a_syntax_error_is_no_parse_error() {
        for stderr in [
            "",
            "[error] Invalid configuration for file \"/p/a.md.1.ts\":\n",
            "[error] /p/a.md.1.ts: SyntaxError: no position here\n",
            "[error] /p/a.md.1.ts: SyntaxError: line zero (0:1)\n",
        ] {
            assert_eq!(parse_error(stderr), None, "{stderr:?}");
        }
    }

    #[test]
    fn any_prettier_ignore_switches_the_format_step_off() {
        for block in [
            "// prettier-ignore\nconst  a = 1;",
            "const a = [/* prettier-ignore */ 1,2];",
            "const a = <div>{/* Prettier-Ignore */}</div>;",
            "// prettier-ignore-start\nconst  a = 1;\n// prettier-ignore-end",
            "const note = \"prettier-ignore\";",
        ] {
            assert!(switches_format_off(block), "{block:?}");
        }
    }

    #[test]
    fn a_block_without_prettier_ignore_keeps_its_format_step() {
        for block in ["const a = 1;", "// prettier ignore\nconst a = 1;", ""] {
            assert!(!switches_format_off(block), "{block:?}");
        }
    }
}
