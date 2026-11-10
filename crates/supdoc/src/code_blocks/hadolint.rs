//! hadolint: its JSON report, its version, and the config supdoc gives it.
//!
//! hadolint reads the block on stdin (`-`). Without `--config` it takes the first config it finds
//! in the directory it runs in, under `XDG_CONFIG_HOME` or in the home directory, so supdoc always
//! passes `--config` with a private file of its own and hadolint searches nowhere. supdoc finds
//! the project's `.hadolint.yaml` or `.hadolint.yml` itself, walking up from the doc to the root,
//! and writes again only the top-level keys that say which rules apply and how: `ignored`,
//! `override`, `trustedRegistries`, `label-schema`, `strict-labels` and `disable-ignore-pragma`.
//! Every other key is
//! dropped: `format` (a config's `format` beats `-f json` on the command line), `no-color`, and
//! `no-fail` and `failure-threshold`, which change only hadolint's exit status. supdoc reads the
//! JSON, never the exit status, so every finding at any level is reported.
//!
//! The config is read as text, never parsed as YAML: each top-level key starts a line, and every
//! indented, blank, comment or `- ` line after it belongs to it. A config with any other kind of
//! line at the top (a quoted or complex key, a document marker, a flow mapping, an indented line
//! before the first key) or a carriage return that doesn't end a line (hadolint breaks lines on
//! it) is refused, so nothing reaches hadolint that supdoc didn't sort. hadolint lints with its defaults when it
//! can't read a config, saying so only on stderr: anything on stderr is a tool failure (see
//! [`lint_verdict`]).
//!
//! hadolint also reads `HADOLINT_*` variables, which supdoc never passes (see
//! [`super::tools::ToolEnv`]).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::format;
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;
use crate::config::confine::{Boundary, ReadEscape};
use crate::hash::Hash;
use crate::path::{self, Component, fixed};

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 256 * 1024;

/// The top-level keys supdoc keeps from the project's config: the rules hadolint applies and the
/// levels it gives them. A severity `override` can raise or lower a level, never hide a finding;
/// `disable-ignore-pragma` can only add findings, so dropping it would make supdoc laxer than the
/// project's own hadolint.
const KEPT: [&str; 6] = [
    "ignored",
    "override",
    "trustedRegistries",
    "label-schema",
    "strict-labels",
    "disable-ignore-pragma",
];

/// What supdoc's copy holds when the project keeps none of [`KEPT`]: hadolint reads an empty
/// config as an error and then lints with its defaults, so the copy is never empty.
const NOTHING_KEPT: &str = "ignored: []\n";

/// What hadolint says on stderr when it can't read the config it was given.
const CONFIG_ERROR: &str = "Error parsing your config file";

/// One entry of `hadolint -f json`'s array.
#[derive(serde::Deserialize)]
struct Entry {
    /// Its rule, such as `DL3006`; `DL1000` is a syntax error.
    code: String,
    /// Its 1-based line.
    line: usize,
    /// Its 1-based column.
    column: usize,
    /// What hadolint says.
    message: String,
}

/// The rule hadolint reports a block it can't parse under.
const SYNTAX: &str = "DL1000";

/// The diagnostics in `hadolint -f json`'s `stdout`, every level alike. `None` when it isn't that
/// JSON array. A syntax error's message keeps only its first line: the rest lists what hadolint
/// expected.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let entries: Vec<Entry> = serde_json::from_slice(stdout).ok()?;
    Some(
        entries
            .into_iter()
            .map(|entry| Diagnostic {
                line: entry.line,
                column: entry.column,
                end_line: entry.line,
                syntax: entry.code == SYNTAX,
                message: entry.message.lines().next().unwrap_or_default().to_owned(),
                rule: entry.code,
            })
            .collect(),
    )
}

/// Judges a hadolint run by its output alone, never its exit status, which a config's `no-fail`
/// or `failure-threshold` would move: anything on stderr is a tool failure (hadolint says there
/// that it couldn't read its config, then lints with its defaults); else an empty JSON array is
/// clean, a non-empty one gives its diagnostics, and any other stdout is a tool failure. hadolint
/// checks no fragments, so no run is framed.
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
    if stderr.contains(CONFIG_ERROR) {
        return failed("hadolint couldn't read the config supdoc gave it".to_owned());
    }
    if !stderr.trim().is_empty() {
        return failed(format::exit_detail(&ran.stderr));
    }
    match diagnostics(&ran.stdout) {
        Some(found) if found.is_empty() => lint::Verdict::Clean,
        Some(found) => lint::Verdict::Diagnostics(found),
        None => failed(format::exit_detail(&ran.stderr)),
    }
}

