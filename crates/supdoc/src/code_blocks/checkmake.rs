//! checkmake: its JSON report, its version, the config supdoc gives it, and the private file it
//! reads the block from.
//!
//! checkmake reads a Makefile only from a path it is given, never from stdin, so supdoc writes the
//! block to one file in a fresh directory of its own beside the tool's working directory, mode
//! 0700 inside a 0700 `checkmake-lint` directory, on every OS, and removes the directory after
//! the run whatever its outcome (see [`BlockFile`]).
//!
//! Without `--config`, checkmake reads `checkmake.ini` in the directory it runs in, else in the
//! home directory; a config it can't find or parse it skips silently and lints with its defaults.
//! So supdoc always passes `--config` with a private file it wrote itself. supdoc finds the
//! project's `checkmake.ini`, walking up from the doc to the root, and writes again only what it
//! reads (see [`kept`]): each rule's `disabled` switch and the rule options `maxBodyLength` and
//! `ignore`. `minphony` is always switched off, since a block is rarely a whole Makefile with the
//! `all`, `clean` and `test` targets it asks for, and a config that switches off every other rule
//! is refused: it would leave the lint step checking nothing.
//!
//! checkmake honors no directive inside the Makefile: its parser skips every comment line
//! (`parser/parser.go:99` in checkmake 0.3.2), so no block can switch a rule off.
//!
//! checkmake exits 1 both for findings and for an error, so supdoc reads its output, never its
//! exit status alone (see [`lint_verdict`]).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::format;
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;
use crate::config::confine::{self, Boundary, ReadEscape};
use crate::hash::Hash;
use crate::path::{self, AbsolutePath, Component};

/// The rules checkmake runs on a block: every rule it has but `minphony`, which supdoc always
/// switches off.
pub(crate) const RULES: [&str; 4] = [
    "maxbodylength",
    "phonydeclared",
    "timestampexpanded",
    "uniquetargets",
];

/// The rule that asks a whole Makefile for `all`, `clean` and `test` targets, which supdoc
/// switches off for every block.
const MINPHONY: &str = "minphony";

/// The sections of checkmake's config that hold its own settings, never a rule's: supdoc drops
/// them, since its flags fix the output checkmake writes.
const OWN_SECTIONS: [&str; 2] = ["default", "DEFAULT"];

/// The name of the config checkmake reads by default, and of the copy supdoc writes.
const CONFIG_FILE: &str = "checkmake.ini";

/// The directory beside the tool's working directory holding supdoc's config copies.
const CONFIG_DIR: &str = "checkmake-config";

/// The directory beside the tool's working directory holding each run's block file.
const LINT_DIR: &str = "checkmake-lint";

/// The name of the block's file in its run's directory.
const BLOCK_FILE: &str = "block.mk";

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 256 * 1024;

/// The longest line, in bytes before its `\n`, checkmake reads: a longer one stops its line
/// scanner without ending its parse, so it never returns.
pub(crate) const MAX_LINE: usize = 65_535;

/// Why checkmake's step doesn't run on a block holding a line longer than [`MAX_LINE`].
pub(crate) const LONG_LINE: &str = "the block holds a line longer than 65535 bytes, which \
                                    checkmake can't read: it never returns";

/// Whether `input` holds a line checkmake can't read (see [`MAX_LINE`]).
#[must_use]
pub(crate) fn too_long(input: &str) -> bool {
    input.split('\n').any(|line| line.len() > MAX_LINE)
}

/// One entry of `checkmake -o json`'s array.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    /// Its rule, such as `uniquetargets`.
    rule: String,
    /// What checkmake says.
    violation: String,
    /// The file checkmake read: supdoc's private one, never shown.
    #[serde(rename = "file_name")]
    _file_name: String,
    /// Its 1-based line.
    line_number: i64,
}

