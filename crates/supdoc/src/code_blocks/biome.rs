//! biome: the extension a tag selects, its syntax-error report, its lint diagnostics, the
//! project config supdoc finds for it, and the private file a block is linted from.
//!
//! biome finds its config from the directory it runs in, never from the block's virtual path,
//! and supdoc runs it from an empty directory of its own. So supdoc walks up from the doc inside
//! the root, passes the config it finds by `--config-path`, and refuses one that could make
//! biome load code the checkout chose: an `extends` (a package from `node_modules`, or any other
//! file whose content the result cache doesn't key on) or a `plugins` list. With no config
//! found, biome gets a supdoc-owned empty one, so a config in the home directory never applies.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use super::format::{self, ParseError};
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;

/// The config names biome looks for in a directory, in its own order.
const CONFIG_FILES: [&str; 4] = ["biome.json", "biome.jsonc", ".biome.json", ".biome.jsonc"];

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 64 * 1024;

/// The content of the config biome gets when the project has none.
const EMPTY_CONFIG: &str = "{}";

/// The file extension biome reads a block tagged `tag` by: the tag itself when it names a
/// module or JSX flavor (`mts`, `cts`, `tsx`, `mjs`, `cjs`, `jsx`), else `ts` or `js`.
#[must_use]
pub(crate) fn extension(tag: &str) -> &'static str {
    match tag {
        "mts" => "mts",
        "cts" => "cts",
        "tsx" => "tsx",
        "mjs" => "mjs",
        "cjs" => "cjs",
        "jsx" => "jsx",
        "js" | "javascript" => "js",
        _ => "ts",
    }
}

/// `biome format`'s syntax-error report in `stderr`: a header line `<path>:L:C parse ━━━` whose
/// path is a block's virtual path (it ends `.<index>.<extension>`). A header naming any other
/// file, such as a config, is not a parse error of the block.
#[must_use]
pub(crate) fn parse_error(stderr: &str) -> Option<ParseError> {
    let header = stderr.lines().find(|line| line.contains(" parse ━"))?;
    let (place, _) = header.split_once(" parse ")?;
    let mut parts = place.rsplitn(3, ':');
    let _column = parts.next()?.parse::<usize>().ok()?;
    let line = parts.next()?.parse::<usize>().ok()?;
    let path = parts.next()?;
    let (rest, extension) = path.rsplit_once('.')?;
    let (_, index) = rest.rsplit_once('.')?;
    let virtual_path = index.parse::<usize>().is_ok()
        && ["ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx"].contains(&extension);
    virtual_path.then_some(ParseError {
        line: line.checked_sub(1),
    })
}

/// One diagnostic of `biome lint --reporter=json`.
#[derive(serde::Deserialize)]
struct BiomeDiagnostic {
    /// Its rule, as `lint/<group>/<rule>`, a suppression comment's problem, as
    /// `suppressions/<name>`, or a category that names neither: `parse` for a syntax error, or a
    /// bare `lint` for a file biome didn't lint, such as one over `files.maxSize`.
    category: String,
    /// What biome says.
    message: String,
    /// Where it starts.
    location: BiomeLocation,
}

/// A diagnostic's place in biome's JSON.
#[derive(serde::Deserialize)]
struct BiomeLocation {
    /// Its first character.
    start: BiomePosition,
}

/// A 1-based line and column in biome's JSON.
#[derive(serde::Deserialize)]
struct BiomePosition {
    /// The line.
    line: usize,
    /// The column.
    column: usize,
}

/// The report `biome lint --reporter=json` prints on stdout.
#[derive(serde::Deserialize)]
struct BiomeReport {
    /// What the run did.
    summary: BiomeSummary,
    /// Every diagnostic, none held back under `--max-diagnostics=none`.
    diagnostics: Vec<BiomeDiagnostic>,
}

/// The counts in a report's summary that say whether biome looked at the block at all.
#[derive(serde::Deserialize)]
struct BiomeSummary {
    /// Files it checked and would change.
    changed: u64,
    /// Files it checked and would leave alone.
    unchanged: u64,
}

/// The diagnostics in `biome lint --reporter=json`'s `stdout`, at every severity: each rule's
/// named without the `lint/` prefix (`suspicious/noDebugger`), and each about a suppression
/// comment in the block named by its whole category (`suppressions/unused`). `None` when it
/// isn't biome's JSON report.
///
/// # Errors
///
/// What biome said, when a diagnostic's category names neither a rule nor a suppression: biome
/// reporting on the run, such as a file it skipped, never a finding in the block.
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Result<Vec<Diagnostic>, String>> {
    let report: BiomeReport = serde_json::from_slice(stdout).ok()?;
    Some(
        report
            .diagnostics
            .into_iter()
            .map(|diagnostic| {
                let category = &diagnostic.category;
                let rule = category
                    .strip_prefix("lint/")
                    .filter(|rule| !rule.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        category
                            .strip_prefix("suppressions/")
                            .is_some_and(|name| !name.is_empty())
                            .then(|| category.clone())
                    });
                rule.map(|rule| Diagnostic {
                    line: diagnostic.location.start.line,
                    column: diagnostic.location.start.column,
                    rule,
                    message: diagnostic.message.clone(),
                })
                .ok_or_else(|| {
                    format!(
                        "biome lint reported a `{}` diagnostic that names no rule: {}",
                        diagnostic.category, diagnostic.message
                    )
                })
            })
            .collect(),
    )
}

