//! Config errors: a stable code, a one-line message and a separate hint for each.

use std::fmt;
use std::ops::Range;

use crate::lock::error::shell_quote;
use crate::namespace::NamespaceName;
use crate::output::paths::relative_to;
use crate::path::{AbsolutePath, RelativePath};

/// Exit status for a config problem the user fixes by editing files (06 X7).
const EXIT_CONFIG: u8 = 78;

/// Exit status for a config file that exists but cannot be read (06 X7).
const EXIT_IO: u8 = 74;

/// Why a config could not be loaded.
///
/// `Display` says what happened in one line, naming files by their absolute paths.
/// [`ConfigError::help`] says what to do next. [`ConfigError::message_from`] and
/// [`ConfigError::help_from`] say the same with files named relative to the current directory.
///
/// No variant has a `source` field: the reason is already in the message, so a renderer walking
/// `source()` would print it twice.
#[derive(Debug)]
pub enum ConfigError {
    /// `--config` named a file that isn't there.
    NotFound {
        /// The file that was asked for.
        path: AbsolutePath,
    },

    /// The file exists but couldn't be read (permissions, not UTF-8, ...).
    Read {
        /// The file that could not be read.
        path: AbsolutePath,
        /// The operating system's reason.
        error: std::io::Error,
    },

    /// Malformed TOML, unknown key, bad glob, bad namespace name or absolute path.
    Parse {
        /// The file that failed to parse.
        path: AbsolutePath,
        /// The parser's error, which carries the span of the offending text.
        error: toml::de::Error,
    },

    /// `extends` names a file that doesn't exist, on a path inside the root with no symlink on the
    /// way. A path that leaves the root or goes through a symlink is `ExtendsNotConfig` instead.
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

    /// `extends` names a directory: a value ending in `.` or `..`, or a path inside the root with no
    /// symlink on the way. The suggestion is offered only for a config file inside the root that
    /// is itself no symlink.
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
    /// symlinks, or leaves the root or goes through a symlink without leading to a
    /// `supdoc.config.toml` file. A value that leaves the root, or goes through a symlink, either
    /// leads to a supdoc.config.toml file or is refused alike: missing, a directory, a loop and
    /// unreadable are all this error, so a checkout can't learn what exists outside it. supdoc
    /// refuses it without reading it.
    ExtendsNotConfig {
        /// The config that wrote `extends`.
        file: AbsolutePath,
        /// The value as written.
        extends: RelativePath,
        /// Byte range of the value inside `file`.
        span: Range<usize>,
    },

    /// The config found by searching upward is a symlink that doesn't lead to a file called
    /// `supdoc.config.toml`. supdoc refuses it without reading it.
    ///
    /// The message is the same whether the target is a file with another name, a directory or
    /// missing, so it reveals nothing about paths outside the root.
    SymlinkNotConfig {
        /// The config file as found, before following the symlink.
        path: AbsolutePath,
    },

    /// A namespace names a directory outside the project root.
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
            Self::SymlinkNotConfig { .. } => "config-symlink-not-config",
            Self::NamespaceOutsideRoot { .. } => "config-namespace-outside-root",
        }
    }

    /// The message, naming each file relative to `cwd`, the directory supdoc runs in, as the
    /// error's location does. No config loaded, so there is no project root to name them from.
    /// A namespace's directory, which lies outside the root, and the root itself stay absolute.
    #[must_use]
    pub fn message_from(&self, cwd: &AbsolutePath) -> String {
        self.describe(&|file| relative_to(cwd, file))
    }

    /// The message, naming each config file as `show` writes it.
    fn describe(&self, show: &dyn Fn(&AbsolutePath) -> String) -> String {
        match self {
            Self::NotFound { path } => format!("no config file at {}", show(path)),
            Self::Read { path, error } => format!("cannot read {}: {error}", show(path)),
            Self::Parse { path, error } => format!("{}: {}", show(path), error.message()),
            Self::ExtendsMissing {
                file,
                extends,
                resolved,
                ..
            } => format!(
                "{} extends `{extends}`, but no file exists at {}",
                show(file),
                show(resolved)
            ),
            Self::ExtendsDirectory { file, extends, .. } => format!(
                "{} extends `{extends}`, which is a directory, not a config file",
                show(file)
            ),
            Self::ExtendsNotConfig { file, extends, .. } => format!(
                "{} extends `{extends}`, which doesnt lead to a supdoc.config.toml supdoc can read \
                 (it may be missing, a directory, or a link to another file)",
                show(file)
            ),
            Self::SymlinkNotConfig { path } => format!(
                "{} is a symlink that doesn't lead to a supdoc.config.toml file",
                show(path)
            ),
            Self::NamespaceOutsideRoot {
                file,
                name,
                directory,
                root,
            } => format!(
                "{} sets namespace `{name}` to {}, which is outside the project root {}",
                show(file),
                directory.display(),
                root.display()
            ),
            Self::ExtendsLoop { chain, .. } => {
                let files: Vec<String> = chain.iter().map(show).collect();
                format!("extends loop: {}", files.join(" \u{2192} "))
            }
        }
    }

    /// What to do next, kept apart from `Display` so the message stays one line, naming any file
    /// by its absolute path.
    ///
    /// `None` when the message is already the fix.
    #[must_use]
    pub fn help(&self) -> Option<String> {
        self.hint(&|file| file.display().to_string())
    }

    /// What to do next, naming any file relative to `cwd`, the directory supdoc runs in, so a
    /// command in the help runs as written.
    ///
    /// `None` when the message is already the fix.
    #[must_use]
    pub fn help_from(&self, cwd: &AbsolutePath) -> Option<String> {
        self.hint(&|file| relative_to(cwd, file))
    }

    /// What to do next, naming any file as `show` writes it.
    fn hint(&self, show: &dyn Fn(&AbsolutePath) -> String) -> Option<String> {
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
                "Check the path. `extends` is relative to this file and must reach a real \
                 `supdoc.config.toml`."
                    .to_owned(),
            ),
            Self::SymlinkNotConfig { path } => {
                let config = shell_quote(&show(path));
                Some(format!(
                    "supdoc reads no other file as the project's config. Replace the symlink with \
                     a real config, for example `rm {config} && supdoc -c {config} init`, or pass \
                     `--config` with the file you mean"
                ))
            }
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
            | Self::SymlinkNotConfig { .. }
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

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.describe(&|file| file.display().to_string()))
    }
}

impl std::error::Error for ConfigError {}