/// The rule checkmake reports at a variable's line, which it counts one past the variable's own:
/// a variable takes the scanner's line count (`parser/parser.go:156`, `:168` and `:197` in
/// checkmake 0.3.2), which runs one ahead of the line read (`parser/scanner.go:52`), where a rule
/// takes one less (`parser/parser.go:204`).
const VARIABLE_RULE: &str = "timestampexpanded";

/// The diagnostics in `checkmake -o json`'s `stdout`, sorted by line, then rule, then message,
/// since checkmake's own order changes from run to run, with a [`VARIABLE_RULE`] finding moved
/// back to its variable's line. `None` when it isn't that JSON array, or an entry's line isn't 1
/// or more.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let entries: Vec<Entry> = serde_json::from_slice(stdout).ok()?;
    let mut found = entries
        .into_iter()
        .map(|entry| {
            let shift = i64::from(entry.rule == VARIABLE_RULE);
            let line = usize::try_from(entry.line_number.saturating_sub(shift))
                .ok()
                .filter(|line| *line > 0)?;
            Some(Diagnostic {
                line,
                column: 1,
                end_line: line,
                syntax: false,
                rule: entry.rule,
                message: entry.violation,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    found.sort_by(|a, b| (a.line, &a.rule, &a.message).cmp(&(b.line, &b.rule, &b.message)));
    Some(found)
}

/// Judges a checkmake run by its output: a successful run that printed nothing is clean; a failed
/// run whose stdout is a non-empty JSON array of findings, and whose stderr says exactly that many
/// were found, gives them, sorted (see [`diagnostics`]). Anything else is a tool failure: checkmake
/// exits 1 for an error too, with nothing on stdout. A `minphony` finding is a tool failure as
/// well: supdoc's config switches that rule off, so checkmake didn't read it. checkmake checks no
/// fragments, so no run is framed.
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
    let stderr = String::from_utf8_lossy(&ran.stderr);
    let quiet = |bytes: &[u8]| bytes.iter().all(u8::is_ascii_whitespace);
    if ran.success {
        return if quiet(&ran.stdout) && quiet(&ran.stderr) {
            lint::Verdict::Clean
        } else {
            failed(format::exit_detail(&ran.stderr))
        };
    }
    let Some(found) = diagnostics(&ran.stdout).filter(|found| !found.is_empty()) else {
        return failed(format::exit_detail(&ran.stderr));
    };
    if found.iter().any(|diagnostic| diagnostic.rule == MINPHONY) {
        return failed(
            "checkmake reported `minphony`, which the config supdoc gave it switches off, so it \
             didn't read that config"
                .to_owned(),
        );
    }
    if stderr.trim() != format!("Error: violations found ({})", found.len()) {
        return failed(format::exit_detail(&ran.stderr));
    }
    lint::Verdict::Diagnostics(found)
}

/// The checkmake versions, as `<major>.<minor>`, supdoc's checks are verified against: supdoc
/// writes checkmake's config and reads its JSON, and another version may read or write either
/// differently. A minor is added only after the Makefile e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 1] = ["0.3"];

/// What `checkmake --version` prints before the version.
const VERSION_PREFIX: &str = "checkmake v";

/// How a checkmake whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// The version `checkmake --version` printed in `stdout` (`checkmake v<major>.<minor>.<patch>
/// built at ...`) when it is one supdoc is verified against (see [`VERIFIED`]).
///
/// # Errors
///
/// How to name the version to the user: the version it printed, or, when its output isn't a
/// version, that.
pub(crate) fn verified(stdout: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(stdout);
    let version = text
        .lines()
        .next()
        .unwrap_or_default()
        .strip_prefix(VERSION_PREFIX)
        .and_then(|rest| rest.split(' ').next())
        .filter(|version| {
            let parts: Vec<&str> = version.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
        .ok_or_else(|| UNREADABLE.to_owned())?;
    let minor = version.rsplit_once('.').map_or(version, |(minor, _)| minor);
    if VERIFIED.contains(&minor) {
        Ok(version.to_owned())
    } else {
        Err(version.to_owned())
    }
}

/// [`verified`] on what `program`, a checkmake binary, prints for `--version`, run as any tool is
/// (see [`runner::run`]) under `timeout`.
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
        name: "checkmake",
        args: &[std::ffi::OsStr::new("--version")],
        extra: &[],
        stdin: b"",
        timeout,
        cwd: runner::Cwd::Neutral,
        cap: runner::CAP,
    };
    match runner::run(&call, env, root) {
        Ok(ran) if ran.success => verified(&ran.stdout),
        _ => Err(UNREADABLE.to_owned()),
    }
}

/// Why a block isn't given to checkmake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's checkmake config, at `file` (relative to the root), can't be given to
    /// checkmake.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// supdoc has no private directory to keep checkmake's config in.
    NoPrivateDir,
}

/// What checkmake is given for the blocks of one doc directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// `--config` and the private file, always: checkmake never searches for a config.
    pub(crate) flags: Vec<OsString>,
}

