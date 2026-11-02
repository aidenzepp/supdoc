//! Config errors: a stable code, a one-line message and a separate hint for each.

use std::fmt;
use std::io::ErrorKind;
use std::ops::Range;

use crate::exit::ExitStatus;
use crate::lock::error::shell_quote;
use crate::namespace::NamespaceName;
use crate::output::paths::relative_to;
use crate::path::{AbsolutePath, RelativePath};

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

    /// A config file in the chain is larger than `max-config-size`, so it was not read.
    TooLarge {
        /// The file.
        path: AbsolutePath,
        /// Its size in bytes, or as many as were read before the read stopped.
        size: u64,
        /// The ceiling in bytes.
        limit: u64,
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
    /// unreadable are all this error, so one outcome stands for every such value. supdoc refuses
    /// it without reading it.
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
    /// missing, so the three share one outcome.
    SymlinkNotConfig {
        /// The config file as found, before following the symlink.
        path: AbsolutePath,
    },

    /// A namespace names a directory outside the project root, either as written or once a
    /// symlink along it is followed.
    NamespaceOutsideRoot {
        /// The config that defined the namespace.
        file: AbsolutePath,
        /// The namespace.
        name: NamespaceName,
        /// The directory it names, as written from `file`.
        directory: AbsolutePath,
        /// Byte range of the namespace's value inside `file`.
        span: Range<usize>,
        /// The project root.
        root: AbsolutePath,
        /// How `directory` ends up outside the root. Where a symlink leads is never printed.
        escape: NamespaceEscape,
    },

    /// A chain of `extends` comes back to a file it already visited.
    ExtendsLoop {
        /// The files in the loop, in order. The last is the first again.
        chain: Vec<AbsolutePath>,
        /// Byte range of the closing `extends` value, inside the file before the last in `chain`.
        span: Range<usize>,
    },
}

/// How a namespace's directory ends up outside the project root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceEscape {
    /// The directory as written lies outside the root.
    AsWritten,
    /// The directory as written lies inside the root, and a symlink along it leads out.
    ThroughSymlink,
    /// Where the directory lies can't be worked out, such as through a symlink loop, so it is
    /// treated as outside.
    Unresolvable,
}

impl ConfigError {
    /// Stable, machine-readable identity. Never reworded.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "config-not-found",
            Self::Read { .. } => "config-read",
            Self::TooLarge { .. } => "config-too-large",
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
    /// A namespace's directory that lies outside the root, and the root itself, stay absolute.
    #[must_use]
    pub fn message_from(&self, cwd: &AbsolutePath) -> String {
        self.describe(&|file| relative_to(cwd, file))
    }