/// Judges a `biome lint` run on its JSON report, not its exit: any rule's or suppression's
/// diagnostic, error, warning or info alike, fails the block, as clippy's warnings fail a Rust
/// block. A report with none is clean on a zero exit; a diagnostic that names neither, a
/// non-zero exit without
/// diagnostics, or output that isn't the report, is a tool failure; a timeout is a timeout.
/// `input` isn't needed: biome counts columns in characters.
#[must_use]
pub(crate) fn lint_verdict(_input: &str, result: Result<Ran, Failure>) -> lint::Verdict {
    let ran = match result {
        Ok(ran) => ran,
        Err(failure) => return lint::Verdict::Failed(format::failed(failure)),
    };
    match diagnostics(&ran.stdout) {
        Some(Err(detail)) => lint::Verdict::Failed(format::Verdict::ToolFailed(detail)),
        Some(Ok(found)) if !found.is_empty() => lint::Verdict::Diagnostics(found),
        Some(Ok(_)) if ran.success => lint::Verdict::Clean,
        _ => lint::Verdict::Failed(format::Verdict::ToolFailed(format::exit_detail(
            &ran.stderr,
        ))),
    }
}

/// Whether `stdout`, biome lint's JSON report, says it checked no file and found nothing: the
/// block was left out by something supdoc didn't drop from the config, so the run says nothing
/// about it.
#[must_use]
pub(crate) fn linted_nothing(stdout: &[u8]) -> bool {
    serde_json::from_slice::<BiomeReport>(stdout).is_ok_and(|report| {
        report.diagnostics.is_empty() && report.summary.changed + report.summary.unchanged == 0
    })
}

/// The settings that leave files out of linting, as `(section, key)` at the config's top level:
/// biome 2's `includes` and the `include` and `ignore` spellings older configs use, and
/// `files.maxSize`, which skips a file larger than it. The lint copy drops them, so a block is
/// linted wherever the format step checks it; an override's `includes` stays, since it only
/// scopes that override's settings.
const LINT_EXCLUSIONS: [(&str, &str); 7] = [
    ("files", "includes"),
    ("files", "include"),
    ("files", "ignore"),
    ("files", "maxSize"),
    ("linter", "includes"),
    ("linter", "include"),
    ("linter", "ignore"),
];

/// `strict`, the JSON config biome is given, without [`LINT_EXCLUSIONS`] and with `vcs.enabled`
/// set to `false` when it has a `vcs` section: the block's directory is no checkout, so a VCS
/// integration has no ignore file to honor and makes biome fail. `None` when it isn't a JSON
/// object.
fn without_exclusions(strict: &str) -> Option<String> {
    let mut config: serde_json::Map<String, Value> = serde_json::from_str(strict).ok()?;
    for (section, key) in LINT_EXCLUSIONS {
        if let Some(Value::Object(settings)) = config.get_mut(section) {
            let _dropped = settings.remove(key);
        }
    }
    if let Some(Value::Object(vcs)) = config.get_mut("vcs") {
        let _previous = vcs.insert("enabled".to_owned(), Value::Bool(false));
    }
    serde_json::to_string(&config).ok()
}

/// One `biome lint` run's private directory, removed when this is dropped: a copy of the config
/// biome is given and the block written as a file, since biome lint reports nothing for a block
/// on stdin. The block sits where its virtual path sits under the config's directory, so the
/// config's relative globs match it as they would the doc's own blocks.
#[derive(Debug)]
pub(crate) struct LintFile {
    /// The directory, owned by this run alone.
    dir: PathBuf,
    /// The block's file in it.
    file: PathBuf,
    /// With a project config: the copy and this directory, with the project's config file and
    /// directory they stand for in biome's messages.
    names: Option<[(PathBuf, PathBuf); 2]>,
}

/// How many [`LintFile`] directories this process has tried to create, which makes each name
/// unique.
static LINT_RUNS: AtomicUsize = AtomicUsize::new(0);

impl LintFile {
    /// A new directory `biome-lint/<pid>-<n>` beside `neutral`, the tool's working directory,
    /// mode 0700 inside a 0700 `biome-lint` directory, holding `biome.json` (the config `given`
    /// names, without the [`LINT_EXCLUSIONS`] that would leave the block unlinted) and `input`
    /// as the `index`th block of `doc`, its extension picked by `tag`. Every file is created
    /// new, mode 0600. `None` when `neutral` is `/`, `given` names no config, or anything can't
    /// be created.
    ///
    /// Effect: reads the private config, creates directories and writes two files.
    #[must_use]
    pub(crate) fn create(
        neutral: &Path,
        given: &Given,
        (doc, index, tag): (&Path, usize, &str),
        input: &str,
    ) -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt as _;