/// The hadolint versions, as `<major>.<minor>`, supdoc's checks are verified against: supdoc
/// writes hadolint's config and reads its JSON, and another version may read or write either
/// differently. A minor is added only after the Dockerfile e2e suite passes against it.
pub(crate) const VERIFIED: [&str; 1] = ["2.15"];

/// What `hadolint --version` prints before the version.
const VERSION_PREFIX: &str = "Haskell Dockerfile Linter ";

/// How a hadolint whose version can't be read is named.
const UNREADABLE: &str = "(whose version supdoc couldn't read)";

/// The version `hadolint --version` printed in `stdout` (`Haskell Dockerfile Linter
/// <major>.<minor>.<patch>`) when it is one supdoc is verified against (see [`VERIFIED`]).
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
        .trim_end()
        .strip_prefix(VERSION_PREFIX)
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

/// [`verified`] on what `program`, a hadolint binary, prints for `--version`, run as any tool is
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
        name: "hadolint",
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

/// Why a block isn't given to hadolint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's hadolint config, at `file` (relative to the root), can't be given to
    /// hadolint.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// supdoc has no private directory to keep hadolint's config in.
    NoPrivateDir,
}

/// What hadolint is given for the blocks of one doc directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// `--config` and the private file, always: hadolint never searches for a config.
    pub(crate) flags: Vec<OsString>,
}

/// The [`Project`] hadolint is given for `doc`: the project's config, found walking up from
/// `doc`'s directory to `root` (see [`candidates`]), its kept keys (see [`kept`]) written into a
/// private file named by the hash of what is written; with no config, a private file holding
/// [`NOTHING_KEPT`].
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config file in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable UTF-8
/// file of at most 256 KiB, or holds a top-level line supdoc doesn't read (see [`kept`]);
/// [`Refusal::NoPrivateDir`] when there is nowhere private to put it.
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
        None => (None, String::new()),
    };
    let text = if text.trim().is_empty() {
        NOTHING_KEPT.to_owned()
    } else {
        text
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

/// The config files in `doc`'s directory and its ancestors, nearest first, never above `root`.
fn candidates(root: &Path, doc: &Path) -> impl Iterator<Item = PathBuf> {
    let dirs: Vec<PathBuf> = doc
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    dirs.into_iter()
        .flat_map(|dir| fixed::HADOLINT_CONFIG_FILES.map(|name| path::beneath(&dir, &name)))
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

/// What supdoc writes again of the config `text`: the lines of each top-level key in [`KEPT`],
/// in order, and nothing of any other key, with `\n` ending every line. A top-level key is a line
/// that starts with a plain name (letters, digits, `-` and `_`) followed by `:` and a space or
/// the line's end; every following line that is blank, indented, a comment or a `- ` item belongs
/// to it. Blank and comment lines before the first key are dropped.
///
/// # Errors
///
/// Why supdoc doesn't read the config: a carriage return that doesn't end a line (hadolint breaks
/// lines on it, so it could hide a key in the line before), an indented or `- ` line before the
/// first key (hadolint may read an indented key as a top-level one), or a line at the top that is
/// none of those.
fn kept(text: &str) -> Result<String, &'static str> {
    let text = text.replace("\r\n", "\n");
    if text.contains('\r') {
        return Err(
            "holds a carriage return that doesn't end a line, which hadolint reads as a line \
             break: end each line with `\\n` or `\\r\\n`",
        );
    }
    let mut copy = String::new();
    // Whether the current top-level key is kept; `None` before the first key.
    let mut keeping = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let blank_or_comment = trimmed.is_empty() || trimmed.starts_with('#');
        let continues = line.starts_with([' ', '\t']) || line == "-" || line.starts_with("- ");
        if blank_or_comment || continues {
            match keeping {
                Some(true) => {
                    copy.push_str(line);
                    copy.push('\n');
                }
                None if !blank_or_comment => {
                    return Err(
                        "holds an indented or `- ` line before its first top-level key, which \
                         hadolint may read as a key of its own: start each top-level key at the \
                         start of a line",
                    );
                }
                Some(false) | None => {}
            }
            continue;
        }
        let key = top_level_key(line).ok_or(
            "holds a line supdoc doesn't read as a top-level key: write each key plainly as \
             `<name>: <value>` at the start of a line",
        )?;
        let keep = KEPT.contains(&key);
        keeping = Some(keep);
        if keep {
            copy.push_str(line);
            copy.push('\n');
        }
    }
    Ok(copy)
}

