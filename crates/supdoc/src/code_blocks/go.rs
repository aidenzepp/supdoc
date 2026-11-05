//! Go's tools: gofmt's environment, where its syntax errors sit, and when its positions are the
//! block's own; and the supdoc-owned module `go vet` lints a block in, and how its report reads.
//!
//! gofmt reads the block on stdin with no arguments. It has no config file and no stdin file
//! name, so no file of the project or the user reaches it, wherever it runs.
//!
//! gofmt tries its input as a file, then as a declaration list behind `package p;`, then as a
//! statement list inside `package p; func _() {` … `}`. Each wrapper opens on the block's first
//! line, so a fragment's error lines are the block's own while a column on line 1 counts the
//! wrapper too, and the statement wrapper's closing lines sit past the block's end: a fragment's
//! error is placed by its line alone, and one past the block is the wrapper's. A `//line` or
//! `/*line` directive in the block renames every position gofmt reports, so such a block's error
//! sits at its fence.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::format::{self, ParseError, Positions, first_line};
use super::lint::{self, Diagnostic};
use super::runner::{Failure, Ran};

/// The variables gofmt gets beyond the tool environment: `GOTOOLCHAIN=local`, so a `go` front
/// end never downloads a toolchain to run it.
pub(crate) const VARS: &[(&str, &str)] = &[("GOTOOLCHAIN", "local")];

/// The variables `go vet` gets beyond the tool environment, so the `go` command loads nothing the
/// checkout or the user chose and fetches nothing: no cgo, no toolchain download, no module proxy,
/// no workspace, no `go env` file, and module mode that never reads a vendor directory. The
/// per-run directories it uses ([`VetModule::vars`]) are set when it runs.
pub(crate) const VET_VARS: &[(&str, &str)] = &[
    ("CGO_ENABLED", "0"),
    ("GOTOOLCHAIN", "local"),
    ("GOPROXY", "off"),
    ("GOWORK", "off"),
    ("GOENV", "off"),
    ("GOFLAGS", "-mod=mod"),
];

/// The `go.mod` supdoc writes beside the block: a module path under `.invalid`, which no lookup
/// resolves, and the language version a block is read at. A Go older than it refuses the module,
/// which is a tool failure.
const GO_MOD: &str = "module supdoc.invalid/block\n\ngo 1.23\n";

/// The block's file in the module.
const MAIN: &str = "main.go";

/// Why `go vet`'s step didn't run on a block that imports a package outside Go's standard
/// library: the module has no other package, and nothing is fetched.
pub(crate) const VET_OFFLINE: &str =
    "the block imports a package outside Go's standard library, which `go vet` can't load offline";

/// Why `go vet`'s step didn't run on a block its build constraints leave out: a `//go:build`
/// line that doesn't hold here, or `import "C"` with cgo off.
pub(crate) const VET_EXCLUDED: &str =
    "`go vet` checked no file: the block's build constraints leave it out";

/// What `go`'s stderr says about an import it can't load: a module it would have to fetch, a
/// path that is in no module and not in the standard library, or a relative path.
const UNLOADABLE_IMPORT: [&str; 4] = [
    "cannot find module providing package",
    "no required module provides package",
    "is not in std",
    "relative import paths are not supported",
];

/// How many [`VetModule`] directories this process has tried to create, which makes each name
/// unique.
static VET_RUNS: AtomicUsize = AtomicUsize::new(0);