        if neutral == Path::new("/") {
            return None;
        }
        let config = given.config.as_deref()?;
        let strict = without_exclusions(&fs::read_to_string(config).ok()?)?;
        let base = private_dir(&neutral.parent()?.join("biome-lint"))?;
        let dir = (0..16).find_map(|_| {
            let count = LINT_RUNS.fetch_add(1, Ordering::Relaxed);
            let dir = base.join(format!("{}-{count}", std::process::id()));
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .ok()
                .map(|()| dir)
        })?;
        let names = given.names.as_ref().map(|[(_, file), (_, project)]| {
            [
                (dir.join("biome.json"), file.clone()),
                (dir.clone(), project.clone()),
            ]
        });
        let mut run = Self {
            dir,
            file: PathBuf::new(),
            names,
        };
        let placed = given
            .mirror
            .as_deref()
            .zip(config.parent())
            .and_then(|(mirror, private)| mirror.strip_prefix(private).ok())
            .map_or_else(
                || PathBuf::from(doc.file_name().unwrap_or_default()),
                Path::to_path_buf,
            );
        let mut name = placed.file_name()?.to_os_string();
        name.push(format!(".{index}.{}", extension(tag)));
        let file = run.dir.join(placed.with_file_name(name));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(file.parent()?)
            .ok()?;
        write_new(&run.dir.join("biome.json"), &strict)?;
        write_new(&file, input)?;
        run.file = file;
        Some(run)
    }

    /// The block's file.
    #[must_use]
    pub(crate) fn file(&self) -> &Path {
        &self.file
    }

    /// `stderr` with this run's private paths biome printed named as the project's config file
    /// and directory they stand for, as [`Given::named`] does for the format step.
    #[must_use]
    pub(crate) fn named(&self, stderr: &[u8]) -> Vec<u8> {
        let Some(names) = &self.names else {
            return stderr.to_vec();
        };
        let mut text = String::from_utf8_lossy(stderr).into_owned();
        for (private, project) in names {
            text = text.replace(&*private.to_string_lossy(), &project.to_string_lossy());
        }
        text.into_bytes()
    }

    /// `--config-path=<the config copy>`, which biome reads instead of the shared private
    /// config, so the block is inside the config's directory.
    #[must_use]
    pub(crate) fn config_flag(&self) -> OsString {
        let mut flag = OsString::from("--config-path=");
        flag.push(self.dir.join("biome.json"));
        flag
    }
}

impl Drop for LintFile {
    fn drop(&mut self) {
        let _removed = fs::remove_dir_all(&self.dir);
    }
}

/// Writes `text` to `path`, which must not exist yet, mode 0600.
///
/// Effect: creates and writes the file.
fn write_new(path: &Path, text: &str) -> Option<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .ok()?
        .write_all(text.as_bytes())
        .ok()
}

/// Why a block isn't given to biome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's biome config, at `file` (relative to the root), can't be given to biome.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: String,
        /// What to do about it.
        help: String,
    },
    /// supdoc has no private directory to keep the config it gives biome in.
    NoPrivateDir,
}

/// What biome is given for a block: the flags, and where its doc stands in for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Given {
    /// `--config-path=<the private file>`.
    pub(crate) flags: Vec<OsString>,
    /// The private file `flags` names, which a [`LintFile`] copies. `None` for rumdl, which
    /// shares this type.
    pub(crate) config: Option<PathBuf>,
    /// With a project config: the doc's place under the private directory, mirroring its place
    /// under the config's directory, which the block's virtual path is built from so relative
    /// globs resolve as they do in the project. `None` when there is no project config.
    pub(crate) mirror: Option<PathBuf>,
    /// With a project config: how to name the private paths in biome's messages: the private
    /// file and directory, with the project's file and directory they stand for.
    pub(crate) names: Option<[(PathBuf, PathBuf); 2]>,
}

impl Given {
    /// `stderr` with each private path biome printed named as the project's path it stands for.
    #[must_use]
    pub(crate) fn named(&self, stderr: &[u8]) -> Vec<u8> {
        let Some(names) = &self.names else {
            return stderr.to_vec();
        };
        let mut text = String::from_utf8_lossy(stderr).into_owned();
        for (private, project) in names {
            text = text.replace(&*private.to_string_lossy(), &project.to_string_lossy());
        }
        text.into_bytes()
    }
}

