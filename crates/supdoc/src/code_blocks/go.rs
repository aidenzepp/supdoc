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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::format::{self, ParseError, Positions, first_line};
use super::lint::{self, Diagnostic};
use super::runner::{self, Call, Failure, Ran};
use sha2::{Digest, Sha256};

use super::tools::ToolEnv;

/// The variables gofmt gets beyond the tool environment: `GOTOOLCHAIN=local`, so a `go` front
/// end never downloads a toolchain to run it.
pub(crate) const VARS: &[(&str, &str)] = &[("GOTOOLCHAIN", "local")];

/// The variables `go vet` gets beyond the tool environment, so the `go` command loads nothing the
/// checkout or the user chose and fetches nothing: no cgo, no toolchain download, no module proxy,
/// no workspace, no `go env` file, and module mode that never reads a vendor directory nor looks
/// up a module, even through a proxy. The per-run directories it uses ([`VetModule::vars`]) are
/// set when it runs.
pub(crate) const VET_VARS: &[(&str, &str)] = &[
    ("CGO_ENABLED", "0"),
    ("GOTOOLCHAIN", "local"),
    ("GOPROXY", "off"),
    ("GOWORK", "off"),
    ("GOENV", "off"),
    ("GOFLAGS", "-mod=readonly"),
];

/// The module path of the `go.mod` supdoc writes: under `.invalid`, which no lookup resolves.
const MODULE: &str = "supdoc.invalid/block";

/// The line `go` prints when the block's build constraints leave out its one file, the
/// directory following.
const EXCLUDED: &str = "package supdoc.invalid/block: build constraints exclude all Go files in ";

/// Why `go vet`'s step didn't run on any Go block when `go env GOVERSION` named no release supdoc
/// can write as the module's `go` line: a block is read at the toolchain's own language version.
pub(crate) const VET_VERSION: &str = "supdoc couldn't read which Go release `go` is";

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

/// A Go release as `go env GOVERSION` names it, `go1.<minor>` or `go1.<minor>.<patch>`, held
/// without its `go` prefix: `1.<minor>[.<patch>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoVersion(String);

impl GoVersion {
    /// The release `text` names, exactly `go1.<minor>` or `go1.<minor>.<patch>` with decimal
    /// numbers. `None` for anything else: a `devel` build, a release candidate, a suffix.
    #[must_use]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let numbers = text.strip_prefix("go1.")?;
        let parts: Vec<&str> = numbers.split('.').collect();
        let decimal = |part: &&str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        (parts.len() <= 2 && parts.iter().all(decimal)).then(|| Self(format!("1.{numbers}")))
    }

    /// The `go.mod` this release reads a block at: [`MODULE`], and the release's language
    /// version `1.<minor>` as its `go` line, which every Go reads (Go 1.20 and older refuse a
    /// patch there) and no local toolchain of this release is older than, so
    /// `GOTOOLCHAIN=local` never asks for another.
    fn go_mod(&self) -> String {
        let language = self.0.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
        format!("module {MODULE}\n\ngo {language}\n")
    }

    /// The release's digest, which a cached `go vet` result is keyed on, so a block is checked
    /// again under another release.
    #[must_use]
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        <Sha256 as Digest>::digest(self.0.as_bytes()).into()
    }
}

/// The release each `go` binary this process asked named, by its real path: asked once per run.
static RELEASES: Mutex<Vec<(PathBuf, Result<GoVersion, String>)>> = Mutex::new(Vec::new());

/// The release `program`, a `go` binary, names in `go env GOVERSION`: asked once per process, in
/// a private module of its own with [`VET_VARS`], under `timeout`.
///
/// # Errors
///
/// What `go` printed, or how its run failed, when that names no release [`GoVersion::parse`]
/// reads.
///
/// Effect: creates and removes a private directory, and runs `go env GOVERSION` the first time.
pub(crate) fn release(
    program: &Path,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<Duration>,
) -> Result<GoVersion, String> {
    let mut known = RELEASES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, found)) = known.iter().find(|(path, _)| path == program) {
        return found.clone();
    }
    let found = ask_release(program, env, root, timeout);
    known.push((program.to_path_buf(), found.clone()));
    found
}

