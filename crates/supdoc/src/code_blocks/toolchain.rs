//! Reading the Rust toolchain a project pins, in-root only.
//!
//! The rustup proxy would read `rust-toolchain(.toml)` itself and run whatever binary a `path`
//! key names, or download what a channel names. supdoc reads the root's file instead, passes
//! only a valid channel name as `RUSTUP_TOOLCHAIN`, and always sets `RUSTUP_AUTO_INSTALL=0`.
//!
//! A root with no toolchain file would leave the proxy to search the root's parents, and cargo,
//! run from the root, could then get another toolchain than clippy-driver, run from a neutral
//! directory. So supdoc asks rustup once per run which toolchain the root gets, and pins that
//! channel for every Rust tool (see [`active`]).

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use super::runner::{self, Call, Cwd};
use super::tools::{Found, ToolEnv, find};
use crate::config::code_blocks::Edition;

/// The toolchain files supdoc reads, in rustup's order: `rust-toolchain` wins when both exist.
const FILES: [&str; 2] = ["rust-toolchain", "rust-toolchain.toml"];

/// The most of a toolchain file supdoc reads. A longer file is refused.
const MAX_BYTES: u64 = 64 * 1024;

/// What the root's toolchain file pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Toolchain {
    /// No toolchain file, or one that names no channel, and no `rustup` to ask: rustup's default.
    Default,
    /// A valid channel name, passed as `RUSTUP_TOOLCHAIN`.
    Channel(String),
    /// The file can't be used safely; the Rust blocks aren't checked.
    Refused {
        /// Why.
        refusal: Refusal,
        /// The refused file's name in the root: `rust-toolchain` or `rust-toolchain.toml`.
        file: &'static str,
    },
    /// The root pins nothing and rustup couldn't name its toolchain as a channel; the Rust
    /// blocks aren't checked.
    Unresolved(Unresolved),
}

/// Why rustup's answer for a root that pins nothing can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unresolved {
    /// The `rustup` on `PATH` is inside the checkout, so the checkout chose it.
    InsideRoot,
    /// `rustup show active-toolchain` ran but named no channel supdoc can pin.
    Rustup {
        /// Why, as rustup's answer or its error shows it.
        cause: Cause,
        /// What rustup said: its first stderr line, or for a path or custom toolchain the name
        /// it printed, at most [`SAID_WIDTH`] characters. Empty when it said nothing.
        said: String,
    },
}

/// Why `rustup show active-toolchain` named no channel supdoc can pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    /// It named a path or a custom toolchain, as a `path` key in a toolchain file above the root
    /// or a `rustup toolchain link` makes it.
    PathOrCustom,
    /// The toolchain it chose isn't installed, and supdoc never lets rustup install one.
    NotInstalled,
    /// A toolchain file it read, above the root, doesn't parse.
    Unparseable,
    /// Nothing chooses a toolchain and rustup has no default.
    NoDefault,
    /// It couldn't start, failed for another reason, or printed nothing supdoc can read.
    Failed,
    /// It ran past the tool timeout and was killed.
    TimedOut,
    /// It printed past the tool output cap and was killed.
    OverCap,
}

/// The most characters of rustup's words an [`Unresolved::Rustup`] keeps.
const SAID_WIDTH: usize = 200;

/// Why the root's toolchain file was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// It has a `path` key, which would run a binary the checkout chose.
    Path,
    /// Its channel isn't a name supdoc accepts, or the file doesn't parse.
    InvalidChannel,
    /// Its real path is outside the root.
    Outside,
    /// It can't be resolved or read, isn't a regular file, isn't UTF-8, or is over 64 KiB.
    Unreadable,
}

impl Toolchain {
    /// The variables every Rust tool call gets: `RUSTUP_AUTO_INSTALL=0`, and `RUSTUP_TOOLCHAIN`
    /// for a pinned channel.
    #[must_use]
    pub(crate) fn env(&self) -> Vec<(&'static str, OsString)> {
        let mut vars = vec![("RUSTUP_AUTO_INSTALL", OsString::from("0"))];
        if let Self::Channel(channel) = self {
            vars.push(("RUSTUP_TOOLCHAIN", OsString::from(channel)));
        }
        vars
    }
}