/// One `go vet` run's module, removed when this is dropped: a directory `go/<pid>-<n>` beside the
/// neutral directory holding the `go.mod` supdoc writes and the block as `main.go`, never a file
/// of the checkout. `go vet` runs in it, so the `go.mod` walk-up stops there.
#[derive(Debug)]
pub(crate) struct VetModule {
    /// The module directory, owned by this run alone.
    dir: PathBuf,
    /// supdoc's code-block cache directory, which the module and the shared Go directories sit in.
    cache: PathBuf,
    /// The variables naming the shared directories `go` keeps its state in: `HOME`, `GOPATH`
    /// and `GOCACHE`, each a 0700 directory beside the module, so no user config, module cache
    /// or telemetry setting reaches the run.
    vars: [(&'static str, PathBuf); 3],
}

impl VetModule {
    /// A new 0700 module directory `go/<pid>-<n>` beside `neutral`, inside a 0700 `go`
    /// directory, holding `go.mod` and `input` as `main.go`, each created new, mode 0600, with the
    /// 0700 directories `go-home`, `go-path` and `go-build` beside it. `None` when `neutral` is
    /// `/` or anything can't be created.
    ///
    /// Effect: creates directories and writes two files.
    #[must_use]
    pub(crate) fn create(neutral: &Path, input: &str) -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt as _;

        if neutral == Path::new("/") {
            return None;
        }
        let cache = neutral.parent()?.to_path_buf();
        let shared = |name: &str| private(&cache.join(name));
        let vars = [
            ("HOME", shared("go-home")?),
            ("GOPATH", shared("go-path")?),
            ("GOCACHE", shared("go-build")?),
        ];
        let base = shared("go")?;
        let dir = (0..16).find_map(|_| {
            let count = VET_RUNS.fetch_add(1, Ordering::Relaxed);
            let dir = base.join(format!("{}-{count}", std::process::id()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .ok()
                .map(|()| dir)
        })?;
        let module = Self { dir, cache, vars };
        write_new(&module.dir.join("go.mod"), GO_MOD)?;
        write_new(&module.dir.join(MAIN), input)?;
        Some(module)
    }

    /// The module directory, where `go vet` runs.
    #[must_use]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// `HOME`, `GOPATH` and `GOCACHE`, each naming its shared directory.
    #[must_use]
    pub(crate) fn vars(&self) -> &[(&'static str, PathBuf)] {
        &self.vars
    }

    /// `ran` with every private path `go` printed taken out: the module directory's files by
    /// their own names, and supdoc's cache directory by name, so no message shows a private path.
    #[must_use]
    pub(crate) fn unnamed(&self, ran: &Ran) -> Ran {
        let paths = |dir: &Path| {
            let real = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
            [real, dir.to_path_buf()]
        };
        let [module, module_given] = paths(&self.dir);
        let [cache, cache_given] = paths(&self.cache);
        let unname = |bytes: &[u8]| {
            let mut text = String::from_utf8_lossy(bytes).into_owned();
            for dir in [&module, &module_given] {
                let dir = dir.to_string_lossy();
                text = text.replace(&format!("{dir}/"), "").replace(&*dir, ".");
            }
            for dir in [&cache, &cache_given] {
                text = text.replace(&*dir.to_string_lossy(), "<supdoc's cache>");
            }
            text.into_bytes()
        };
        Ran {
            success: ran.success,
            stdout: unname(&ran.stdout),
            stderr: unname(&ran.stderr),
        }
    }
}

impl Drop for VetModule {
    fn drop(&mut self) {
        let _removed = std::fs::remove_dir_all(&self.dir);
    }
}

/// `dir`, created mode 0700 with its parents when missing, when it is a directory of mode 0700.
///
/// Effect: creates the directory and reads its metadata.
fn private(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let meta = std::fs::symlink_metadata(dir).ok()?;
    (meta.is_dir() && meta.permissions().mode() & 0o777 == 0o700).then(|| dir.to_path_buf())
}

/// Writes `text` to `path`, which must not exist yet, mode 0600.
///
/// Effect: creates and writes the file.
fn write_new(path: &Path, text: &str) -> Option<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .ok()?
        .write_all(text.as_bytes())
        .ok()
}

/// Why a finished `go vet` run says nothing about its block, so its step counts as not run:
/// [`VET_OFFLINE`] or [`VET_EXCLUDED`]. `None` for any other run.
#[must_use]
pub(crate) fn vet_not_run(ran: &Ran) -> Option<&'static str> {
    if ran.success {
        return None;
    }
    let stderr = String::from_utf8_lossy(&ran.stderr);
    if stderr.contains("build constraints exclude all Go files") {
        Some(VET_EXCLUDED)
    } else if UNLOADABLE_IMPORT
        .iter()
        .any(|marker| stderr.contains(marker))
    {
        Some(VET_OFFLINE)
    } else {
        None
    }
}

/// `go vet`'s verdict on `result`, a run over the block `input`. A block with no `package` clause
/// isn't a Go file, so it fails with one diagnostic naming the clause to add, whatever the run.
/// A run that exits 0 gives its `-json` report's diagnostics, each named by its analyzer. One that
/// fails gives its first syntax error as a parse failure, else its type errors as `typecheck`
/// diagnostics, else a tool failure. A `//line` or `/*line` directive renames the positions `go`
/// reports, so each such block's diagnostics sit at its fence.
#[must_use]
pub(crate) fn vet_verdict(
    input: &str,
    _framed: bool,
    result: Result<Ran, Failure>,
) -> lint::Verdict {
    if !is_file(input) {
        return lint::Verdict::Diagnostics(vec![Diagnostic {
            line: 1,
            column: 1,
            end_line: 1,
            syntax: false,
            rule: "package".to_owned(),
            message: "the block has no `package` clause, so `go vet` can't check it as a Go \
                      file: add one, such as `package main`, as its first line"
                .to_owned(),
        }]);
    }
    let renamed = positions(input) == Positions::Renamed;
    match result {
        Ok(ran) if ran.success => match report(&ran.stdout, renamed) {
            Some(found) if found.is_empty() => lint::Verdict::Clean,
            Some(found) => lint::Verdict::Diagnostics(found),
            None => lint::Verdict::Failed(format::Verdict::ToolFailed(
                "`go vet -json` printed a report supdoc couldn't read".to_owned(),
            )),
        },
        Ok(ran) => {
            let errors = errors(&String::from_utf8_lossy(&ran.stderr), renamed);
            if let Some(syntax) = errors.iter().find(|error| error.syntax) {
                let detail = if syntax.line == 0 {
                    syntax.message.clone()
                } else {
                    format!("{}:{}: {}", syntax.line, syntax.column, syntax.message)
                };
                lint::Verdict::Failed(format::Verdict::ParseFailed {
                    detail,
                    line: syntax.line.checked_sub(1),
                })
            } else if errors.is_empty() {
                lint::judge(Ok(ran), |_| None)
            } else {
                lint::Verdict::Diagnostics(errors)
            }
        }
        Err(failure) => lint::judge(Err(failure), |_| None),
    }
}

/// The diagnostics in `go vet -json`'s report: an object of packages, each an object of
/// analyzers, each a list of `{posn, end, message}`. `None` when `stdout` isn't such a report, or
/// an analyzer reports an error of its own.
fn report(stdout: &[u8], renamed: bool) -> Option<Vec<Diagnostic>> {
    let report: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let mut found = Vec::new();
    for analyzers in report.as_object()?.values() {
        for (rule, listed) in analyzers.as_object()? {
            for diagnostic in listed.as_array()? {
                let (line, column) = place(diagnostic.get("posn")?.as_str()?, renamed);
                let end_line = diagnostic
                    .get("end")
                    .and_then(serde_json::Value::as_str)
                    .map_or(line, |end| place(end, renamed).0.max(line));
                found.push(Diagnostic {
                    line,
                    column,
                    end_line,
                    syntax: false,
                    rule: rule.clone(),
                    message: diagnostic.get("message")?.as_str()?.to_owned(),
                });
            }
        }
    }
    found.sort_by_key(|diagnostic| (diagnostic.line, diagnostic.column));
    Some(found)
}

/// The file, line and column `posn`, `<file>:<line>[:<column>]`, names, a leading `./` dropped:
/// column 0 when it names none. `None` when `posn` isn't a position.
fn position(posn: &str) -> Option<(&str, usize, usize)> {
    let (rest, last) = posn.rsplit_once(':')?;
    let last = last.parse().ok()?;
    let (file, line, column) = rest
        .rsplit_once(':')
        .and_then(|(file, line)| Some((file, line.parse().ok()?, last)))
        .unwrap_or((rest, last, 0));
    Some((file.strip_prefix("./").unwrap_or(file), line, column))
}

/// The line and column `posn` names in the block: `(0, 0)`, the fence, when it names another
/// file, isn't a position, or `renamed` says a directive renamed it.
fn place(posn: &str, renamed: bool) -> (usize, usize) {
    match position(posn) {
        Some((file, line, column)) if !renamed && file == MAIN => (line, column),
        _ => (0, 0),
    }
}

/// The errors `go vet` printed for a Go file, each `[vet: ]<file>:<line>:<column>: <message>`,
/// a syntax error's marked so, and its `(and N more errors)` dropped. One in a file other than
/// the block's, renamed by a directive, sits at the fence.
fn errors(stderr: &str, renamed: bool) -> Vec<Diagnostic> {
    stderr
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("vet: ").unwrap_or(line);
            let (posn, message) = line.split_once(": ")?;
            let (file, ..) = position(posn)?;
            if Path::new(file)
                .extension()
                .is_none_or(|extension| extension != "go")
            {
                return None;
            }
            let (line, column) = place(posn, renamed);
            let message = message
                .split_once(" (and ")
                .filter(|(_, more)| more.ends_with(" more errors)"))
                .map_or(message, |(message, _)| message);
            Some(Diagnostic {
                line,
                column,
                end_line: line,
                syntax: is_syntax(message),
                rule: "typecheck".to_owned(),
                message: message.to_owned(),
            })
        })
        .collect()
}