    /// The message, naming each config file as `show` writes it.
    fn describe(&self, show: &dyn Fn(&AbsolutePath) -> String) -> String {
        match self {
            Self::NotFound { path } => format!("no config file at {}", show(path)),
            Self::Read { path, error } => format!("cannot read {}: {error}", show(path)),
            Self::TooLarge { path, size, limit } => format!(
                "{} is {} (limit {}), so it was not read",
                show(path),
                size_text(*size),
                size_text(*limit)
            ),
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
                "{} extends `{extends}`, which doesn't lead to a supdoc.config.toml supdoc can read \
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
                escape: NamespaceEscape::ThroughSymlink,
                ..
            } => format!(
                "{} sets namespace `{name}` to {}, which leads outside the project root {} \
                 through a symlink",
                show(file),
                show(directory),
                root.display()
            ),
            Self::NamespaceOutsideRoot {
                file,
                name,
                directory,
                escape: NamespaceEscape::Unresolvable,
                ..
            } => format!(
                "{} sets namespace `{name}` to {}, which supdoc can't follow to a real location \
                 (a symlink along it may loop)",
                show(file),
                show(directory)
            ),
            Self::NamespaceOutsideRoot {
                file,
                name,
                directory,
                root,
                escape: NamespaceEscape::AsWritten,
                ..
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
    /// command in the help runs as written. Every error has one.
    #[must_use]
    pub fn help_from(&self, cwd: &AbsolutePath) -> String {
        self.hint(&|file| relative_to(cwd, file))
            .unwrap_or_else(|| GLOB_OR_SYNTAX_HELP.to_owned())
    }

    /// What to do next, naming any file as `show` writes it.
    fn hint(&self, show: &dyn Fn(&AbsolutePath) -> String) -> Option<String> {
        match self {
            Self::NotFound { path } => {
                let config = shell_quote(&show(path));
                Some(format!(
                    "Create it with `supdoc -c {config} init`, or leave out `--config` to search \
                     upward from the current directory"
                ))
            }
            Self::Read { path, error } => Some(read_help(&shell_quote(&show(path)), error.kind())),
            Self::TooLarge { size, .. } => {
                let raised = size_text(size.saturating_add(1).next_power_of_two());
                Some(format!(
                    "Real configs are a few KiB. If this one is real, raise the limit with \
                     `SUPDOC_MAX_CONFIG_SIZE={raised}` or `--max-config-size {raised}`. `0` means no \
                     limit. The limit can't be set inside a config file, because it guards reading \
                     that file"
                ))
            }
            Self::Parse { error, .. } => parse_help(error.message()),
            Self::ExtendsMissing { resolved, .. } => {
                let resolved = shell_quote(&show(resolved));
                Some(format!(
                    "Create it with `supdoc -c {resolved} init`, or point `extends` at an \
                     existing supdoc.config.toml, relative to this file"
                ))
            }
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
            Self::NamespaceOutsideRoot {
                escape: NamespaceEscape::ThroughSymlink,
                ..
            } => Some(
                "Point the namespace at a real directory inside the project root, not a symlink \
                 that leads out of it"
                    .to_owned(),
            ),
            Self::NamespaceOutsideRoot {
                escape: NamespaceEscape::Unresolvable,
                ..
            } => Some(
                "Point the namespace at a real directory inside the project root, and check that no \
                 symlink along it leads back to itself"
                    .to_owned(),
            ),
            Self::NamespaceOutsideRoot {
                escape: NamespaceEscape::AsWritten,
                ..
            } => Some(
                "Point the namespace at a directory inside the project root, or move the project's \
                 `supdoc.config.toml` up to a directory that holds both"
                    .to_owned(),
            ),
        }
    }

    /// The process exit status for this error (06 X7): 74 when a file can't be read, else 78.
    #[must_use]
    pub fn status(&self) -> ExitStatus {
        match self {
            Self::Read { .. } => ExitStatus::Io,
            Self::NotFound { .. }
            | Self::TooLarge { .. }
            | Self::Parse { .. }
            | Self::ExtendsMissing { .. }
            | Self::ExtendsDirectory { .. }
            | Self::ExtendsLoop { .. }
            | Self::ExtendsNotConfig { .. }
            | Self::SymlinkNotConfig { .. }
            | Self::NamespaceOutsideRoot { .. } => ExitStatus::Config,
        }
    }
}

/// `bytes` in the largest unit, `B`, `KiB` or `MiB`, that divides it; a size with a remainder is
/// shown in `KiB` or `MiB` to one decimal place, rounded down, as `292.9KiB`.
pub(crate) fn size_text(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    let (unit, scale) = match bytes {
        b if b >= MIB => ("MiB", MIB),
        b if b >= KIB => ("KiB", KIB),
        _ => return format!("{bytes}B"),
    };
    if bytes.is_multiple_of(scale) {
        format!("{}{unit}", bytes / scale)
    } else {
        let tenths = bytes.saturating_mul(10) / scale;
        format!("{}.{}{unit}", tenths / 10, tenths % 10)
    }
}

/// The help for a parse error with no hint of its own: a bad glob or a TOML syntax error.
const GLOB_OR_SYNTAX_HELP: &str = "Fix the text at the line shown. A list is written \
     `include = [\"docs\"]` and a glob like `docs/**/*.md`";

/// The help for a config file that could not be read, chosen from why. `file` is already quoted
/// for a shell. The file may be reached through `extends`, so `--config` is not offered.
fn read_help(file: &str, kind: ErrorKind) -> String {
    match kind {
        ErrorKind::PermissionDenied => {
            format!("Give the current user read access, for example `chmod u+r {file}`")
        }
        ErrorKind::InvalidData => {
            format!("Save {file} as UTF-8 text: supdoc cannot read any other encoding")
        }
        _ => format!("Check that {file} is a file this user can read, for example `cat {file}`"),
    }
}

/// The hint for a parse error, chosen from the message the value types wrote.
///
/// The types raise their errors through serde, which keeps only the text, so this reads the text
/// back. The unit tests pin each message so a reworded one fails here, not silently.
fn parse_help(message: &str) -> Option<String> {
    if message.starts_with("unknown field `max-config-size`") {
        Some(
            "Remove `max-config-size` from this file: it guards reading config files, so no \
             config file can set it. Set it with `SUPDOC_MAX_CONFIG_SIZE=1MiB` or \
             `--max-config-size 1MiB` instead"
                .to_owned(),
        )
    } else if let Some(help) = limit_help(message) {
        Some(help)
    } else if message.starts_with("unknown field") {
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
    } else if message.starts_with("invalid type: string")
        && message.ends_with("expected a sequence")
    {
        let value = message.split('"').nth(1).unwrap_or_default();
        Some(format!(
            "Write a list, such as `[\"{value}\"]`: `include` and `exclude` take a list of \
             patterns even when there is only one"
        ))
    } else if let Some(form) = message
        .strip_prefix("invalid type:")
        .and_then(|_| message.rsplit(", expected ").next())
        .map(type_form)
    {
        Some(form)
    } else if message.contains("is an absolute path") {
        Some("Write the path relative to this config file, such as `services/auth`".to_owned())
    } else if message.starts_with("pattern `")
        && message.ends_with("leaves this config's directory")
    {
        let pattern = message.split('`').nth(1).unwrap_or_default();
        Some(format!(
            "Patterns match only files under this config's directory. Remove `..`, or move the \
             pattern into the config of a directory that holds `{pattern}`"
        ))
    } else if message.starts_with("pattern `") && message.contains("` has a `.` segment; write `") {
        let corrected = message.split('`').nth(5).unwrap_or_default();
        Some(format!(
            "Write `{corrected}`: patterns match paths under this config's directory, which never \
             contain a `.` segment"
        ))
    } else if let Some((count, _)) =
        message.split_once(" patterns are too many or too complex to match together")
    {
        Some(format!(
            "Use fewer, broader patterns than these {count}: a pattern also matches everything \
             beneath it, so `generated` covers every file under that directory"
        ))
    } else {
        None
    }
}

/// The help for a `[limits]` value that isn't `0` or a number with a unit, from the message
/// [`crate::limits::LimitError`] wrote or the one serde wrote for a value of another type, such as
/// a float. `None` for any other message.
fn limit_help(message: &str) -> Option<String> {
    const DURATION: &str = "a duration such as `parse-timeout = \"10s\"` (also `ms` or `m`)";
    const SIZE: &str = "a size such as `max-lock-size = \"16MiB\"` (also `B` or `KiB`)";
    let form = if message.contains("` is not a duration; give a unit") {
        DURATION.to_owned()
    } else if message.contains("` is not a size; give a unit") {
        SIZE.to_owned()
    } else if message.starts_with("invalid type:")
        && message.ends_with(
            "expected a value with a unit, such as \"10s\" or \"256KiB\", or 0 for no limit",
        )
    {
        format!("{DURATION}, or {SIZE}")
    } else {
        return None;
    };
    Some(format!(
        "Write the limit as a whole number with its unit in quotes: {form}. `0` means no limit"
    ))
}

/// The help for a value of the wrong type, from the type the message says was expected.
fn type_form(expected: &str) -> String {
    match expected {
        "a sequence" => "`include` and `exclude` take a list of patterns: write \
                         `include = [\"docs\"]`"
            .to_owned(),
        "a string" => "`extends` and each namespace take a path in quotes: write \
                       `extends = \"../supdoc.config.toml\"`"
            .to_owned(),
        "a map" | "a table" => "`namespaces` and `features` are tables: write `[namespaces]` on \
                                its own line, then `docs = \"docs\"` beneath it"
            .to_owned(),
        _ => format!(
            "Rewrite the value as {expected}: `include = [\"docs\"]` is a list, \
             `extends = \"base/supdoc.config.toml\"` is a path"
        ),
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.describe(&|file| file.display().to_string()))
    }
}

impl std::error::Error for ConfigError {}