/// What biome is given for `doc`: the project's config, found walking up from `doc`'s directory
/// to `root`, parsed and validated by supdoc and re-serialized as strict JSON into a private
/// file, so biome never reads the project's bytes. Each setting that names a file biome reads
/// is rewritten to its absolute path. With no project config the file holds `{}`.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directories and the config file in them.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable
/// JSON(C) object of at most 64 KiB, extends anything, declares plugins, or names a file
/// outside the root; [`Refusal::NoPrivateDir`] when there is nowhere private to put the file.
pub(crate) fn project_args(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Given, Refusal> {
    let found = find(root, doc);
    let strict = match &found {
        Some(found) => checked(root, found)?,
        None => EMPTY_CONFIG.to_owned(),
    };
    let config = write_private(env, root, &strict).ok_or(Refusal::NoPrivateDir)?;
    let mut flag = OsString::from("--config-path=");
    flag.push(&config);
    let private_config = config.clone();
    let project_dir = found.as_deref().and_then(Path::parent);
    let private_dir = config.parent().map(Path::to_path_buf);
    let mirror = project_dir
        .zip(private_dir.as_ref())
        .and_then(|(project, private)| Some(private.join(doc.strip_prefix(project).ok()?)));
    let names = found
        .as_deref()
        .zip(project_dir.zip(private_dir))
        .map(|(file, (dir, private))| [(config, file.to_path_buf()), (private, dir.to_path_buf())]);
    Ok(Given {
        flags: vec![flag],
        config: Some(private_config),
        mirror,
        names,
    })
}

/// The first biome config in `doc`'s directory or an ancestor of it inside `root`.
fn find(root: &Path, doc: &Path) -> Option<PathBuf> {
    doc.parent()?
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .flat_map(|dir| CONFIG_FILES.iter().map(move |name| dir.join(name)))
        .find(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// The strict JSON of the safe config at `found`, a file of the project at `root`.
///
/// # Errors
///
/// [`Refusal::Config`] naming what is wrong with it.
fn checked(root: &Path, found: &Path) -> Result<String, Refusal> {
    let file = found
        .strip_prefix(root)
        .unwrap_or(found)
        .display()
        .to_string();
    let refuse = |why: &str, help: &str| Refusal::Config {
        file: file.clone(),
        why: why.to_owned(),
        help: help.to_owned(),
    };
    let unreadable = || {
        refuse(
            "couldn't be read",
            "make it a readable UTF-8 file inside the project",
        )
    };
    let real_root = root.canonicalize().map_err(|_| unreadable())?;
    let real = found.canonicalize().map_err(|_| unreadable())?;
    if !real.starts_with(&real_root) {
        return Err(refuse(
            "is a link that resolves outside the project",
            "replace it with a regular file inside the project",
        ));
    }
    let meta = fs::metadata(&real).map_err(|_| unreadable())?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err(refuse(
            "is too large or isn't a regular file (the limit is 64 KiB)",
            "make it a regular file of at most 64 KiB",
        ));
    }
    let text = fs::read_to_string(&real).map_err(|_| unreadable())?;
    let Ok(Value::Object(config)) = serde_json::from_str(&strip_jsonc(&text)) else {
        return Err(refuse(
            "doesn't parse as a JSON or JSONC object",
            "fix its syntax so it is one JSON object",
        ));
    };
    let mut value = Value::Object(config);
    if let Some(unsafe_key) = unsafe_part(&value) {
        return Err(match unsafe_key {
            "plugins" => refuse(
                "declares `plugins`, which biome would load and run",
                "remove `plugins` from it, or mark the blocks `ignore(<why>)`",
            ),
            _ => refuse(
                "`extends` another config, which supdoc doesn't follow",
                "remove `extends` from it, or mark the blocks `ignore(<why>)`",
            ),
        });
    }
    let dir = found.parent().unwrap_or(root);
    if let Err((setting, why)) = absolutize(&mut value, "", dir, &real_root) {
        return Err(refuse(
            &format!("sets `{setting}`, which {why}"),
            &format!(
                "point `{setting}` at an existing file or directory inside the project, or mark the blocks `ignore(<why>)`"
            ),
        ));
    }
    serde_json::to_string(&value).map_err(|_| unreadable())
}

/// Rewrites each setting in `value`, found under the key `parent`, that names something biome
/// reads from disk to its absolute path, resolved from the config's directory `dir`: biome
/// 2.6.5's `tailwind.stylesheet` (a file) and `vcs.root` (a directory). Its schema holds no
/// other path-valued setting, and `formatter.useEditorconfig` reads an `.editorconfig` from
/// beside the config, which the private copy doesn't have, so it can't be honored.
///
/// # Errors
///
/// The setting's name and why it isn't usable: it resolves outside `root`, doesn't exist, or
/// asks for the `.editorconfig`.
fn absolutize(
    value: &mut Value,
    parent: &str,
    dir: &Path,
    root: &Path,
) -> Result<(), (String, &'static str)> {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                let named = matches!(
                    (parent, key.as_str()),
                    ("tailwind", "stylesheet") | ("vcs", "root")
                );
                if key == "useEditorconfig" && *inner == Value::Bool(true) {
                    return Err((
                        format!("{parent}.{key}"),
                        "loads an `.editorconfig` from beside the config, which supdoc can't give biome",
                    ));
                }
                match inner {
                    Value::String(path) if named => {
                        let real = dir.join(&*path).canonicalize().map_err(|_| {
                            (format!("{parent}.{key}"), "names a path that doesn't exist")
                        })?;
                        if !real.starts_with(root) {
                            return Err((
                                format!("{parent}.{key}"),
                                "names a path outside the project",
                            ));
                        }
                        *path = real.to_string_lossy().into_owned();
                    }
                    other => absolutize(other, key, dir, root)?,
                }
            }
            Ok(())
        }
        Value::Array(items) => items
            .iter_mut()
            .try_for_each(|item| absolutize(item, parent, dir, root)),
        _ => Ok(()),
    }
}

/// The first key in `value`, at any depth, biome mustn't be given: a non-empty `plugins` or
/// `extends`.
fn unsafe_part(value: &Value) -> Option<&'static str> {
    match value {
        Value::Object(map) => ["plugins", "extends"]
            .into_iter()
            .find(|key| {
                map.get(*key).is_some_and(|value| match value {
                    Value::Null => false,
                    Value::Array(items) => !items.is_empty(),
                    Value::String(name) => !name.is_empty(),
                    Value::Object(items) => !items.is_empty(),
                    _ => true,
                })
            })
            .or_else(|| map.values().find_map(unsafe_part)),
        Value::Array(items) => items.iter().find_map(unsafe_part),
        _ => None,
    }
}

/// `text`, JSON with comments and trailing commas, as plain JSON.
fn strip_jsonc(text: &str) -> String {
    without_trailing_commas(&without_comments(text))
}