/// `go env GOVERSION`'s answer, as [`release`] reads it, asked afresh.
///
/// # Errors
///
/// As [`release`].
///
/// Effect: creates and removes a private directory, and runs `go`.
fn ask_release(
    program: &Path,
    env: &ToolEnv,
    root: &Path,
    timeout: Option<Duration>,
) -> Result<GoVersion, String> {
    const ASKED: &str = "`go env GOVERSION`";
    let neutral = runner::neutral_dir(env, root);
    let module = VetModule::create(&neutral, &format!("module {MODULE}\n"), None)
        .ok_or_else(|| format!("supdoc couldn't create a private directory to run {ASKED} in"))?;
    let extra = module.extra();
    let args = [OsStr::new("env"), OsStr::new("GOVERSION")];
    let call = Call {
        program,
        name: "go",
        args: &args,
        extra: &extra,
        stdin: &[],
        timeout,
        cwd: runner::Cwd::Private(module.dir()),
        cap: 4096,
    };
    match runner::run(&call, env, root).map(|ran| module.unnamed(&ran)) {
        Ok(ran) if ran.success => {
            let printed = String::from_utf8_lossy(&ran.stdout);
            let printed = printed.trim_end();
            GoVersion::parse(printed).ok_or_else(|| {
                let shown: String = printed
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                format!("{ASKED} printed `{shown}`, not `go1.<minor>` or `go1.<minor>.<patch>`")
            })
        }
        Ok(ran) => Err(format!(
            "{ASKED} failed: {}",
            format::exit_detail(&ran.stderr)
        )),
        Err(Failure::TimedOut) => Err(format!("{ASKED} ran past the time limit")),
        Err(Failure::OverCap { stream, .. }) => {
            Err(format!("{ASKED} printed too much to {stream}"))
        }
        Err(Failure::Spawn(error)) => Err(format!("{ASKED} couldn't start: {error}")),
    }
}

/// How many [`VetModule`] directories this process has tried to create, which makes each name
/// unique.
static VET_RUNS: AtomicUsize = AtomicUsize::new(0);

