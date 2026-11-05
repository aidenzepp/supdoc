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
//! them. `typescript` formats a `.tsx` path's JSX, and `babel` a `.jsx` path's.

use super::format::ParseError;

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
    use super::{parse_error, parser, switches_format_off};
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