/// The openings of Go's parser and scanner errors, which its type checker's never share.
const PARSER: [&str; 9] = [
    "expected ",
    "missing '",
    "illegal ",
    "unexpected ",
    "invalid character",
    "string literal not terminated",
    "raw string literal not terminated",
    "comment not terminated",
    "rune literal",
];

/// Whether `message`, an error `go` printed, is Go's parser's rather than its type checker's.
fn is_syntax(message: &str) -> bool {
    PARSER.iter().any(|opening| message.starts_with(opening))
}

/// gofmt's syntax-error report: a line `<standard input>:L:C: <message>`. `None` for any other
/// stderr.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let first = stderr.lines().next()?;
    let line = first_line(first)?;
    first
        .starts_with("<standard input>:")
        .then_some(ParseError { line: Some(line) })
}

/// How far gofmt's error positions on `input` are the block's own: renamed by a `//line` or
/// `/*line` directive, wholly for a file, and by line alone for a fragment.
#[must_use]
pub(crate) fn positions(input: &str) -> Positions {
    if input.contains("//line ") || input.contains("/*line ") {
        Positions::Renamed
    } else if is_file(input) {
        Positions::Own
    } else {
        Positions::Lines
    }
}

/// Whether gofmt reads `input` as a whole file: the first token past comments is `package`.
fn is_file(input: &str) -> bool {
    let mut rest = input;
    loop {
        rest = rest.trim_start();
        if let Some(comment) = rest.strip_prefix("//") {
            rest = comment.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
    }
    rest.strip_prefix("package")
        .is_some_and(|after| after.starts_with(|c: char| c.is_whitespace() || c == '/'))
}

#[cfg(test)]
mod tests {
    use super::{
        VET_EXCLUDED, VET_OFFLINE, is_file, parse_error, positions, vet_not_run, vet_verdict,
    };
    use crate::code_blocks::format::{ParseError, Positions, Verdict as FormatVerdict};
    use crate::code_blocks::lint::{Diagnostic, Verdict};
    use crate::code_blocks::runner::Ran;

    /// A finished run of go vet.
    fn ran(success: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// A clean Go file.
    const FILE: &str = "package p\n\nfunc f() {}\n";

    #[test]
    fn vets_json_report_gives_one_diagnostic_per_analyzer_finding_in_line_order() {
        let stdout = r#"{"supdoc.invalid/block": {
            "printf": [{"posn": "main.go:7:2", "end": "main.go:8:3", "message": "b"}],
            "unusedresult": [{"posn": "main.go:3:14", "end": "main.go:3:16", "message": "a"}]
        }}"#;

        let verdict = vet_verdict(FILE, false, Ok(ran(true, stdout, "")));

        let Verdict::Diagnostics(found) = verdict else {
            panic!("the report should give diagnostics: {verdict:?}");
        };
        let placed: Vec<_> = found
            .iter()
            .map(|d| (d.line, d.column, d.end_line, d.rule.as_str()))
            .collect();
        assert_eq!(placed, [(3, 14, 3, "unusedresult"), (7, 2, 8, "printf")]);
    }

    #[test]
    fn an_empty_report_is_clean_and_an_unreadable_one_a_tool_failure() {
        assert_eq!(
            vet_verdict(FILE, false, Ok(ran(true, "{}", ""))),
            Verdict::Clean
        );
        assert!(matches!(
            vet_verdict(FILE, false, Ok(ran(true, "nope", ""))),
            Verdict::Failed(FormatVerdict::ToolFailed(_))
        ));
    }

    #[test]
    fn a_position_in_another_file_or_after_a_line_directive_sits_at_the_fence() {
        let stdout = |file: &str| {
            format!(r#"{{"m": {{"printf": [{{"posn": "{file}:10", "message": "x"}}]}}}}"#)
        };
        let lines = |input: &str, file: &str| match vet_verdict(
            input,
            false,
            Ok(ran(true, &stdout(file), "")),
        ) {
            Verdict::Diagnostics(found) => found.iter().map(|d| d.line).collect::<Vec<_>>(),
            other => panic!("the report should give diagnostics: {other:?}"),
        };

        assert_eq!(lines(FILE, "foo.go"), [0]);
        assert_eq!(lines(FILE, "main.go"), [10]);
        assert_eq!(lines("package p\n//line main.go:9\n", "main.go"), [0]);
    }

    #[test]
    fn a_syntax_error_is_a_parse_failure_at_its_line_and_a_type_error_a_typecheck_diagnostic() {
        let syntax = "# m\nvet: ./main.go:4:7: expected operand, found '<' (and 1 more errors)\n";
        let typed = "vet: ./main.go:4:14: cannot use \"s\" as int value\n";

        assert_eq!(
            vet_verdict(FILE, false, Ok(ran(false, "", syntax))),
            Verdict::Failed(FormatVerdict::ParseFailed {
                detail: "4:7: expected operand, found '<'".to_owned(),
                line: Some(3),
            })
        );
        assert_eq!(
            vet_verdict(FILE, false, Ok(ran(false, "", typed))),
            Verdict::Diagnostics(vec![Diagnostic {
                line: 4,
                column: 14,
                end_line: 4,
                syntax: false,
                rule: "typecheck".to_owned(),
                message: "cannot use \"s\" as int value".to_owned(),
            }])
        );
    }

    #[test]
    fn a_block_with_no_package_clause_fails_with_one_diagnostic_naming_the_clause() {
        let verdict = vet_verdict("x := 1\n", false, Ok(ran(true, "{}", "")));

        let Verdict::Diagnostics(found) = verdict else {
            panic!("a block without a package clause should fail: {verdict:?}");
        };
        let [only] = found.as_slice() else {
            panic!("the block should get one diagnostic: {found:?}");
        };
        assert_eq!(only.rule, "package");
        assert!(only.message.contains("`package main`"));
    }

    #[test]
    fn an_unloadable_import_or_excluded_file_is_not_run_and_any_other_failure_is_judged() {
        let offline = "main.go:3:8: cannot find module providing package example.com/x: module \
                       lookup disabled by GOPROXY=off\n";
        let unknown = "main.go:3:8: package foo/bar is not in std (/go/src/foo/bar)\n";
        let excluded = "package m: build constraints exclude all Go files in .\n";

        assert_eq!(vet_not_run(&ran(false, "", offline)), Some(VET_OFFLINE));
        assert_eq!(vet_not_run(&ran(false, "", unknown)), Some(VET_OFFLINE));
        assert_eq!(vet_not_run(&ran(false, "", excluded)), Some(VET_EXCLUDED));
        assert_eq!(
            vet_not_run(&ran(false, "", "main.go:1:1: expected x\n")),
            None
        );
        assert_eq!(vet_not_run(&ran(true, "{}", excluded)), None);
    }

    #[test]
    fn a_standard_input_position_is_a_parse_error_at_that_line() {
        assert_eq!(
            parse_error("<standard input>:4:7: expected operand, found '<'\n"),
            Some(ParseError { line: Some(3) })
        );
    }

    #[test]
    fn stderr_without_a_standard_input_position_is_not_a_parse_error() {
        assert_eq!(parse_error("gofmt: read error\n"), None);
        assert_eq!(parse_error("/p/a.go:1:2: x\n"), None);
        assert_eq!(parse_error(""), None);
    }

    #[test]
    fn a_block_opening_with_a_package_clause_is_a_file() {
        assert!(is_file("package main\n\nfunc main() {}\n"));
        assert!(is_file(
            "// Package p does a thing.\n/* more */\npackage p\n"
        ));
        assert!(is_file("package/* c */p\n"));
    }

    #[test]
    fn a_line_directive_renames_positions_and_a_fragment_keeps_its_lines() {
        assert_eq!(positions("package p\n"), Positions::Own);
        assert_eq!(positions("x := 1\n"), Positions::Lines);
        assert_eq!(positions("package p\n//line a.go:9\n"), Positions::Renamed);
        assert_eq!(positions("x := 1 /*line :4*/\n"), Positions::Renamed);
    }

    #[test]
    fn a_fragment_is_not_a_file() {
        assert!(!is_file("x := 1\nfmt.Println(x)\n"));
        assert!(!is_file("func f() {}\n"));
        assert!(!is_file("packages := 1\n"));
        assert!(!is_file("// package p\nx := 1\n"));
        assert!(!is_file(""));
    }
}