/// One `go` run's module, removed when this is dropped: a directory `go/<pid>-<n>` beside the
/// neutral directory holding the `go.mod` supdoc writes, the block as `main.go` and a `tmp`
/// directory for `go`'s own work files, never a file of the checkout. `go` runs in it, so the
/// `go.mod` walk-up stops there.
#[derive(Debug)]
pub(crate) struct VetModule {
    /// The module directory, owned by this run alone.
    dir: PathBuf,
    /// supdoc's code-block cache directory, which the module and the shared Go directories sit in.
    cache: PathBuf,
    /// The variables naming the directories `go` keeps its state in: `HOME`, `GOPATH` and
    /// `GOCACHE`, each a shared 0700 directory beside the module, so no user config, module
    /// cache or telemetry setting reaches the run, and `GOTMPDIR`, the module's own 0700 `tmp`,
    /// so a work directory a killed `go` leaves behind goes with the module.
    vars: [(&'static str, PathBuf); 4],
}

impl VetModule {
    /// A new module for the block `input`, read at `release`: see [`VetModule::create`].
    ///
    /// Effect: creates directories and writes two files.
    #[must_use]
    pub(crate) fn for_block(neutral: &Path, release: &GoVersion, input: &str) -> Option<Self> {
        Self::create(neutral, &release.go_mod(), Some(input))
    }

    /// A new 0700 module directory `go/<pid>-<n>` beside `neutral`, inside a 0700 `go`
    /// directory, holding `go_mod` as `go.mod`, `input` as `main.go` when given, each created new,
    /// mode 0600, and a 0700 `tmp`, with the 0700 directories `go-home`, `go-path` and `go-build`
    /// beside it. `None` when `neutral` is `/` or anything can't be created.
    ///
    /// Effect: creates directories and writes up to two files.
    #[must_use]
    fn create(neutral: &Path, go_mod: &str, input: Option<&str>) -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt as _;

        if neutral == Path::new("/") {
            return None;
        }
        let cache = neutral.parent()?.to_path_buf();
        let shared = |name: &str| private(&cache.join(name));
        let home = shared("go-home")?;
        let path = shared("go-path")?;
        let build = shared("go-build")?;
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
        let tmp = dir.join("tmp");
        let module = Self {
            vars: [
                ("HOME", home),
                ("GOPATH", path),
                ("GOCACHE", build),
                ("GOTMPDIR", tmp.clone()),
            ],
            dir,
            cache,
        };
        let _tmp = private(&tmp)?;
        write_new(&module.dir.join("go.mod"), go_mod)?;
        if let Some(input) = input {
            write_new(&module.dir.join(MAIN), input)?;
        }
        Some(module)
    }

    /// The module directory, where `go vet` runs.
    #[must_use]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The variables `go` runs with beyond the tool environment: [`VET_VARS`], then `HOME`,
    /// `GOPATH`, `GOCACHE` and `GOTMPDIR`, each naming its private directory.
    #[must_use]
    pub(crate) fn extra(&self) -> Vec<(&'static str, &OsStr)> {
        VET_VARS
            .iter()
            .map(|(name, value)| (*name, OsStr::new(*value)))
            .chain(
                self.vars
                    .iter()
                    .map(|(name, value)| (*name, value.as_os_str())),
            )
            .collect()
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

/// Why a finished `go vet` run on the block `input` says nothing about it, so its step counts as
/// not run: [`VET_OFFLINE`] when `go` couldn't load an import the block names, or
/// [`VET_EXCLUDED`] when the block's build constraints leave its file out. Only when the block's
/// text can't have written those lines itself: it holds no `//line` or `/*line` directive, its
/// import declarations read plainly (see [`plain_imports`]), and every line of stderr is one of
/// `go`'s own load or exclusion lines (see [`go_line`]), each load line naming an import the
/// block declares. `None` for any other run, which is then judged as a failure.
#[must_use]
pub(crate) fn vet_not_run(input: &str, ran: &Ran) -> Option<&'static str> {
    if ran.success || positions(input) == Positions::Renamed {
        return None;
    }
    let paths = plain_imports(input)?;
    let stderr = String::from_utf8_lossy(&ran.stderr);
    let mut reason = None;
    for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
        match go_line(line, &paths)? {
            GoLine::Load => reason = Some(VET_OFFLINE),
            GoLine::Excluded => reason = reason.or(Some(VET_EXCLUDED)),
            GoLine::Note => {}
        }
    }
    reason
}

/// One line of `go`'s own that says it didn't check the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoLine {
    /// It couldn't load an import the block declares.
    Load,
    /// The block's build constraints leave its one file out.
    Excluded,
    /// It went looking for an import the block declares, or says how to add it.
    Note,
}

/// What `line`, a whole line of `go`'s stderr, says about an import in `paths` (the block's) or
/// the block's file, as `go` itself prints it. `None` for any other line.
fn go_line(line: &str, paths: &[&str]) -> Option<GoLine> {
    let declared = |path: &str| paths.contains(&path);
    if line.strip_prefix(EXCLUDED) == Some(".") {
        return Some(GoLine::Excluded);
    }
    if let Some(path) = line
        .strip_prefix("go: finding module for package ")
        .or_else(|| line.strip_prefix("\tgo get "))
    {
        return declared(path).then_some(GoLine::Note);
    }
    let (posn, message) = line.split_once(": ")?;
    let (file, ..) = position(posn)?;
    (file == MAIN && posn.starts_with(MAIN) && unloadable(message).is_some_and(declared))
        .then_some(GoLine::Load)
}