/// The [`Project`] checkmake is given for `doc`: the project's `checkmake.ini`, found walking up
/// from `doc`'s directory to `root`, what supdoc reads of it (see [`kept`]) written into a private
/// file named by the hash of what is written; with no config, a private file that only switches
/// `minphony` off.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config file in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable UTF-8
/// file of at most 256 KiB, holds a line supdoc doesn't read, or switches off every rule in
/// [`RULES`] (see [`kept`]); [`Refusal::NoPrivateDir`] when there is nowhere private to put it.
pub(crate) fn project_args(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Project, Refusal> {
    let (file, text) = match candidates(root, doc).next() {
        Some(found) => {
            let file = relative(root, &found);
            let refuse = |why| Refusal::Config {
                file: file.clone(),
                why,
            };
            let text = read_text(root, &found).map_err(refuse)?;
            (Some(file.clone()), kept(&text).map_err(refuse)?)
        }
        None => (None, kept("").unwrap_or_default()),
    };
    let private = private_config(env, root, &text).ok_or(match file {
        Some(file) => Refusal::Config {
            file,
            why: "couldn't be written again into supdoc's private directory",
        },
        None => Refusal::NoPrivateDir,
    })?;
    Ok(Project {
        flags: vec![OsString::from("--config"), private.into_os_string()],
    })
}

/// `path` relative to `root`, as shown to the user.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The `checkmake.ini` files in `doc`'s directory and its ancestors, nearest first, never above
/// `root`.
fn candidates(root: &Path, doc: &Path) -> impl Iterator<Item = PathBuf> {
    let name = Component::new(CONFIG_FILE);
    let dirs: Vec<PathBuf> = doc
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.into_iter()
        .filter_map(move |dir| Some(path::beneath(&dir, name.as_ref()?)))
        .filter(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// The text of the config at `found`, which must resolve inside `root`.
///
/// # Errors
///
/// What is wrong with the file.
fn read_text(root: &Path, found: &Path) -> Result<String, &'static str> {
    let real = Boundary::resolve(root)
        .real_inside(found)
        .map_err(|escape| match escape {
            ReadEscape::Outside { .. } => "is a link that resolves outside the project",
            ReadEscape::BoundaryUnresolved | ReadEscape::Unresolved => "couldn't be resolved",
        })?;
    let meta = fs::metadata(&real).map_err(|_| "couldn't be read")?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err("isn't a regular file of at most 256 KiB");
    }
    fs::read_to_string(&real).map_err(|_| "couldn't be read as UTF-8")
}

/// The options supdoc reads in a rule's section: `disabled` in every one, and the rule's own.
fn options(rule: &str) -> &'static [&'static str] {
    match rule {
        "maxbodylength" => &["disabled", "maxBodyLength"],
        "uniquetargets" => &["disabled", "ignore"],
        _ => &["disabled"],
    }
}