/// `text` with its `//` and `/* */` comments removed.
fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    match inner {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' || skipped == '\r' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _opener = chars.next();
                let mut last = ' ';
                for skipped in chars.by_ref() {
                    if last == '*' && skipped == '/' {
                        break;
                    }
                    last = skipped;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// `text` with each comma that only precedes a closing `}` or `]` removed.
fn without_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        match c {
            '"' => {
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    match inner {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            ',' => {
                let next = chars.clone().find(|c| !c.is_whitespace());
                if matches!(next, Some('}' | ']')) {
                    let _comma = out.pop();
                }
            }
            _ => {}
        }
    }
    out
}

/// A private file holding `strict`, named by the hash of its bytes, under a `biome-config`
/// directory beside the empty working directory tools run in. `None` when there is no private
/// directory or it fails [`private_dir`].
///
/// Effect: creates the directories mode 0700 and writes the file mode 0600.
fn write_private(env: &ToolEnv, root: &Path, strict: &str) -> Option<PathBuf> {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let base = private_dir(&neutral.parent()?.join("biome-config"))?;
    let name = Sha256::digest(strict.as_bytes())
        .iter()
        .fold(String::new(), |mut name, byte| {
            let _written = write!(name, "{byte:02x}");
            name
        });
    let dir = private_dir(&base.join(name))?;
    let file = dir.join("biome.json");
    if fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file())
        && fs::read_to_string(&file).is_ok_and(|text| text == strict)
    {
        return Some(file);
    }
    let _stale = fs::remove_file(&file);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .ok()?;
    out.write_all(strict.as_bytes()).ok()?;
    Some(file)
}

/// `dir`, created mode 0700 when missing, when it is a real directory (not a link) only its
/// owner reaches.
fn private_dir(dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let meta = fs::symlink_metadata(dir).ok()?;
    (meta.is_dir() && meta.permissions().mode() & 0o777 == 0o700).then(|| dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{
        LintFile, Refusal, diagnostics, extension, lint_verdict, linted_nothing, parse_error,
        project_args, without_exclusions, write_new,
    };
    use crate::code_blocks::format::{ParseError, Verdict as FormatVerdict};
    use crate::code_blocks::lint::{Diagnostic, Verdict};
    use crate::code_blocks::runner::{self, Ran};
    use crate::code_blocks::tools::ToolEnv;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    #[test]
    fn biomes_json_report_gives_one_diagnostic_each_at_its_1_based_line_and_column() {
        let stdout = br#"{"summary":{"changed":0,"unchanged":1,"errors":2},"diagnostics":[
            {"severity":"error","message":"This is an unexpected use of the debugger statement.","category":"lint/suspicious/noDebugger","location":{"path":"/r/a.md.1.js","start":{"line":2,"column":1},"end":{"line":2,"column":10}},"advices":[]},
            {"severity":"warning","message":"Don't use a label.","category":"lint/suspicious/noConfusingLabels","location":{"path":"/r/a.md.1.js","start":{"line":3,"column":9},"end":{"line":3,"column":10}},"advices":[]}
        ],"command":"lint"}"#;

        assert_eq!(
            diagnostics(stdout),
            Some(Ok(vec![
                Diagnostic {
                    line: 2,
                    column: 1,
                    rule: "suspicious/noDebugger".to_owned(),
                    message: "This is an unexpected use of the debugger statement.".to_owned(),
                },
                Diagnostic {
                    line: 3,
                    column: 9,
                    rule: "suspicious/noConfusingLabels".to_owned(),
                    message: "Don't use a label.".to_owned(),
                },
            ]))
        );
    }

    #[test]
    fn a_suppression_comments_diagnostic_is_a_finding_named_by_its_category() {
        let unused = br#"{"summary":{"changed":0,"unchanged":1},"diagnostics":[{"severity":"warning","message":"Suppression comment has no effect.","category":"suppressions/unused","location":{"path":"x","start":{"line":1,"column":1},"end":{"line":1,"column":46}},"advices":[]}],"command":"lint"}"#;

        assert_eq!(
            diagnostics(unused),
            Some(Ok(vec![Diagnostic {
                line: 1,
                column: 1,
                rule: "suppressions/unused".to_owned(),
                message: "Suppression comment has no effect.".to_owned(),
            }]))
        );
    }

    #[test]
    fn a_diagnostic_whose_category_names_no_rule_is_a_tool_failure_never_a_finding() {
        let rule = r#"{"severity":"error","message":"This is an unexpected use of the debugger statement.","category":"lint/suspicious/noDebugger","location":{"path":"x","start":{"line":1,"column":1},"end":{"line":1,"column":10}},"advices":[]}"#;
        let too_large = r#"{"severity":"warning","message":"The size of the file is 10 B, which exceeds the configured maximum of 4 B for this project.","category":"lint","location":{"path":"x","start":{"line":0,"column":0},"end":{"line":0,"column":0}},"advices":[]}"#;

        for found in [
            too_large.to_owned(),
            too_large.replace(r#""lint""#, r#""lint/""#),
            too_large.replace(r#""lint""#, r#""parse""#),
            too_large.replace(r#""lint""#, r#""suppressions/""#),
            format!("{rule},{too_large}"),
        ] {
            let verdict = lint_verdict("", Ok(ran(true, report(&found))));

            assert!(
                matches!(
                    &verdict,
                    Verdict::Failed(FormatVerdict::ToolFailed(detail))
                        if detail.contains("names no rule") && detail.contains("exceeds")
                ),
                "{found}: {verdict:?}"
            );
        }
    }

    /// A report holding `diagnostics` (JSON array items) for one checked file.
    fn report(diagnostics: &str) -> Vec<u8> {
        format!(
            r#"{{"summary":{{"changed":0,"unchanged":1}},"diagnostics":[{diagnostics}],"command":"lint"}}"#
        )
        .into_bytes()
    }

    /// A finished run that exited `success` with `stdout`.
    fn ran(success: bool, stdout: Vec<u8>) -> Ran {
        Ran {
            success,
            stdout,
            stderr: b"boom\n".to_vec(),
        }
    }

    #[test]
    fn any_diagnostic_fails_the_block_whatever_its_severity_and_biomes_exit() {
        let info = r#"{"severity":"information","message":"This block statement doesn't serve any purpose.","category":"lint/complexity/noUselessLoneBlockStatements","location":{"path":"x","start":{"line":1,"column":1},"end":{"line":1,"column":9}},"advices":[]}"#;
        let warning = info.replace("information", "warning").replace(
            "complexity/noUselessLoneBlockStatements",
            "suspicious/noConfusingLabels",
        );

        for (exit, found) in [
            (true, info.to_owned()),
            (true, warning),
            (false, info.to_owned()),
        ] {
            let verdict = lint_verdict("", Ok(ran(exit, report(&found))));
            assert!(
                matches!(&verdict, Verdict::Diagnostics(listed) if listed.len() == 1),
                "exit ok {exit}: {verdict:?}"
            );
        }
    }

    #[test]
    fn a_report_with_no_diagnostics_is_clean_on_a_zero_exit_and_a_tool_failure_otherwise() {
        assert_eq!(lint_verdict("", Ok(ran(true, report("")))), Verdict::Clean);
        for (exit, stdout) in [
            (false, report("")),
            (true, b"not json".to_vec()),
            (false, b"not json".to_vec()),
        ] {
            assert!(
                matches!(
                    lint_verdict("", Ok(ran(exit, stdout.clone()))),
                    Verdict::Failed(FormatVerdict::ToolFailed(_))
                ),
                "exit ok {exit}, stdout {stdout:?}"
            );
        }
    }

    #[test]
    fn a_report_that_checked_no_file_and_found_nothing_linted_nothing() {
        let nothing = br#"{"summary":{"changed":0,"unchanged":0,"skipped":0},"diagnostics":[],"command":"lint"}"#;

        assert!(linted_nothing(nothing));
        assert!(!linted_nothing(&report("")), "one file was checked");
        assert!(!linted_nothing(b"not json"));
    }

    #[test]
    fn the_lint_copy_drops_top_level_exclusions_and_keeps_an_overrides_includes() {
        let strict = r#"{"files":{"includes":["src/**"],"ignore":["x"],"maxSize":9,"ignoreUnknown":true},"linter":{"includes":["src/**"],"enabled":true},"overrides":[{"includes":["docs/**"],"linter":{"enabled":false}}]}"#;

        assert_eq!(
            without_exclusions(strict).as_deref(),
            Some(
                r#"{"files":{"ignoreUnknown":true},"linter":{"enabled":true},"overrides":[{"includes":["docs/**"],"linter":{"enabled":false}}]}"#
            )
        );
    }

    #[test]
    fn the_lint_copy_turns_a_vcs_integration_off_and_adds_none_where_there_was_none() {
        let vcs = r#"{"vcs":{"enabled":true,"clientKind":"git","useIgnoreFile":true,"root":"/r"}}"#;

        assert_eq!(
            without_exclusions(vcs).as_deref(),
            Some(
                r#"{"vcs":{"clientKind":"git","enabled":false,"root":"/r","useIgnoreFile":true}}"#
            )
        );
        assert_eq!(
            without_exclusions(r#"{"vcs":{}}"#).as_deref(),
            Some(r#"{"vcs":{"enabled":false}}"#)
        );
        assert_eq!(without_exclusions("{}").as_deref(), Some("{}"));
    }

    #[test]
    fn a_run_file_already_there_or_planted_as_a_link_is_refused_and_left_untouched() {
        let dir = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let existing = dir.path().join("a.md.1.ts");
        let target = dir.path().join("target");
        let link = dir.path().join("b.md.1.ts");
        fs::write(&existing, "kept").expect("the planted file");
        fs::write(&target, "kept").expect("the link's target");
        std::os::unix::fs::symlink(&target, &link).expect("the planted link");

        assert_eq!(write_new(&existing, "block"), None);
        assert_eq!(write_new(&link, "block"), None);
        assert_eq!(fs::read_to_string(&existing).ok().as_deref(), Some("kept"));
        assert_eq!(fs::read_to_string(&target).ok().as_deref(), Some("kept"));
    }

    #[test]
    fn output_that_isnt_biomes_json_report_has_no_diagnostics() {
        assert_eq!(diagnostics(b"The contents aren't fixed.\n"), None);
        assert_eq!(diagnostics(b"[]"), None);
    }

    #[test]
    fn a_lint_file_mirrors_the_docs_place_under_a_private_config_copy_and_is_removed_when_dropped()
    {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("pkg/docs")).expect("the fixture dirs");
        fs::write(root.path().join("pkg/biome.json"), "{\"linter\":{}}").expect("the config");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));
        let doc = root.path().join("pkg/docs/a.md");
        let given = project_args(&env, root.path(), &doc).expect("a plain config is given");
        let neutral = runner::neutral_dir(&env, root.path());
        let mirror = given
            .mirror
            .clone()
            .expect("a project config gives a mirror");

        let run = LintFile::create(&neutral, &given, (&mirror, 3, "tsx"), "let a = 1;\n")
            .expect("the run's private directory is creatable");

        let file = run.file().to_path_buf();
        let dir = file
            .parent()
            .and_then(Path::parent)
            .expect("the block sits one directory down")
            .to_path_buf();
        assert!(file.ends_with("docs/a.md.3.tsx"), "{file:?}");
        assert!(!file.starts_with(root.path()), "{file:?}");
        assert_eq!(
            fs::read_to_string(&file).ok().as_deref(),
            Some("let a = 1;\n")
        );
        assert_eq!(
            fs::read_to_string(dir.join("biome.json")).ok().as_deref(),
            Some("{\"linter\":{}}")
        );
        let mode = |path: &Path| {
            fs::symlink_metadata(path)
                .map(|meta| meta.permissions().mode() & 0o777)
                .ok()
        };
        assert_eq!(mode(&dir), Some(0o700));
        assert_eq!(mode(&file), Some(0o600));
        let mut flag = std::ffi::OsString::from("--config-path=");
        flag.push(dir.join("biome.json"));
        assert_eq!(run.config_flag(), flag);
        drop(run);
        assert!(!dir.exists(), "the run's directory should be gone: {dir:?}");
    }

    #[test]
    fn with_no_project_config_the_lint_file_sits_beside_the_empty_config() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));
        let doc = root.path().join("docs/a.md");
        let given = project_args(&env, root.path(), &doc).expect("no config gives an empty one");
        let neutral = runner::neutral_dir(&env, root.path());

        let run = LintFile::create(&neutral, &given, (&doc, 1, "js"), "f();\n")
            .expect("the run's private directory is creatable");

        let file = run.file();
        assert!(file.ends_with("a.md.1.js"), "{file:?}");
        assert_eq!(
            fs::read_to_string(file.with_file_name("biome.json"))
                .ok()
                .as_deref(),
            Some("{}")
        );
    }

    #[test]
    fn a_tag_picks_the_extension_biome_reads_the_block_by() {
        let picked: Vec<_> = [
            "ts",
            "typescript",
            "mts",
            "cts",
            "tsx",
            "js",
            "javascript",
            "mjs",
            "cjs",
            "jsx",
        ]
        .map(extension)
        .into();

        assert_eq!(
            picked,
            [
                "ts", "ts", "mts", "cts", "tsx", "js", "js", "mjs", "cjs", "jsx"
            ]
        );
    }

    #[test]
    fn a_parse_header_names_the_zero_based_body_line() {
        let stderr = "/x/doc.md.1.ts:3:11 parse ━━━━━━━━\n\n  × Expected an expression\n";

        assert_eq!(parse_error(stderr), Some(ParseError { line: Some(2) }));
    }

    #[test]
    fn other_stderr_is_no_parse_error() {
        assert_eq!(parse_error("biome.json configuration ━━━\n  × bad\n"), None);
    }

    /// The config supdoc gives biome for a root holding `config`, as a project at `root` with
    /// `HOME` set to a directory outside it.
    fn given(config: &str) -> Result<String, Refusal> {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(root.path().join("biome.jsonc"), config).expect("the config");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));
        let args = project_args(&env, root.path(), &root.path().join("a.md"))?;
        let flag = args
            .flags
            .first()
            .expect("one flag")
            .to_string_lossy()
            .into_owned();
        let path = flag.strip_prefix("--config-path=").expect("the flag");
        Ok(fs::read_to_string(path).expect("the written config"))
    }

    #[test]
    fn a_config_biome_would_read_differently_is_never_given_to_biome_as_written() {
        for config in [
            "{\"formatter\":{} // x, \"plugins\": [\"./p.js\"]\r}",
            "{\"formatter\":{} // x, \"plugins\": [\"./p.js\"]\r\n}",
            "{\"formatter\":{} // x, \"plugins\": [\"./p.js\"]\u{2028}}",
            "{\"formatter\":{} /* x */, \"plugins\": [\"./p.js\"]}",
            "{\"a\": 1,\r\"plugins\": [\"./p.js\"]}",
        ] {
            match given(config) {
                Err(_) => {}
                Ok(written) => assert!(
                    !written.contains("plugins") && !written.contains("p.js"),
                    "{config:?} gave biome {written}"
                ),
            }
        }
    }

    #[test]
    fn what_biome_is_given_is_strict_json_without_comments() {
        let written = given("{ // c\n \"formatter\": {\"indentWidth\": 4,}, /* d */ }")
            .expect("a plain config is given");

        assert_eq!(written, "{\"formatter\":{\"indentWidth\":4}}");
    }

    #[test]
    fn plugins_and_extends_at_any_depth_are_refused_with_their_own_help() {
        let plugins = given("{\"overrides\": [{\"plugins\": [\"x.grit\"]}]}");
        let extends = given("{\"extends\": [\"./b.json\"]}");
        let broken = given("{\"a\": ");
        let empty = given("{\"extends\": [], \"plugins\": []}");

        let help = |refused: Result<String, Refusal>| match refused {
            Err(Refusal::Config { help, .. }) => help,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(help(plugins).contains("plugins"));
        assert!(help(extends).contains("extends"));
        assert!(help(broken).contains("syntax"));
        assert_eq!(empty, Ok("{\"extends\":[],\"plugins\":[]}".to_owned()));
    }

    #[test]
    fn a_parse_header_naming_a_config_is_not_a_block_parse_error() {
        let stderr = "/p/biome.json:2:3 parse ━━━━\n  × bad\n";

        assert_eq!(parse_error(stderr), None);
    }

    #[test]
    fn the_nearest_config_inside_the_root_is_used_and_an_unsafe_one_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(
            root.path().join("biome.json"),
            "{\"plugins\": [\"a.grit\"]}",
        )
        .expect("the root config");
        fs::write(root.path().join("docs/biome.jsonc"), "{ // ok\n}").expect("the docs config");
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));

        let passed = project_args(&env, root.path(), &root.path().join("docs/deep/a.md"));
        let refused = project_args(&env, root.path(), &root.path().join("a.md"));

        assert!(passed.is_ok(), "got {passed:?}");
        assert!(
            matches!(refused, Err(Refusal::Config { ref file, .. }) if file == "biome.json"),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_comment_ended_by_a_bare_cr_leaves_the_key_after_it_in_the_written_config() {
        let written = given("{ // c\r \"formatter\": {\"indentWidth\": 4}}")
            .expect("a plain config is given");

        assert_eq!(written, "{\"formatter\":{\"indentWidth\":4}}");
    }

    #[test]
    fn the_exploit_shape_a_cr_ended_comment_before_plugins_is_refused() {
        let refused = given("{\"formatter\":{} // x\r, \"plugins\": [\"./p.js\"]\n}");

        assert!(
            matches!(refused, Err(Refusal::Config { ref why, .. }) if why.contains("plugins")),
            "got {refused:?}"
        );
    }

    /// `project_args` for `doc` in a project holding `files`, as the project root's tempdir and
    /// the result.
    fn project_with(
        files: &[(&str, &str)],
        doc: &str,
    ) -> (tempfile::TempDir, Result<super::Given, Refusal>) {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let home = tempfile::TempDir::new().expect("a temp dir should be creatable");
        for (name, text) in files {
            let path = root.path().join(name);
            fs::create_dir_all(path.parent().expect("a parent")).expect("the fixture dirs");
            fs::write(path, text).expect("the fixture file");
        }
        let env = ToolEnv::from_vars(|name| (name == "HOME").then(|| home.path().into()));
        let given = project_args(&env, root.path(), &root.path().join(doc));
        // The home must outlive the written config's use below.
        std::mem::forget(home);
        (root, given)
    }

    #[test]
    fn a_docs_place_under_the_private_dir_mirrors_its_place_under_the_configs_dir() {
        let (_root, given) = project_with(
            &[("pkg/biome.json", "{}"), ("pkg/docs/a.md", "")],
            "pkg/docs/a.md",
        );

        let given = given.expect("a plain config is given");

        let mirror = given.mirror.expect("a project config gives a mirror");
        assert!(mirror.ends_with("docs/a.md"), "got {mirror:?}");
        assert!(!mirror.to_string_lossy().contains("pkg"), "got {mirror:?}");
    }

    #[test]
    fn a_relative_stylesheet_is_written_absolute_and_one_outside_the_project_is_refused() {
        let (root, inside) = project_with(
            &[
                (
                    "biome.json",
                    "{\"tailwind\":{\"stylesheet\":\"./docs/app.css\"}}",
                ),
                ("docs/app.css", ""),
            ],
            "docs/a.md",
        );
        let (_other, outside) = project_with(
            &[(
                "biome.json",
                "{\"tailwind\":{\"stylesheet\":\"../app.css\"}}",
            )],
            "a.md",
        );

        let flag = inside.expect("an inside stylesheet is given").flags;
        let flag = flag
            .first()
            .expect("one flag")
            .to_string_lossy()
            .into_owned();
        let written = fs::read_to_string(flag.strip_prefix("--config-path=").expect("the flag"))
            .expect("the written config");
        let css = root
            .path()
            .canonicalize()
            .expect("the root resolves")
            .join("docs/app.css");
        assert_eq!(
            written,
            format!(
                "{{\"tailwind\":{{\"stylesheet\":{:?}}}}}",
                css.display().to_string()
            )
        );
        assert!(
            matches!(outside, Err(Refusal::Config { ref why, .. }) if why.contains("tailwind.stylesheet")),
            "got {outside:?}"
        );
    }

    #[test]
    fn a_config_that_loads_an_editorconfig_is_refused_naming_the_setting() {
        let refused = given("{\"formatter\": {\"useEditorconfig\": true}}");

        assert!(
            matches!(refused, Err(Refusal::Config { ref why, .. }) if why.contains("formatter.useEditorconfig")),
            "got {refused:?}"
        );
    }

    #[test]
    fn biomes_messages_name_the_projects_config_not_the_private_copy() {
        let (root, given) = project_with(&[("biome.json", "{}")], "a.md");
        let given = given.expect("a plain config is given");
        let private = given.mirror.clone().unwrap_or_default();
        let flag = given
            .flags
            .first()
            .expect("one flag")
            .to_string_lossy()
            .into_owned();
        let config = flag
            .strip_prefix("--config-path=")
            .expect("the flag")
            .to_owned();

        let named = given.named(format!("{config}:1:2 bad\n{}", private.display()).as_bytes());

        let text = String::from_utf8(named).expect("UTF-8");
        assert!(
            text.contains(&root.path().join("biome.json").display().to_string()),
            "{text}"
        );
        assert!(!text.contains("biome-config"), "{text}");
    }

    #[test]
    fn with_no_config_and_no_private_dir_the_block_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");

        let refused = project_args(&ToolEnv::default(), root.path(), &root.path().join("a.md"));

        assert_eq!(refused.err(), Some(Refusal::NoPrivateDir));
    }
}
