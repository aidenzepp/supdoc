//! Reading the Rust toolchain a project pins, in-root only.
//!
//! The rustup proxy would read `rust-toolchain(.toml)` itself and run whatever binary a `path`
//! key names, or download what a channel names. supdoc reads the root's file instead, passes
//! only a valid channel name as `RUSTUP_TOOLCHAIN`, and always sets `RUSTUP_AUTO_INSTALL=0`.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::Path;

/// The toolchain files supdoc reads, in rustup's order: `rust-toolchain` wins when both exist.
const FILES: [&str; 2] = ["rust-toolchain", "rust-toolchain.toml"];

/// The most of a toolchain file supdoc reads. A longer file is refused.
const MAX_BYTES: u64 = 64 * 1024;

/// What the root's toolchain file pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Toolchain {
    /// No toolchain file, or one that names no channel: rustup's default.
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
}

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

/// The text of `file` when its real path is a regular UTF-8 file under `root`'s real path and
/// is at most [`MAX_BYTES`] long.
///
/// # Errors
///
/// [`Refusal::Outside`] when its real path leaves the root, and [`Refusal::Unreadable`] when it
/// can't be resolved, opened or read, isn't a regular file, isn't UTF-8, or is too long.
///
/// Effect: resolves symlinks and reads the file.
fn read_inside(file: &Path, root: &Path) -> Result<String, Refusal> {
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

    use super::{Refusal, Toolchain, is_valid_channel, parse, read};

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
    fn no_toolchain_file_is_rustups_default() {
        let root = tempfile::TempDir::new().expect("a temporary directory should be creatable");

        assert_eq!(read(root.path()), Toolchain::Default);
    }
}