/// Whether `value` is one supdoc writes again for `key`: `disabled` is `true` or `false`,
/// `maxBodyLength` is a whole number of at most nine digits, and `ignore` is a comma-separated
/// list of plain target names.
fn valid(key: &str, value: &str) -> bool {
    match key {
        "disabled" => matches!(value, "true" | "false"),
        "maxBodyLength" => {
            (1..=9).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
        }
        _ => value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b',' | b' ' | b'.' | b'_' | b'-' | b'/' | b'%')
        }),
    }
}

/// What supdoc writes as checkmake's config for the project's config `text`: `minphony` switched
/// off, then each section of a rule in [`RULES`], in order, holding its options supdoc reads (see
/// [`options`]) as `<key> = <value>`. The `default` section and keys before any section hold
/// checkmake's own settings and are dropped, as is a `minphony` section.
///
/// # Errors
///
/// Why supdoc doesn't read the config: a carriage return that doesn't end a line; an indented
/// line; a section that isn't a rule's or checkmake's own; a line in a rule's section that isn't
/// `<option> = <value>` with an option supdoc reads and a value it accepts (see [`valid`]); a
/// section or option written twice; or every rule in [`RULES`] switched off.
fn kept(text: &str) -> Result<String, &'static str> {
    let text = text.replace("\r\n", "\n");
    if text.contains('\r') {
        return Err(
            "holds a carriage return that doesn't end a line: end each line with `\\n` \
                    or `\\r\\n`",
        );
    }
    let mut sections: Vec<(&str, Vec<(&str, &str)>)> = Vec::new();
    // The rule section lines go to: `None` before any section and in a dropped one.
    let mut current: Option<usize> = None;
    let mut seen: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with([';', '#']) {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(
                "holds an indented line, which checkmake may read as part of the line \
                        before: start each line at the start of the line",
            );
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            if seen.contains(&name) {
                return Err("names a section twice: write each section once");
            }
            seen.push(name);
            current = if RULES.contains(&name) {
                sections.push((name, Vec::new()));
                Some(sections.len() - 1)
            } else if name == MINPHONY || OWN_SECTIONS.contains(&name) {
                None
            } else {
                return Err(
                    "names a section that isn't a checkmake rule: name each section \
                            `[maxbodylength]`, `[minphony]`, `[phonydeclared]`, \
                            `[timestampexpanded]`, `[uniquetargets]` or `[default]`",
                );
            };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("holds a line that is neither a section nor `<option> = <value>`");
        };
        let (key, value) = (key.trim(), value.trim());
        let Some(index) = current else {
            continue;
        };
        let Some((rule, written)) = sections.get_mut(index) else {
            continue;
        };
        if !options(rule).contains(&key) || !valid(key, value) {
            return Err(
                "holds an option supdoc doesn't read, or a value it doesn't accept: \
                        write `disabled = true` or `false`, `maxBodyLength = <whole number>` and \
                        `ignore = <targets, comma-separated>`",
            );
        }
        if written.iter().any(|(known, _)| *known == key) {
            return Err("sets an option twice in one section: set each option once");
        }
        written.push((key, value));
    }
    let off = |rule: &str| {
        sections
            .iter()
            .any(|(name, written)| *name == rule && written.contains(&("disabled", "true")))
    };
    if RULES.iter().all(|rule| off(rule)) {
        return Err(
            "switches off every checkmake rule supdoc runs on a block, so their lint \
                    steps would check nothing: leave at least one rule on, or mark the blocks \
                    `lint(off: <reason>)`",
        );
    }
    let mut copy = format!("[{MINPHONY}]\ndisabled = true\n");
    for (rule, written) in &sections {
        copy.push_str("\n[");
        copy.push_str(rule);
        copy.push_str("]\n");
        for (key, value) in written {
            copy.push_str(key);
            copy.push_str(" = ");
            copy.push_str(value);
            copy.push('\n');
        }
    }
    Ok(copy)
}