/// The text of `rest` before `end`, when `end` is in it.
fn before<'text>(rest: &'text str, end: &str) -> Option<&'text str> {
    rest.split_once(end).map(|(path, _)| path)
}

/// The import `message`, at an import in the block's file, says `go` can't load: a module it
/// would have to look up, a path in no module and not in the standard library, or a relative
/// path. `None` for any other message.
fn unloadable(message: &str) -> Option<&str> {
    if let Some(rest) = message.strip_prefix("cannot find module providing package ") {
        return Some(before(rest, ": ").unwrap_or(rest));
    }
    if let Some(rest) = message.strip_prefix("no required module provides package ") {
        return before(rest, ";");
    }
    if let Some(rest) = message.strip_prefix("package ") {
        return before(rest, " is not in std");
    }
    if let Some(rest) = message.strip_prefix("local import \"") {
        return before(rest, "\" in non-local package");
    }
    let rest = message.strip_prefix('"')?;
    before(
        rest,
        "\" is relative, but relative import paths are not supported",
    )
}

/// The import paths `input`'s import declarations name, each as written between its quotes, when
/// they read plainly: the package clause, then import declarations, each path an interpreted
/// string with no escape or control character or a raw string with no control character. `None`
/// otherwise, and for a comment or literal left open, since a path `go` prints raw could then
/// write a line of its own.
fn plain_imports(input: &str) -> Option<Vec<&str>> {
    let mut tokens = Lexer { rest: input };
    if tokens.next()? != Token::Word("package") || !matches!(tokens.next()?, Token::Word(_)) {
        return None;
    }
    let mut paths = Vec::new();
    loop {
        match tokens.next() {
            Some(Token::Punct(';')) => {}
            Some(Token::Word("import")) => match tokens.next()? {
                Token::Punct('(') => loop {
                    match tokens.next()? {
                        Token::Punct(')') => break,
                        Token::Punct(';') => {}
                        first => paths.push(spec(first, &mut tokens)?),
                    }
                },
                first => paths.push(spec(first, &mut tokens)?),
            },
            Some(Token::Unplain) => return None,
            _ => return Some(paths),
        }
    }
}

/// The path of the import spec opening with `first`: a path, or a name, `.` or `_` and then a
/// path. `None` for any other spec.
fn spec<'text>(first: Token<'text>, tokens: &mut Lexer<'text>) -> Option<&'text str> {
    match first {
        Token::Path(path) => Some(path),
        Token::Word(_) | Token::Punct('.') => match tokens.next()? {
            Token::Path(path) => Some(path),
            _ => None,
        },
        _ => None,
    }
}

/// One token of the start of a Go file, as far as its import declarations go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'text> {
    /// An identifier or keyword.
    Word(&'text str),
    /// A string literal that reads plainly, between its quotes.
    Path(&'text str),
    /// Any other character.
    Punct(char),
    /// A literal or comment left open, or a literal that doesn't read plainly.
    Unplain,
}

/// The tokens of a Go file's start, comments skipped.
struct Lexer<'text> {
    /// The text not yet read.
    rest: &'text str,
}