/// The toolchain `root`'s `rust-toolchain` or `rust-toolchain.toml` pins, read only through a
/// real path inside `root`'s real path. Only the first file that exists is read.
///
/// Effect: reads the file's metadata, resolves symlinks, and reads at most 64 KiB of it.
#[must_use]
pub(crate) fn read(root: &Path) -> Toolchain {
    let Some(file) = FILES
        .into_iter()
        .find(|name| std::fs::symlink_metadata(root.join(name)).is_ok())
    else {
        return Toolchain::Default;
    };
    match read_inside(&root.join(file), root) {
        Ok(text) => parse(&text).unwrap_or_else(|refusal| Toolchain::Refused { refusal, file }),
        Err(refusal) => Toolchain::Refused { refusal, file },
    }
}

/// The toolchain rustup gives `root`, for a root whose own file pins none: [`Toolchain::Default`]
/// when no `rustup` is on `env`'s `PATH`, else the channel `rustup show active-toolchain` names
/// first, run once from the root with `env` plus `RUSTUP_AUTO_INSTALL=0`, under `timeout` and the
/// tool output cap. A `rustup` inside the checkout isn't run, and an answer [`is_valid_channel`]
/// rejects, such as the path a parent's `path` key names, is [`Toolchain::Unresolved`], as is
/// any failure, each with its [`Cause`] (see [`judge`]).
///
/// Effect: reads file metadata along `PATH`, resolves symlinks, and runs `rustup`, killing it at
/// the timeout.
#[must_use]
pub(crate) fn active(env: &ToolEnv, root: &Path, timeout: Option<Duration>) -> Toolchain {
    let rustup = match find("rustup", env, root) {
        Found::Missing => return Toolchain::Default,
        Found::InsideRoot(_) => return Toolchain::Unresolved(Unresolved::InsideRoot),
        Found::OnPath(rustup) => rustup,
    };
    let args = [OsStr::new("show"), OsStr::new("active-toolchain")];
    let call = Call {
        program: &rustup,
        name: "rustup",
        args: &args,
        extra: &[("RUSTUP_AUTO_INSTALL", OsStr::new("0"))],
        stdin: b"",
        timeout,
        cwd: Cwd::Root,
        cap: runner::CAP,
    };
    judge(runner::run(&call, env, root))
}

/// The toolchain `outcome`, a run of `rustup show active-toolchain`, names: the channel its first
/// field names when it succeeded and [`is_valid_channel`] accepts it, else
/// [`Toolchain::Unresolved`] with the [`Cause`] its answer, its first stderr line or its failure
/// shows. A failure whose words name no cause supdoc knows is [`Cause::Failed`].
fn judge(outcome: Result<runner::Ran, runner::Failure>) -> Toolchain {
    let (cause, said) = match outcome {
        Err(runner::Failure::TimedOut) => (Cause::TimedOut, String::new()),
        Err(runner::Failure::OverCap { .. }) => (Cause::OverCap, String::new()),
        Err(runner::Failure::Spawn(error)) => {
            (Cause::Failed, first_line(error.to_string().as_bytes()))
        }
        Ok(ran) if ran.success => {
            if let Some(channel) = active_channel(&ran.stdout) {
                return Toolchain::Channel(channel);
            }
            let named = first_line(&ran.stdout);
            if std::str::from_utf8(&ran.stdout).is_ok() && !named.is_empty() {
                let name = named
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                (Cause::PathOrCustom, name)
            } else {
                (Cause::Failed, first_line(&ran.stderr))
            }
        }
        Ok(ran) => {
            let said = first_line(&ran.stderr);
            (cause_in(&said), said)
        }
    };
    Toolchain::Unresolved(Unresolved::Rustup { cause, said })
}

/// The cause the words of a failed rustup, `said`, name: the phrases rustup prints for each.
fn cause_in(said: &str) -> Cause {
    let said = said.to_ascii_lowercase();
    if said.contains("is not installed") {
        Cause::NotInstalled
    } else if said.contains("could not parse") || said.contains("error parsing") {
        Cause::Unparseable
    } else if said.contains("no default") {
        Cause::NoDefault
    } else if said.contains("invalid toolchain")
        || said.contains("path toolchain")
        || said.contains("custom toolchain")
    {
        Cause::PathOrCustom
    } else {
        Cause::Failed
    }
}