/// Writes `text` into `checkmake.ini` in a private directory named by the hash of `text`, under
/// supdoc's `checkmake-config` directory, and returns the file's path, as hadolint's copy is
/// written.
///
/// Effect: creates the directories mode 0700 and the file mode 0600, and may replace a stale one.
///
/// `None` when there is no private directory or the file can't be written.
fn private_config(env: &ToolEnv, root: &Path, text: &str) -> Option<PathBuf> {
    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return None;
    }
    let cache = runner::exact(neutral.parent()?)?;
    let parent = cache.beneath(&Component::new(CONFIG_DIR)?);
    let digest = Component::new(&Hash::digest(text.as_bytes()).to_string())?;
    let dir = parent.beneath(&digest);
    let file = dir.beneath(&Component::new(CONFIG_FILE)?);
    let parent_boundary = runner::create_private(&Boundary::resolve(&cache), &parent)?;
    let own = runner::create_private(&parent_boundary, &dir)?;
    runner::write_private(&own, &file, text)?;
    Some(file.as_path().to_path_buf())
}

/// One checkmake run's private directory, removed when this is dropped, holding the block as
/// `block.mk`: checkmake reads only a path, never stdin.
#[derive(Debug)]
pub(crate) struct BlockFile {
    /// The `checkmake-lint` directory `dir` sits in, which removing `dir` is admitted against.
    base: Boundary,
    /// The directory, owned by this run alone.
    dir: AbsolutePath,
    /// The block's file in it.
    file: AbsolutePath,
}

/// How many [`BlockFile`] directories this process has tried to create, which makes each name
/// unique.
static RUNS: AtomicUsize = AtomicUsize::new(0);

impl BlockFile {
    /// A new directory `checkmake-lint/<pid>-<n>` beside `neutral`, the tool's working directory,
    /// mode 0700 inside a 0700 `checkmake-lint` directory, holding `input` as `block.mk`, created
    /// new, mode 0600. `None` when `neutral` is `/` or anything can't be created.
    ///
    /// Effect: creates the directories and writes the file.
    #[must_use]
    pub(crate) fn create(neutral: &Path, input: &str) -> Option<Self> {
        use std::io::Write as _;

        if neutral == Path::new("/") {
            return None;
        }
        let cache = runner::exact(neutral.parent()?)?;
        let base_dir = cache.beneath(&Component::new(LINT_DIR)?);
        let base = runner::create_private(&Boundary::resolve(&cache), &base_dir)?;
        let dir = (0..16).find_map(|_| {
            let count = RUNS.fetch_add(1, Ordering::Relaxed);
            let dir =
                base_dir.beneath(&Component::new(&format!("{}-{count}", std::process::id()))?);
            confine::create_dir(&base.admit(&dir).ok()?, 0o700)
                .ok()
                .map(|()| dir)
        })?;
        let run = Self {
            file: dir.beneath(&Component::new(BLOCK_FILE)?),
            base,
            dir,
        };
        confine::create_new(&Boundary::resolve(&run.dir).admit(&run.file).ok()?, 0o600)
            .ok()?
            .write_all(input.as_bytes())
            .ok()?;
        Some(run)
    }

    /// The block's file.
    #[must_use]
    pub(crate) fn file(&self) -> &Path {
        self.file.as_path()
    }

    /// `stderr` with the block's private path checkmake printed named as `block`, the block's
    /// virtual path.
    #[must_use]
    pub(crate) fn named(&self, stderr: &[u8], block: &std::ffi::OsStr) -> Vec<u8> {
        String::from_utf8_lossy(stderr)
            .replace(
                &*self.file.as_path().to_string_lossy(),
                &block.to_string_lossy(),
            )
            .into_bytes()
    }
}