impl<'text> Lexer<'text> {
    /// The next token, past whitespace and comments. `None` at the end.
    fn next(&mut self) -> Option<Token<'text>> {
        loop {
            self.rest = self.rest.trim_start();
            if let Some(comment) = self.rest.strip_prefix("//") {
                self.rest = comment.split_once('\n').map_or("", |(_, after)| after);
            } else if let Some(comment) = self.rest.strip_prefix("/*") {
                let Some((_, after)) = comment.split_once("*/") else {
                    self.rest = "";
                    return Some(Token::Unplain);
                };
                self.rest = after;
            } else {
                break;
            }
        }
        let mut chars = self.rest.chars();
        let first = chars.next()?;
        if first == '"' || first == '`' {
            let Some((path, after)) = chars.as_str().split_once(first) else {
                self.rest = "";
                return Some(Token::Unplain);
            };
            self.rest = after;
            let plain =
                !path.chars().any(char::is_control) && (first == '`' || !path.contains('\\'));
            return Some(if plain {
                Token::Path(path)
            } else {
                Token::Unplain
            });
        }
        if first.is_alphanumeric() || first == '_' {
            let end = self
                .rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(self.rest.len());
            let (word, after) = self.rest.split_at(end);
            self.rest = after;
            return Some(Token::Word(word));
        }
        self.rest = chars.as_str();
        Some(Token::Punct(first))
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

/// The errors `go vet` printed for the block, each `[vet: ]<file>:<line>[:<column>]: <message>`
/// after any indent, a syntax error's marked so, and its `(and N more errors)` dropped. Without
/// a directive only the block's own file counts; after a `//line` or `/*line` directive, which
/// renames positions to any file it names, every error sits at the fence.
fn errors(stderr: &str, renamed: bool) -> Vec<Diagnostic> {
    stderr
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let line = line.strip_prefix("vet: ").unwrap_or(line);
            let (posn, message) = line.split_once(": ")?;
            let (file, ..) = position(posn)?;
            if !renamed && file != MAIN {
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
        GoVersion, VET_EXCLUDED, VET_OFFLINE, is_file, parse_error, positions, vet_not_run,
        vet_verdict,
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
    fn a_release_is_go1_minor_or_go1_minor_patch_and_nothing_else() {
        assert!(GoVersion::parse("go1.27.3").is_some());
        assert!(GoVersion::parse("go1.27").is_some());
        for refused in [
            "",
            "go1",
            "go1.",
            "go1.27.",
            "go1.27.3.1",
            "go1.27rc1",
            "devel go1.28-abc",
            "go1.27.3 X:boringcrypto",
            "go2.0",
            "go1.-1",
            "go1.27\nx",
            " go1.27",
        ] {
            assert_eq!(GoVersion::parse(refused), None, "`{refused}`");
        }
    }

    #[test]
    fn the_go_line_is_the_releases_language_version_without_its_patch() {
        let go_mod = |text| GoVersion::parse(text).map(|release| release.go_mod());

        for (release, line) in [
            ("go1.27.3", "1.27"),
            ("go1.27", "1.27"),
            ("go1.20.5", "1.20"),
            ("go1.9", "1.9"),
        ] {
            assert_eq!(
                go_mod(release),
                Some(format!("module supdoc.invalid/block\n\ngo {line}\n")),
                "`{release}`"
            );
        }
    }

    #[test]
    fn two_releases_have_different_fingerprints() {
        let fingerprint = |text| GoVersion::parse(text).map(|release| release.fingerprint());
        assert_ne!(fingerprint("go1.27.3"), fingerprint("go1.27.2"));
    }

    #[test]
    fn a_type_checkers_line_quoting_gos_words_is_never_a_reason_not_to_run() {
        for words in [
            "is not in std",
            "cannot find module providing package x",
            "package supdoc.invalid/block: build constraints exclude all Go files in .",
        ] {
            let stderr = format!(
                "# supdoc.invalid/block\nvet: ./main.go:3:13: cannot use \"{words}\" as int value\n"
            );
            assert_eq!(
                vet_not_run(FILE, &ran(false, "", &stderr)),
                None,
                "`{words}`"
            );
        }
    }

    #[test]
    fn an_indented_error_and_one_renamed_to_any_file_are_read() {
        let internal = "package supdoc.invalid/block\n\tmain.go:3:8: use of internal package \
                        internal/abi not allowed\n";
        let renamed = "# m\nvet: /etc/passwd:3: cannot use \"s\" as int value\n";
        let lines =
            |input: &str, stderr: &str| match vet_verdict(input, false, Ok(ran(false, "", stderr)))
            {
                Verdict::Diagnostics(found) => found
                    .iter()
                    .map(|d| (d.line, d.rule.clone()))
                    .collect::<Vec<_>>(),
                other => panic!("the run should give diagnostics: {other:?}"),
            };

        assert_eq!(lines(FILE, internal), [(3, "typecheck".to_owned())]);
        assert_eq!(
            lines("package p\n//line /etc/passwd:1\n", renamed),
            [(0, "typecheck".to_owned())]
        );
    }

    #[test]
    fn an_unloadable_import_or_excluded_file_is_not_run_and_any_other_failure_is_judged() {
        let offline = "main.go:3:8: cannot find module providing package example.com/x: module \
                       lookup disabled by GOPROXY=off\n";
        let unknown = "main.go:3:8: package foo/bar is not in std (/go/src/foo/bar)\n";
        let excluded =
            "package supdoc.invalid/block: build constraints exclude all Go files in .\n";

        let relative = "main.go:5:2: \"./x\" is relative, but relative import paths are not \
                        supported in module mode\nmain.go:5:2: local import \"./x\" in non-local \
                        package\n";

        assert_eq!(
            vet_not_run(IMPORTS, &ran(false, "", offline)),
            Some(VET_OFFLINE)
        );
        assert_eq!(
            vet_not_run(IMPORTS, &ran(false, "", unknown)),
            Some(VET_OFFLINE)
        );
        assert_eq!(
            vet_not_run(IMPORTS, &ran(false, "", relative)),
            Some(VET_OFFLINE)
        );
        assert_eq!(
            vet_not_run(FILE, &ran(false, "", excluded)),
            Some(VET_EXCLUDED)
        );
        assert_eq!(
            vet_not_run(IMPORTS, &ran(false, "", "main.go:1:1: expected x\n")),
            None
        );
        assert_eq!(vet_not_run(FILE, &ran(true, "{}", excluded)), None);
    }

    /// A file importing three packages `go` can't load offline.
    const IMPORTS: &str = "// Package p.\npackage p\n\nimport \"example.com/x\"\n\nimport (\n\t\
                           bar \"foo/bar\" // a comment\n\t_ `./x`\n)\n\nvar s = \"a\\nb\"\n";

    #[test]
    fn a_load_line_beside_any_other_error_line_is_a_failure_not_a_reason_not_to_run() {
        let mixed = "main.go:3:8: invalid import path: a\nmain.go:1:1: cannot find module providing \
                     package example.com/x\n";

        assert_eq!(vet_not_run(IMPORTS, &ran(false, "", mixed)), None);
    }

    #[test]
    fn a_load_line_for_an_import_the_block_doesnt_declare_is_a_failure() {
        let other = "main.go:3:8: cannot find module providing package example.com/y: x\n";

        assert_eq!(vet_not_run(IMPORTS, &ran(false, "", other)), None);
    }

    #[test]
    fn a_block_whose_text_could_write_gos_lines_is_never_not_run() {
        let load = "main.go:3:8: cannot find module providing package b\n";
        for input in [
            "package p\n//line main.go:1\nimport \"b\"\n",
            "package p\n\nimport _ \"a\\nmain.go:3:8: cannot find module providing package b\"\nimport \"b\"\n",
            "package p\n\nimport _ `a\nb`\nimport \"b\"\n",
            "package p\n\nimport \"b\"\nimport \"c",
            "package p\n\nimport \"b\" /* open",
            "package p\n\nimport \"b\u{1b}\"\nimport \"b\"\n",
        ] {
            assert_eq!(vet_not_run(input, &ran(false, "", load)), None, "{input:?}");
        }
        assert_eq!(
            vet_not_run("package p\n\nimport \"b\"\n", &ran(false, "", load)),
            Some(VET_OFFLINE)
        );
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
