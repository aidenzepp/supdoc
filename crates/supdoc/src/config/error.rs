//! Config errors: a stable code, a one-line message and a separate hint for each.

use std::fmt::Write as _;
use std::ops::Range;

use crate::namespace::NamespaceName;
use crate::path::{AbsolutePath, RelativePath};

/// Exit status for a config problem the user fixes by editing files (06 X7).
const EXIT_CONFIG: u8 = 78;

/// Exit status for a config file that exists but cannot be read (06 X7).
const EXIT_IO: u8 = 74;

/// Why a config could not be loaded.
///
/// `Display` says what happened in one line. [`ConfigError::help`] says what to do next.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `--config` named a file that isn't there.
    #[error("no config file at {}", .path.display())]
    NotFound {
        /// The file that was asked for.
        path: AbsolutePath,
    },

    /// The file exists but couldn't be read (permissions, not UTF-8, ...).
    ///
    /// The field is `error`, not `source`: the reason is already in `Display`, so a renderer
    /// walking `source()` would print it twice.
    #[error("cannot read {}: {error}", .path.display())]
    Read {
        /// The file that could not be read.
        path: AbsolutePath,
        /// The operating system's reason.
        error: std::io::Error,
    },

    /// Malformed TOML, unknown key, bad glob, bad namespace name or absolute path.
    #[error("{}: {}", .path.display(), .error.message())]
    Parse {
        /// The file that failed to parse.
        path: AbsolutePath,
        /// The parser's error, which carries the span of the offending text.
        error: toml::de::Error,
    },

    /// `extends` names a file that doesn't exist.
    #[error(
        "{} extends `{}`, but no file exists at {}",
        .file.display(), .extends, .resolved.display()
    )]
    ExtendsMissing {
        /// The config that wrote `extends`.
        file: AbsolutePath,
        /// The value as written.
        extends: RelativePath,
        /// Byte range of the value inside `file`.
        span: Range<usize>,
        /// Where the value points.
        resolved: AbsolutePath,
    },

    /// `extends` names a directory.
    #[error(
        "{} extends `{}`, which is a directory, not a config file",
        .file.display(), .extends
    )]
    ExtendsDirectory {
        /// The config that wrote `extends`.
        file: AbsolutePath,
        /// The value as written.
        extends: RelativePath,
        /// Byte range of the value inside `file`.
        span: Range<usize>,
        /// The config file inside that directory, when one exists.
        suggestion: Option<RelativePath>,
    },

    /// `extends` names a file that isn't called `supdoc.config.toml`, before or after following
    /// symlinks. supdoc refuses it without reading it.
    #[error(
        "{} extends `{}`, which isn't a supdoc.config.toml file",
        .file.display(), .extends
    )]
    ExtendsNotConfig {
        /// The config that wrote `extends`.
        file: AbsolutePath,
        /// The value as written.
        extends: RelativePath,
        /// Byte range of the value inside `file`.
        span: Range<usize>,
    },

    /// A namespace names a directory outside the project root.
    #[error(
        "{} sets namespace `{name}` to {}, which is outside the project root {}",
        .file.display(), .directory.display(), .root.display()
    )]
    NamespaceOutsideRoot {
        /// The config that defined the namespace.
        file: AbsolutePath,
        /// The namespace.
        name: NamespaceName,
        /// The directory it names, as written from `file`.
        directory: AbsolutePath,
        /// The project root.
        root: AbsolutePath,
    },

    /// A chain of `extends` comes back to a file it already visited.
    #[error("extends loop: {}", display_chain(.chain))]
    ExtendsLoop {
        /// The files in the loop, in order. The last is the first again.
        chain: Vec<AbsolutePath>,
        /// Byte range of the closing `extends` value, inside the file before the last in `chain`.
        span: Range<usize>,
    },
}