/// The plain name `line` starts a top-level key with: letters, digits, `-` and `_` up to a `:`
/// that ends the line or is followed by a space. `None` for any other line.
fn top_level_key(line: &str) -> Option<&str> {
    let (key, rest) = line.split_once(':')?;
    let plain = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (plain && (rest.is_empty() || rest.starts_with([' ', '\t']))).then_some(key)
}

/// Writes `text` into `hadolint.yaml` in a private directory named by the hash of `text`, under
/// supdoc's `hadolint-config` directory, and returns the file's path. A file already holding the
/// same content is kept; anything else at the name is replaced by writing a temporary file beside
/// it and renaming it into place, so concurrent runs never see half a file.
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
    let parent = cache.beneath(&fixed::HADOLINT_CONFIG_DIR);
    let digest = Component::new(&Hash::digest(text.as_bytes()).to_string())?;
    let dir = parent.beneath(&digest);
    let file = dir.beneath(&fixed::HADOLINT_YAML);
    let parent_boundary = runner::create_private(&Boundary::resolve(&cache), &parent)?;
    let own = runner::create_private(&parent_boundary, &dir)?;
    runner::write_private(&own, &file, text)?;
    Some(file.as_path().to_path_buf())
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

    /// One finding of hadolint's JSON at `line` under `code` with `level`.
    fn finding(code: &str, level: &str, line: usize) -> String {
        format!(
            r#"{{"code":"{code}","column":1,"file":"-","level":"{level}","line":{line},"message":"m"}}"#
        )
    }

    #[test]
    fn a_finding_at_every_level_is_a_diagnostic_whatever_the_exit_status() {
        let levels = ["error", "warning", "info", "style"];
        let all = levels
            .iter()
            .enumerate()
            .map(|(at, level)| finding("DL3006", level, at + 1))
            .collect::<Vec<_>>()
            .join(",");
        for success in [true, false] {
            let verdict = lint_verdict("", false, Ok(ran(success, &format!("[{all}]"), "")));
            let lint::Verdict::Diagnostics(found) = verdict else {
                panic!("exit {success}: expected diagnostics, got {verdict:?}");
            };
            assert_eq!(
                found.iter().map(|found| found.line).collect::<Vec<_>>(),
                [1, 2, 3, 4],
                "exit {success}"
            );
        }
    }

    #[test]
    fn an_empty_array_is_clean_whatever_the_exit_status() {
        for success in [true, false] {
            assert_eq!(
                lint_verdict("", false, Ok(ran(success, "[]", ""))),
                lint::Verdict::Clean,
                "exit {success}"
            );
        }
    }

    #[test]
    fn a_config_error_on_stderr_is_a_tool_failure_even_with_an_empty_array() {
        let said = "Error parsing your config file in  '/c/hadolint.yaml':\nIt should contain one \
                    of the keys 'override', 'ignored'\n";

        let verdict = lint_verdict("", false, Ok(ran(true, "[]", said)));

        assert_eq!(
            verdict,
            lint::Verdict::Failed(format::Verdict::ToolFailed(
                "hadolint couldn't read the config supdoc gave it".to_owned()
            ))
        );
    }

    #[test]
    fn any_other_stderr_is_a_tool_failure_naming_what_it_said() {
        let verdict = lint_verdict("", false, Ok(ran(true, "[]", "boom\n")));

        assert_eq!(
            verdict,
            lint::Verdict::Failed(format::Verdict::ToolFailed("boom".to_owned()))
        );
    }

    #[test]
    fn stdout_that_isnt_the_json_array_is_a_tool_failure() {
        let verdict = lint_verdict("", false, Ok(ran(true, "-:1 DL3006 warning: x", "")));

        assert!(
            matches!(verdict, lint::Verdict::Failed(_)),
            "got {verdict:?}"
        );
    }

    #[test]
    fn a_syntax_error_keeps_only_its_first_line_and_is_marked_syntax() {
        let stdout = r#"[{"code":"DL1000","column":1,"file":"/dev/stdin","level":"error","line":1,"message":"unexpected 'F'\nexpecting '#'"}]"#;

        let found = diagnostics(stdout.as_bytes()).expect("the array should parse");

        let [only] = found.as_slice() else {
            panic!("expected one diagnostic, got {found:?}");
        };
        assert!(only.syntax, "DL1000 is a syntax error");
        assert_eq!(only.message, "unexpected 'F'");
    }

    #[test]
    fn only_a_verified_minor_passes_the_version_gate() {
        assert_eq!(
            verified(b"Haskell Dockerfile Linter 2.15.1\n"),
            Ok("2.15.1".to_owned())
        );
        assert_eq!(
            verified(b"Haskell Dockerfile Linter 2.16.0\n"),
            Err("2.16.0".to_owned())
        );
        for unreadable in [
            &b"Haskell Dockerfile Linter 2.15\n"[..],
            b"hadolint 2.15.1\n",
            b"Haskell Dockerfile Linter 2.15.1-beta\n",
            b"",
        ] {
            assert_eq!(
                verified(unreadable),
                Err(UNREADABLE.to_owned()),
                "{}",
                String::from_utf8_lossy(unreadable)
            );
        }
    }

    #[test]
    fn the_copy_keeps_the_rule_keys_and_drops_format_and_the_exit_switches() {
        let config = "# the project's\nformat: tty\nno-color: true\nno-fail: true\n\
                      failure-threshold: error\nignored:\n- DL3006\n  # why\n  - DL3008\n\
                      override:\n  error: [DL3015]\ndisable-ignore-pragma: true\n\
                      trustedRegistries: [docker.io]\nlabel-schema:\n  author: text\n\
                      strict-labels: true\n";

        let copy = kept(config).expect("a plain config should be read");

        assert_eq!(
            copy,
            "ignored:\n- DL3006\n  # why\n  - DL3008\noverride:\n  error: [DL3015]\n\
             disable-ignore-pragma: true\ntrustedRegistries: [docker.io]\nlabel-schema:\n  \
             author: text\nstrict-labels: true\n"
        );
    }

    #[test]
    fn a_bare_carriage_return_is_refused_so_it_cant_smuggle_a_dropped_key() {
        let refused = kept("ignored: []\rformat: tty\n");

        assert!(
            refused.is_err_and(|why| why.contains("carriage return")),
            "a lone \\r should be refused"
        );
    }

    #[test]
    fn a_crlf_config_keeps_its_kept_keys_with_plain_line_ends() {
        assert_eq!(
            kept("format: tty\r\nignored:\r\n  - DL3006\r\nno-fail: true\r\n"),
            Ok("ignored:\n  - DL3006\n".to_owned())
        );
    }

    #[test]
    fn an_indented_line_before_the_first_key_is_refused() {
        for config in [
            "  ignored: [DL3006]\n",
            "# header\n\n  ignored: [DL3006]\nformat: tty\n",
            "- DL3006\n",
        ] {
            assert!(
                kept(config).is_err_and(|why| why.contains("before its first top-level key")),
                "{config:?} should be refused"
            );
        }
    }

    #[test]
    fn indented_comments_and_blanks_before_the_first_key_are_dropped() {
        assert_eq!(
            kept("  # a note\n   \nignored: []\n"),
            Ok("ignored: []\n".to_owned())
        );
    }

    #[test]
    fn a_top_level_line_that_isnt_a_plain_key_is_refused() {
        for config in [
            "\"format\": tty\n",
            "format : tty\n",
            "? format\n: tty\n",
            "---\nignored: []\n",
            "{format: tty}\n",
            "<<: *base\n",
            "ignored:[DL3006]\n",
        ] {
            assert!(kept(config).is_err(), "{config:?} should be refused");
        }
    }

    #[test]
    fn a_config_keeping_nothing_copies_nothing() {
        assert_eq!(kept("format: tty\nno-fail: true\n"), Ok(String::new()));
    }
}