impl Drop for BlockFile {
    /// Removes the run's directory, once it is admitted beneath `checkmake-lint`.
    fn drop(&mut self) {
        if let Ok(dir) = self.base.admit(&self.dir) {
            let _removed = confine::remove_dir_all(&dir);
        }
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

    /// One finding of checkmake's JSON under `rule` at `line`.
    fn finding(rule: &str, line: i64) -> String {
        format!(
            r#"{{"rule":"{rule}","violation":"v {rule}","file_name":"/p/block.mk","line_number":{line}}}"#
        )
    }

    #[test]
    fn findings_with_their_count_on_stderr_are_diagnostics_sorted_by_line_then_rule() {
        let stdout = format!(
            "[{},{},{}]",
            finding("uniquetargets", 5),
            finding("phonydeclared", 2),
            finding("maxbodylength", 5)
        );

        let verdict = lint_verdict(
            "",
            false,
            Ok(ran(false, &stdout, "Error: violations found (3)\n")),
        );

        let lint::Verdict::Diagnostics(found) = verdict else {
            panic!("findings should be diagnostics, got {verdict:?}");
        };
        let order: Vec<(usize, &str)> = found
            .iter()
            .map(|diagnostic| (diagnostic.line, diagnostic.rule.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                (2, "phonydeclared"),
                (5, "maxbodylength"),
                (5, "uniquetargets")
            ]
        );
    }

    #[test]
    fn only_a_quiet_successful_run_is_clean_and_an_error_exit_without_the_array_fails() {
        let failure = |verdict: &lint::Verdict| {
            matches!(
                verdict,
                lint::Verdict::Failed(format::Verdict::ToolFailed(_))
            )
        };

        assert_eq!(
            lint_verdict("", false, Ok(ran(true, "", ""))),
            lint::Verdict::Clean
        );
        for (success, stdout, stderr) in [
            (false, "", "Error: failed to parse \"x\""),
            (false, "[]", "Error: violations found (0)"),
            (false, "RULE  DESCRIPTION", "Error: violations found (1)"),
            (true, "Usage: checkmake", ""),
            (true, "", "something"),
        ] {
            let verdict = lint_verdict("", false, Ok(ran(success, stdout, stderr)));
            assert!(
                failure(&verdict),
                "{success} {stdout:?} {stderr:?} gave {verdict:?}"
            );
        }
    }

    #[test]
    fn a_variables_finding_moves_back_to_the_variables_own_line() {
        let found = diagnostics(format!("[{}]", finding("timestampexpanded", 6)).as_bytes())
            .expect("the array should be read");

        assert_eq!(found.first().map(|diagnostic| diagnostic.line), Some(5));
        assert_eq!(
            diagnostics(format!("[{}]", finding("timestampexpanded", 1)).as_bytes()),
            None
        );
    }

    #[test]
    fn a_count_on_stderr_that_doesnt_match_the_array_is_a_tool_failure() {
        let stdout = format!("[{}]", finding("uniquetargets", 3));

        let verdict = lint_verdict(
            "",
            false,
            Ok(ran(false, &stdout, "Error: violations found (2)")),
        );

        assert!(
            matches!(verdict, lint::Verdict::Failed(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn a_minphony_finding_is_a_tool_failure_since_supdocs_config_switches_it_off() {
        let stdout = format!("[{}]", finding("minphony", 1));

        let verdict = lint_verdict(
            "",
            false,
            Ok(ran(false, &stdout, "Error: violations found (1)")),
        );

        assert!(
            matches!(verdict, lint::Verdict::Failed(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn an_entry_whose_line_isnt_positive_or_with_an_unknown_field_isnt_read() {
        assert_eq!(
            diagnostics(format!("[{}]", finding("uniquetargets", -1)).as_bytes()),
            None
        );
        assert_eq!(
            diagnostics(format!("[{}]", finding("uniquetargets", 0)).as_bytes()),
            None
        );
        assert_eq!(
            diagnostics(br#"[{"rule":"a","violation":"b","file_name":"c","line_number":1,"x":1}]"#),
            None
        );
    }

    #[test]
    fn only_a_verified_minor_is_verified() {
        assert_eq!(
            verified(b"checkmake v0.3.2 built at 2026-01-10T15:47:31Z by m with go1.25.1\n"),
            Ok("0.3.2".to_owned())
        );
        assert_eq!(
            verified(b"checkmake v0.4.0 built at x\n"),
            Err("0.4.0".to_owned())
        );
        assert_eq!(verified(b"checkmake 0.3.2\n"), Err(UNREADABLE.to_owned()));
        assert_eq!(verified(b"checkmake v0.3\n"), Err(UNREADABLE.to_owned()));
    }

    #[test]
    fn with_no_config_the_copy_only_switches_minphony_off() {
        assert_eq!(kept(""), Ok("[minphony]\ndisabled = true\n".to_owned()));
    }

    #[test]
    fn the_copy_keeps_each_rules_options_and_drops_minphony_and_checkmakes_own_settings() {
        let text = "output = text\n; comment\n[default]\nformat = x\n\n[minphony]\n\
                    disabled = false\nrequired = all\n[maxbodylength]\nmaxBodyLength=9\n\
                    [uniquetargets]\nignore = a, b.c\ndisabled = false\n";

        assert_eq!(
            kept(text),
            Ok(
                "[minphony]\ndisabled = true\n\n[maxbodylength]\nmaxBodyLength = 9\n\n\
                [uniquetargets]\nignore = a, b.c\ndisabled = false\n"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_config_switching_off_every_rule_supdoc_runs_is_refused() {
        let off = |rules: &[&str]| {
            let mut text = String::new();
            for rule in rules {
                text.push('[');
                text.push_str(rule);
                text.push_str("]\ndisabled = true\n");
            }
            text
        };
        let every = off(&RULES);
        let all_but_one = off(RULES.get(1..).unwrap_or_default());

        assert!(kept(&every).is_err(), "every rule off should be refused");
        assert!(
            kept(&all_but_one).is_ok(),
            "one rule left on is a lint step"
        );
    }

    #[test]
    fn a_near_miss_or_a_line_supdoc_cant_sort_is_refused() {
        for text in [
            "[maxBodyLength]\ndisabled = true\n",
            "[uniquetargets]\nDisabled = true\n",
            "[uniquetargets]\ndisabled = TRUE\n",
            "[uniquetargets]\ndisabled = \"true\"\n",
            "[uniquetargets]\ndisabled: true\n",
            "[uniquetargets]\n  disabled = true\n",
            "[uniquetargets]\ndisabled = true\ndisabled = false\n",
            "[uniquetargets]\n[uniquetargets]\n",
            "[maxbodylength]\nmaxBodyLength = -1\n",
            "[maxbodylength]\nmaxBodyLength = 5 ; six\n",
            "[uniquetargets]\nignore = a # b\n",
            "[phonydeclared]\nignore = a\n",
            "[uniquetargets]\rdisabled = true\n",
            "\u{feff}[uniquetargets]\n",
        ] {
            assert!(kept(text).is_err(), "{text:?} should be refused");
        }
    }

    #[test]
    fn a_line_longer_than_checkmake_reads_is_too_long() {
        let longest = "x".repeat(MAX_LINE);

        assert!(!too_long(&format!("a:\n\t{}\n", "x".repeat(MAX_LINE - 1))));
        assert!(!too_long(&format!("{longest}\n")));
        assert!(too_long(&format!("{longest}x\n")));
    }

    #[test]
    fn a_block_file_holds_the_block_privately_and_is_removed_when_dropped() {
        let home = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let neutral = home.path().join("cache").join("neutral");
        fs::create_dir_all(&neutral).expect("the neutral directory should be creatable");

        let run = BlockFile::create(&neutral, "all:\n\techo\n").expect("a block file");
        let file = run.file().to_path_buf();
        let dir = file
            .parent()
            .expect("the file has a directory")
            .to_path_buf();

        assert_eq!(
            fs::read_to_string(&file).expect("the block file should be readable"),
            "all:\n\techo\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| {
                fs::metadata(path)
                    .expect("the path should exist")
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(dir.parent().expect("a base directory")), 0o700);
            assert_eq!(mode(&file), 0o600);
        }
        drop(run);
        assert!(!dir.exists(), "the run's directory should be removed");
    }
}