impl ConfigError {
    /// Stable, machine-readable identity. Never reworded.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "config-not-found",
            Self::Read { .. } => "config-read",
            Self::Parse { .. } => "config-parse",
            Self::ExtendsMissing { .. } => "config-extends-missing",
            Self::ExtendsDirectory { .. } => "config-extends-directory",
            Self::ExtendsLoop { .. } => "config-extends-loop",
            Self::ExtendsNotConfig { .. } => "config-extends-not-config",
            Self::NamespaceOutsideRoot { .. } => "config-namespace-outside-root",
        }
    }

    /// What to do next, kept apart from `Display` so the message stays one line.
    ///
    /// `None` when the message is already the fix.
    #[must_use]
    pub fn help(&self) -> Option<String> {
        match self {
            Self::NotFound { .. } => Some(
                "Check the `--config` path, or leave out `--config` to search upward from the \
                 current directory"
                    .to_owned(),
            ),
            Self::Read { .. } => Some(
                "Give the current user read access, or pass `--config` with a readable file"
                    .to_owned(),
            ),
            Self::Parse { error, .. } => parse_help(error.message()),
            Self::ExtendsMissing { .. } => Some(
                "Point `extends` at an existing supdoc.config.toml, relative to this file"
                    .to_owned(),
            ),
            Self::ExtendsDirectory { suggestion, .. } => Some(match suggestion {
                Some(file) => format!("Use `extends = \"{file}\"`"),
                None => "Point `extends` at a supdoc.config.toml file, not a directory".to_owned(),
            }),
            Self::ExtendsLoop { .. } => Some("Remove `extends` from one of these files".to_owned()),
            Self::ExtendsNotConfig { .. } => Some(
                "Point `extends` at a file named supdoc.config.toml. supdoc reads no other file \
                 through `extends`"
                    .to_owned(),
            ),
            Self::NamespaceOutsideRoot { .. } => Some(
                "Point the namespace at a directory inside the project root, or move the project's \
                 `supdoc.config.toml` up to a directory that holds both. supdoc reads nothing \
                 outside the root"
                    .to_owned(),
            ),
        }
    }

    /// The process exit status for this error (06 X7): 74 when a file can't be read, else 78.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Read { .. } => EXIT_IO,
            Self::NotFound { .. }
            | Self::Parse { .. }
            | Self::ExtendsMissing { .. }
            | Self::ExtendsDirectory { .. }
            | Self::ExtendsLoop { .. }
            | Self::ExtendsNotConfig { .. }
            | Self::NamespaceOutsideRoot { .. } => EXIT_CONFIG,
        }
    }
}

/// The hint for a parse error, chosen from the message the value types wrote.
///
/// The types raise their errors through serde, which keeps only the text, so this reads the text
/// back. The unit tests pin each message so a reworded one fails here, not silently.
fn parse_help(message: &str) -> Option<String> {
    if message.starts_with("unknown field") {
        let key = message.split('`').nth(1).unwrap_or_default();
        Some(format!(
            "Remove `{key}`, or rename it to one of the keys the message lists"
        ))
    } else if message.starts_with("namespace names") {
        Some(
            "Rename the namespace to an ASCII letter followed by letters, digits, `-` or `_`, \
             at least 2 characters long"
                .to_owned(),
        )
    } else if message.contains("is an absolute path") {
        Some("Write the path relative to this config file, such as `services/auth`".to_owned())
    } else if message.ends_with("leaves this config's directory") {
        Some(
            "Links, namespaces and extends stay inside the project root. For a monorepo, put \
             `supdoc.config.toml` at the repo root"
                .to_owned(),
        )
    } else {
        None
    }
}

/// Joins a chain of config files with arrows, for the loop message.
fn display_chain(chain: &[AbsolutePath]) -> String {
    let mut text = String::new();
    for (index, file) in chain.iter().enumerate() {
        if index > 0 {
            text.push_str(" \u{2192} ");
        }
        // Writing to a `String` cannot fail.
        let _written = write!(text, "{}", file.display());
    }
    text
}