/// The first non-blank line of `bytes`, trimmed, read lossily as UTF-8 and cut to
/// [`SAID_WIDTH`] characters. Empty when there is none.
fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(SAID_WIDTH)
        .collect()
}

/// The channel `stdout`, what `rustup show active-toolchain` printed, names in its first field,
/// when [`is_valid_channel`] accepts it.
fn active_channel(stdout: &[u8]) -> Option<String> {
    let first = std::str::from_utf8(stdout)
        .ok()?
        .split_whitespace()
        .next()?;
    is_valid_channel(first).then(|| first.to_owned())
}

/// The edition the root's `Cargo.toml` names: `[workspace.package] edition`, then `[package]
/// edition` (or its legacy name `[project]`'s). Read only through an in-root real path, as a regular file of at most 64 KiB, and
/// never executed. `None` when there is no such file, it can't be read or parsed, or no edition
/// in it is one of `2015`, `2018`, `2021` or `2024`.
///
/// Effect: resolves symlinks and reads the file.
#[must_use]
pub(crate) fn cargo_edition(root: &Path) -> Option<Edition> {
    let text = read_inside(&root.join("Cargo.toml"), root).ok()?;
    edition_in(&text)
}

/// The lint groups among the names a `Cargo.toml` `[lints]` table may set, which go first so a
/// single lint's level overrides its group's. `clippy::cargo` is left out of the flags entirely.
const LINT_GROUPS: [&str; 22] = [
    "warnings",
    "unused",
    "nonstandard_style",
    "future_incompatible",
    "let_underscore",
    "keyword_idents",
    "refining_impl_trait",
    "deprecated_safe",
    "rust_2018_idioms",
    "rust_2018_compatibility",
    "rust_2021_compatibility",
    "rust_2024_compatibility",
    "clippy::all",
    "clippy::correctness",
    "clippy::suspicious",
    "clippy::style",
    "clippy::complexity",
    "clippy::perf",
    "clippy::pedantic",
    "clippy::restriction",
    "clippy::nursery",
    "clippy::cargo",
];

/// The root `Cargo.toml`'s lint levels as rustc flags (`-W clippy::pedantic`): its
/// `[workspace.lints]`, else its `[lints]`, for the `rust` and `clippy` tools only. Read only
/// through an in-root real path, as [`cargo_edition`] reads it. Ordered by `priority`, then groups
/// before single lints, then by name; `clippy::cargo`, which judges a manifest no block has, is
/// left out. Empty when there is no such file or table.
///
/// Effect: resolves symlinks and reads the file.
#[must_use]
pub(crate) fn cargo_lints(root: &Path) -> Vec<OsString> {
    read_inside(&root.join("Cargo.toml"), root)
        .map(|text| lints_in(&text))
        .unwrap_or_default()
}

/// The lint flags `text`, a `Cargo.toml`, sets, by [`cargo_lints`]' rules.
fn lints_in(text: &str) -> Vec<OsString> {
    let Ok(table) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(lints) = table
        .get("workspace")
        .and_then(|workspace| workspace.get("lints"))
        .or_else(|| table.get("lints"))
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };
    let mut found: Vec<(i64, bool, String, &'static str)> = Vec::new();
    for (tool, entries) in lints {
        let prefix = match tool.as_str() {
            "rust" => "",
            "clippy" => "clippy::",
            _ => continue,
        };
        for (name, value) in entries.as_table().into_iter().flatten() {
            let (level, priority) = match value {
                toml::Value::String(level) => (level.as_str(), 0),
                toml::Value::Table(entry) => (
                    entry
                        .get("level")
                        .and_then(toml::Value::as_str)
                        .unwrap_or_default(),
                    entry
                        .get("priority")
                        .and_then(toml::Value::as_integer)
                        .unwrap_or(0),
                ),
                _ => continue,
            };
            let flag = match level {
                "forbid" => "-F",
                "deny" => "-D",
                "warn" => "-W",
                "allow" => "-A",
                _ => continue,
            };
            let lint = format!("{prefix}{name}");
            if lint == "clippy::cargo" {
                continue;
            }
            let single = !LINT_GROUPS.contains(&lint.as_str());
            found.push((priority, single, lint, flag));
        }
    }
    found.sort();
    found
        .into_iter()
        .flat_map(|(_, _, lint, flag)| [OsString::from(flag), OsString::from(lint)])
        .collect()
}

/// The edition `text`, a `Cargo.toml`, names: its workspace package's, then its package's, read
/// from `[package]` or the legacy `[project]` cargo still reads as it.
fn edition_in(text: &str) -> Option<Edition> {
    let table = text.parse::<toml::Table>().ok()?;
    let named = |section: Option<&toml::Value>| {
        section
            .and_then(|section| section.get("edition"))
            .and_then(toml::Value::as_str)
            .and_then(Edition::from_year)
    };
    named(table.get("workspace").and_then(|w| w.get("package")))
        .or_else(|| named(table.get("package")))
        .or_else(|| named(table.get("project")))
}

/// The text of `file` when its real path is a regular UTF-8 file under `root`'s real path and
/// is at most [`MAX_BYTES`] long.
///
/// # Errors
///
/// [`Refusal::Outside`] when its real path leaves the root, and [`Refusal::Unreadable`] when it
/// can't be resolved, opened or read, isn't a regular file, isn't UTF-8, or is too long.
///
/// Effect: resolves symlinks and reads the file.
pub(super) fn read_inside(file: &Path, root: &Path) -> Result<String, Refusal> {
    let unreadable = |_| Refusal::Unreadable;
    let real_root = root.canonicalize().map_err(unreadable)?;
    let real = file.canonicalize().map_err(unreadable)?;
    if !real.starts_with(&real_root) {
        return Err(Refusal::Outside);
    }
    if !std::fs::metadata(&real).map_err(unreadable)?.is_file() {
        return Err(Refusal::Unreadable);
    }
    let mut text = String::new();
    let _read = std::fs::File::open(&real)
        .map_err(unreadable)?
        .take(MAX_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(unreadable)?;
    if text.len() as u64 > MAX_BYTES {
        return Err(Refusal::Unreadable);
    }
    Ok(text)
}

/// What a toolchain file's `text` pins: a legacy one-line channel, or TOML with a `[toolchain]`
/// table whose `path` key is refused and whose `channel` must be valid.
///
/// # Errors
///
/// [`Refusal::Path`] for a `path` key, and [`Refusal::InvalidChannel`] for a channel
/// [`is_valid_channel`] rejects or text that doesn't parse.
fn parse(text: &str) -> Result<Toolchain, Refusal> {
    let trimmed = text.trim();
    if !trimmed.contains('\n') && !trimmed.starts_with('[') && !trimmed.contains('=') {
        return channel(trimmed);
    }
    let table = text
        .parse::<toml::Table>()
        .map_err(|_| Refusal::InvalidChannel)?;
    let toolchain = table
        .get("toolchain")
        .and_then(toml::Value::as_table)
        .ok_or(Refusal::InvalidChannel)?;
    if toolchain.contains_key("path") {
        return Err(Refusal::Path);
    }
    match toolchain.get("channel") {
        None => Ok(Toolchain::Default),
        Some(toml::Value::String(name)) => channel(name),
        Some(_) => Err(Refusal::InvalidChannel),
    }
}

/// `name` as a pinned channel.
///
/// # Errors
///
/// [`Refusal::InvalidChannel`] when [`is_valid_channel`] rejects it.
fn channel(name: &str) -> Result<Toolchain, Refusal> {
    if is_valid_channel(name) {
        Ok(Toolchain::Channel(name.to_owned()))
    } else {
        Err(Refusal::InvalidChannel)
    }
}

/// Whether `name` is `stable`, `beta`, `nightly` or `X.Y[.Z]`, then an optional `-YYYY-MM-DD`,
/// then an optional host triple. Nothing else: no `/`, so rustup can't read it as a path, and no
/// custom toolchain name.
fn is_valid_channel(name: &str) -> bool {
    let rest = ["stable", "beta", "nightly"]
        .iter()
        .find_map(|base| name.strip_prefix(base))
        .or_else(|| strip_version(name));
    let Some(rest) = rest else {
        return false;
    };
    let rest = strip_date(rest).unwrap_or(rest);
    rest.is_empty() || rest.strip_prefix('-').is_some_and(is_host_triple)
}

/// `name` after a leading `X.Y` or `X.Y.Z` of ASCII digits, when it has one.
fn strip_version(name: &str) -> Option<&str> {
    let end = name
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(name.len());
    let (version, rest) = name.split_at(end);
    let parts: Vec<&str> = version.split('.').collect();
    let valid = (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    valid.then_some(rest)
}

/// `rest` after a leading `-YYYY-MM-DD`, when it starts with one followed by its end or `-`.
fn strip_date(rest: &str) -> Option<&str> {
    let date = rest.strip_prefix('-')?.get(..10)?;
    let shape = date.bytes().enumerate().all(|(at, byte)| match at {
        4 | 7 => byte == b'-',
        _ => byte.is_ascii_digit(),
    });
    let after = rest.get(11..)?;
    (shape && (after.is_empty() || after.starts_with('-'))).then_some(after)
}

/// Whether `triple` is two to five `-`-separated parts of ASCII lowercase letters, digits, `_`
/// and `.`, starting with a letter, as `x86_64-unknown-linux-gnu` is. The letter keeps a
/// malformed date from passing as a triple.
fn is_host_triple(triple: &str) -> bool {
    let parts: Vec<&str> = triple.split('-').collect();
    (2..=5).contains(&parts.len())
        && triple.starts_with(|c: char| c.is_ascii_lowercase())
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'.')
                })
        })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    use super::{
        Cause, Refusal, Toolchain, Unresolved, active, active_channel, cargo_edition, edition_in,
        is_valid_channel, judge, parse, read,
    };
    use crate::code_blocks::runner::{Failure, Ran};
    use crate::code_blocks::tools::ToolEnv;
    use crate::config::code_blocks::Edition;

    /// The [`Ran`] of a rustup that exited `success` after printing `stdout` and `stderr`.
    fn finished(success: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            success,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// The unresolved toolchain with `cause` and rustup's words `said`.
    fn unresolved(cause: Cause, said: &str) -> Toolchain {
        Toolchain::Unresolved(Unresolved::Rustup {
            cause,
            said: said.to_owned(),
        })
    }

    #[test]
    fn each_rustup_failure_carries_its_own_cause_and_rustups_first_stderr_line() {
        let ran = |success, stdout, stderr| Ok(finished(success, stdout, stderr));
        let not_installed = "error: override toolchain '1.60.0-aarch64-apple-darwin' is not \
                             installed: the toolchain file at '/w/rust-toolchain.toml' specifies an \
                             uninstalled toolchain";
        let unparseable = "error: could not parse override file: '/w/rust-toolchain.toml': error \
                           parsing override file: TOML parse error at line 1, column 5";
        let no_default = "error: rustup could not choose a version of rustc to run, because one \
                          wasn't specified explicitly, and no default is configured.";
        for (outcome, expected) in [
            (
                ran(
                    false,
                    "",
                    &format!("{not_installed}\nhelp: run rustup toolchain install\n"),
                ),
                unresolved(Cause::NotInstalled, not_installed),
            ),
            (
                ran(false, "", &format!("\n{unparseable}\n  |\n1 | not [toml\n")),
                unresolved(Cause::Unparseable, unparseable),
            ),
            (
                ran(false, "", &format!("{no_default}\n")),
                unresolved(Cause::NoDefault, no_default),
            ),
            (
                ran(
                    false,
                    "",
                    "error: invalid toolchain: the path '/opt/x' has no bin/ directory\n",
                ),
                unresolved(
                    Cause::PathOrCustom,
                    "error: invalid toolchain: the path '/opt/x' has no bin/ directory",
                ),
            ),
            (
                ran(
                    true,
                    "/w/evil (overridden by '/w/rust-toolchain.toml')\n",
                    "",
                ),
                unresolved(Cause::PathOrCustom, "/w/evil"),
            ),
            (
                ran(true, "my-toolchain (default)\n", ""),
                unresolved(Cause::PathOrCustom, "my-toolchain"),
            ),
            (
                ran(false, "", "error: something new\n"),
                unresolved(Cause::Failed, "error: something new"),
            ),
            (ran(false, "", ""), unresolved(Cause::Failed, "")),
            (
                ran(true, "", "warning: odd\n"),
                unresolved(Cause::Failed, "warning: odd"),
            ),
            (Err(Failure::TimedOut), unresolved(Cause::TimedOut, "")),
            (
                Err(Failure::OverCap {
                    stream: "stdout",
                    cap: 1024,
                }),
                unresolved(Cause::OverCap, ""),
            ),
            (
                ran(true, "stable-aarch64-apple-darwin (default)\n", ""),
                Toolchain::Channel("stable-aarch64-apple-darwin".to_owned()),
            ),
        ] {
            assert_eq!(judge(outcome), expected);
        }
    }

    #[test]
    fn rustups_quoted_line_is_cut_to_two_hundred_characters() {
        let long = format!("error: {}\n", "x".repeat(400));

        let Toolchain::Unresolved(Unresolved::Rustup { said, .. }) =
            judge(Ok(finished(false, "", &long)))
        else {
            panic!("a failed rustup should be unresolved");
        };

        assert_eq!(said.chars().count(), 200, "{said}");
    }

    /// A directory holding a stub `rustup` whose body is `body`, and the environment with it alone
    /// on `PATH`.
    fn rustup_stub(body: &str) -> (tempfile::TempDir, ToolEnv) {
        let bin = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let stub = bin.path().join("rustup");
        std::fs::write(&stub, format!("#!/bin/sh\n{body}")).expect("the stub should be writable");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
        let path = bin.path().as_os_str().to_owned();
        let env = ToolEnv::from_vars(|name| (name == "PATH").then(|| path.clone()));
        (bin, env)
    }

    #[test]
    fn a_rustup_that_runs_past_the_timeout_is_unresolved() {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let (_bin, env) = rustup_stub("exec /bin/sleep 30\n");

        let toolchain = active(&env, root.path(), Some(Duration::from_millis(200)));

        assert_eq!(toolchain, unresolved(Cause::TimedOut, ""));
    }

    #[test]
    fn a_rustup_that_prints_past_the_cap_is_unresolved() {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let (_bin, env) = rustup_stub(
            "/bin/dd if=/dev/zero bs=1048577 count=1 2>/dev/null\nexec /bin/sleep 30\n",
        );

        let toolchain = active(&env, root.path(), Some(Duration::from_secs(20)));

        assert_eq!(toolchain, unresolved(Cause::OverCap, ""));
    }

    #[test]
    fn channels_with_dates_and_host_triples_are_valid() {
        for name in [
            "stable",
            "beta",
            "nightly",
            "1.85",
            "1.85.0",
            "nightly-2025-01-31",
            "1.85.0-x86_64-unknown-linux-gnu",
            "nightly-2025-01-31-aarch64-apple-darwin",
        ] {
            assert!(is_valid_channel(name), "{name:?} should be a valid channel");
        }
    }

    #[test]
    fn paths_custom_names_and_malformed_versions_are_invalid() {
        for name in [
            "",
            "/opt/evil",
            "../evil",
            "stable/../../x",
            "my-toolchain",
            "1",
            "1.85.0.1",
            "1..2",
            "nightly-2025-1-31",
            "stable-X86",
            "stable-",
            "1.85.0-x86_64-unknown-linux-gnu/../bin",
        ] {
            assert!(!is_valid_channel(name), "{name:?} should be refused");
        }
    }

    #[test]
    fn rustups_answer_is_its_first_field_when_that_is_a_channel() {
        for (printed, channel) in [
            (
                "stable-aarch64-apple-darwin (default)\n",
                Some("stable-aarch64-apple-darwin"),
            ),
            (
                "1.85.0-x86_64-unknown-linux-gnu (overridden by '/w/rust-toolchain.toml')\n",
                Some("1.85.0-x86_64-unknown-linux-gnu"),
            ),
            ("nightly-2025-01-31\n", Some("nightly-2025-01-31")),
            ("/w/evil (overridden by '/w/rust-toolchain.toml')\n", None),
            ("my-toolchain (default)\n", None),
            ("", None),
            ("\n", None),
        ] {
            assert_eq!(
                active_channel(printed.as_bytes()).as_deref(),
                channel,
                "{printed:?}"
            );
        }
        assert_eq!(active_channel(b"stable\xff (default)\n"), None);
    }

    #[test]
    fn a_path_key_is_refused_even_beside_a_valid_channel() {
        assert_eq!(
            parse("[toolchain]\nchannel = \"stable\"\npath = \"/opt/evil\"\n"),
            Err(Refusal::Path)
        );
    }

    #[test]
    fn a_toml_channel_and_a_legacy_line_are_both_read() {
        assert_eq!(
            parse("[toolchain]\nchannel = \"1.85.0\"\ncomponents = [\"rustfmt\"]\n"),
            Ok(Toolchain::Channel("1.85.0".to_owned()))
        );
        assert_eq!(
            parse("nightly-2025-01-31\n"),
            Ok(Toolchain::Channel("nightly-2025-01-31".to_owned()))
        );
        assert_eq!(
            parse("[toolchain]\nchannel = \"evil/x\"\n"),
            Err(Refusal::InvalidChannel)
        );
    }

    #[test]
    fn every_toolchain_disables_auto_install_and_a_channel_is_pinned() {
        assert_eq!(
            Toolchain::Default.env(),
            [("RUSTUP_AUTO_INSTALL", OsString::from("0"))]
        );
        assert_eq!(
            Toolchain::Channel("stable".to_owned()).env(),
            [
                ("RUSTUP_AUTO_INSTALL", OsString::from("0")),
                ("RUSTUP_TOOLCHAIN", OsString::from("stable")),
            ]
        );
    }

    #[test]
    fn a_toolchain_file_linked_outside_the_root_is_refused() {
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let target = outside.path().join("rust-toolchain.toml");
        std::fs::write(&target, "[toolchain]\nchannel = \"stable\"\n")
            .expect("the outside file should be writable");
        std::os::unix::fs::symlink(&target, root.path().join("rust-toolchain.toml"))
            .expect("the link should be creatable");

        assert_eq!(
            read(root.path()),
            Toolchain::Refused {
                refusal: Refusal::Outside,
                file: "rust-toolchain.toml"
            }
        );
    }

    #[test]
    fn an_in_root_toolchain_file_that_isnt_utf_8_is_unreadable_not_outside() {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        std::fs::write(root.path().join("rust-toolchain"), b"stable\xff\n")
            .expect("the toolchain file should be writable");

        assert_eq!(
            read(root.path()),
            Toolchain::Refused {
                refusal: Refusal::Unreadable,
                file: "rust-toolchain"
            }
        );
    }

    #[test]
    fn a_workspace_edition_beats_a_package_edition() {
        assert_eq!(
            edition_in("[package]\nedition = \"2018\"\n[workspace.package]\nedition = \"2021\"\n"),
            Some(Edition::E2021)
        );
        assert_eq!(
            edition_in("[package]\nname = \"x\"\nedition = \"2018\"\n"),
            Some(Edition::E2018)
        );
    }

    #[test]
    fn a_legacy_project_table_names_the_edition_like_a_package() {
        assert_eq!(
            edition_in("[project]\nname = \"x\"\nedition = \"2018\"\n"),
            Some(Edition::E2018)
        );
    }

    #[test]
    fn an_unknown_or_inherited_edition_falls_through() {
        assert_eq!(
            edition_in("[workspace.package]\nedition = \"2030\"\n[package]\nedition = \"2015\"\n"),
            Some(Edition::E2015)
        );
        assert_eq!(edition_in("[package]\nedition.workspace = true\n"), None);
        assert_eq!(edition_in("not toml ["), None);
    }

    #[test]
    fn a_cargo_toml_linked_outside_the_root_is_not_read() {
        let outside = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let target = outside.path().join("Cargo.toml");
        std::fs::write(&target, "[package]\nedition = \"2018\"\n")
            .expect("the outside file should be writable");
        std::os::unix::fs::symlink(&target, root.path().join("Cargo.toml"))
            .expect("the link should be creatable");

        assert_eq!(cargo_edition(root.path()), None);
    }

    #[test]
    fn no_toolchain_file_is_rustups_default() {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");

        assert_eq!(read(root.path()), Toolchain::Default);
    }
}
