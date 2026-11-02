//! Lockfile errors: what failed, a stable code, and what to do next (02 Errors).

use std::fmt::{self, Display, Formatter};
use std::io::ErrorKind;

use crate::exit::ExitStatus;
use crate::hash::LineEndings;
use crate::limits::SizeLimit;
use crate::link::LinkKey;
use crate::output::paths::relative_to;
use crate::path::AbsolutePath;

/// Why lockfile text could not be turned into a [`Lockfile`](super::model::Lockfile).
///
/// Parsing changes nothing on disk, so every variant leaves the lockfile as it was.
#[derive(Debug, thiserror::Error)]
pub enum ParseLockError {
    /// A merge left `<<<<<<<`, `=======` or `>>>>>>>` at the start of a line.
    #[error("unresolved merge conflict at line {line}")]
    ConflictMarkers {
        /// The 1-based line of the first marker.
        line: usize,
    },

    /// The file was written by a newer supdoc than this one.
    #[error("lockfile format {found} is newer than this supdoc supports ({supported})")]
    NewerVersion {
        /// The `version` the file declares.
        found: u32,
        /// The newest `version` this supdoc reads.
        supported: u32,
    },

    /// The text is not valid v1 TOML, or a field is not well formed.
    #[error("{}", toml_message(*.line, .error))]
    Toml {
        /// The 1-based line the problem is on, when TOML reported where.
        line: Option<usize>,
        /// The underlying TOML or field error.
        error: toml::de::Error,
        /// Whether the error is the `[features.hash]` table, or a key in it, being absent
        /// (rather than a `[[link]]` entry missing the same key).
        missing_features: bool,
    },

    /// Two entries have the same key but different hashes.
    #[error("two entries for {key} disagree (lines {first_line} and {second_line})")]
    ConflictingDuplicate {
        /// The key both entries share.
        key: LinkKey,
        /// The 1-based line of the first entry.
        first_line: usize,
        /// The 1-based line of the second entry.
        second_line: usize,
    },
}

/// The TOML message, followed by its line when known.
fn toml_message(line: Option<usize>, error: &toml::de::Error) -> String {
    match line {
        Some(line) => format!("{} (line {line})", error.message()),
        None => error.message().to_owned(),
    }
}

/// Whether a TOML message is about the lockfile header's `version`, not a `[[link]]` entry.
///
/// An empty file and a file with no `version` both report the field as missing. `version` is the
/// lockfile's only `u32` field, so a wrongly typed one (`"1"`, `1.0`, `-1`) is the only error
/// that expects a `u32`.
fn is_header_problem(message: &str) -> bool {
    message == "missing field `version`"
        || message.starts_with("unsupported lockfile format")
        || message.ends_with("expected u32")
}

/// `text` as one shell word: unchanged when it holds only characters the shell never treats
/// specially, otherwise in single quotes, so a help can name a path with spaces and still run.
pub(crate) fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c));
    if plain {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

impl ParseLockError {
    /// A stable identifier for machine-readable output.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ConflictMarkers { .. } => "lock-conflict",
            Self::NewerVersion { .. } => "lock-newer-version",
            Self::Toml { .. } => "lock-parse",
            Self::ConflictingDuplicate { .. } => "lock-duplicate",
        }
    }

    /// What to do about it.
    #[must_use]
    pub fn help(&self) -> &'static str {
        match self {
            Self::ConflictMarkers { .. } => {
                "Resolve the conflict, keeping both sides' entries, then run `supdoc check`. \
                 supdoc re-sorts the file on its next write"
            }
            Self::NewerVersion { .. } => {
                "Upgrade supdoc with `cargo install supdoc`. This lockfile was written by a \
                 newer release"
            }
            Self::Toml {
                missing_features: true,
                ..
            } => {
                "Add `[features.hash]` and `line-endings = \"normalize\"` under `version = 1`, \
                 as every lockfile supdoc writes has, then run `supdoc check`. Or delete \
                 `supdoc.lock` and run `supdoc update --yes` to record the links again"
            }
            Self::Toml { error, .. } if is_header_problem(error.message()) => {
                "Write `version = 1` at the top of supdoc.lock, then run `supdoc check`. Or \
                 delete `supdoc.lock` and run `supdoc update --yes` to record the links again"
            }
            Self::Toml { .. } => {
                "Fix that line or delete the entry, then run `supdoc check`. \
                 A deleted entry is re-added the next time you run `supdoc update`"
            }
            Self::ConflictingDuplicate { .. } => {
                "A merge probably kept both sides. Delete one entry and run `supdoc check`. \
                 If the doc is now stale, `check` will say so"
            }
        }
    }
}

/// Why the lockfile on disk could not be read or written.
///
/// `Display` names the lockfile by its absolute path. [`LockError::message_from`] names it
/// relative to the project root, for output a user reads.
///
/// No variant has a `source` field: the reason is already in the message, so a renderer walking
/// `source()` would print it twice.
#[derive(Debug)]
pub enum LockError {
    /// The file exists but could not be read.
    Read {
        /// The lockfile.
        path: AbsolutePath,
        /// The underlying I/O failure.
        error: std::io::Error,
    },

    /// Something other than a regular file, such as a directory or FIFO, is at the lockfile's
    /// path (or at the in-root file its symlink leads to), so supdoc refuses it without opening
    /// it. Reported under `lock-read`.
    NotRegularFile {
        /// The lockfile, as supdoc looked for it.
        path: AbsolutePath,
    },

    /// The lockfile is a symlink that doesn't lead to a `supdoc.lock` inside the project root,
    /// so supdoc refuses it without reading it. The message is the same whether or not anything
    /// is there.
    OutsideRoot {
        /// The lockfile, as supdoc looked for it.
        path: AbsolutePath,
    },

    /// The lockfile is larger than `max-lock-size`, so supdoc refuses it before reading it.
    TooLarge {
        /// The lockfile.
        path: AbsolutePath,
        /// Its size in bytes, as measured on the open file, or as far as it was read if it grew.
        size: u64,
        /// The `max-lock-size` in effect.
        limit: SizeLimit,
    },

    /// The file was read but its text is not a valid lockfile.
    Parse {
        /// The lockfile.
        path: AbsolutePath,
        /// Why the text was rejected.
        error: Box<ParseLockError>,
    },

    /// The lockfile could not be written. The previous one, if any, is unchanged.
    Write {
        /// The lockfile.
        path: AbsolutePath,
        /// The underlying I/O failure.
        error: std::io::Error,
    },
}

impl LockError {
    /// A stable identifier for machine-readable output.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Read { .. } | Self::NotRegularFile { .. } => "lock-read",
            Self::OutsideRoot { .. } => "lock-outside-root",
            Self::TooLarge { .. } => "lock-too-large",
            Self::Parse { error, .. } => error.code(),
            Self::Write { .. } => "lock-write",
        }
    }

    /// The lockfile this error is about.
    #[must_use]
    pub fn path(&self) -> &AbsolutePath {
        match self {
            Self::Read { path, .. }
            | Self::NotRegularFile { path }
            | Self::OutsideRoot { path }
            | Self::TooLarge { path, .. }
            | Self::Parse { path, .. }
            | Self::Write { path, .. } => path,
        }
    }

    /// The message, naming the lockfile relative to `root`, the project root, as the read
    /// messages name files, so no message carries a machine-specific absolute path.
    #[must_use]
    pub fn message_from(&self, root: &AbsolutePath) -> String {
        self.message_naming(&relative_to(root, self.path()))
    }

    /// The message, naming the lockfile as `shown`.
    fn message_naming(&self, shown: &dyn Display) -> String {
        match self {
            Self::Read { error, .. } => format!("cannot read {shown}: {error}"),
            Self::NotRegularFile { .. } => format!("cannot read {shown}: it is not a regular file"),
            Self::OutsideRoot { .. } => format!(
                "{shown} is a symlink that doesn't lead to a supdoc.lock inside the project root"
            ),
            Self::TooLarge { size, limit, .. } => {
                format!("{shown} is {size} bytes; the max-lock-size limit is {limit}")
            }
            Self::Parse { error, .. } => format!("{shown}: {error}"),
            Self::Write { error, .. } => format!("cannot write {shown}: {error}"),
        }
    }

    /// What to do about it, naming the lockfile relative to `cwd`, the directory supdoc runs
    /// in, so a command in the help runs as written. Read and write failures branch on the
    /// I/O error's kind, so a full disk is never answered with a `chmod`.
    #[must_use]
    pub fn help_from(&self, cwd: &AbsolutePath) -> String {
        let shown = || shell_quote(&relative_to(cwd, self.path()));
        match self {
            Self::Read { error, .. } => read_help(error.kind(), &shown()),
            Self::NotRegularFile { .. } => format!(
                "supdoc.lock must be a regular file. Move this one aside with `mv {0} {0}.old`, \
                 then run `supdoc update --yes` to record the links again",
                shown()
            ),
            Self::OutsideRoot { .. } => format!(
                "supdoc reads only a supdoc.lock inside the project root. Remove the symlink with \
                 `rm {}`, then run `supdoc update --yes` to record the links again",
                shown()
            ),
            Self::TooLarge { size, .. } => format!(
                "supdoc refuses a lockfile this large before reading it. If {} really holds that \
                 many links, raise the limit above {size} bytes with `max-lock-size` under \
                 `[limits]` in `supdoc.config.toml`, `SUPDOC_MAX_LOCK_SIZE` or `--max-lock-size` \
                 (such as `64MiB`, or `0` for no limit). Otherwise restore it with `git checkout \
                 -- {0}`",
                shown()
            ),
            Self::Parse { error, .. } => error.help().to_owned(),
            Self::Write { error, path } => {
                let directory = path.parent().map_or_else(
                    || ".".to_owned(),
                    |directory| shell_quote(&relative_to(cwd, &directory)),
                );
                format!(
                    "The existing lockfile was not changed. {}",
                    write_help(error.kind(), &directory)
                )
            }
        }
    }

    /// The process exit status for this error (06 X7): 78 when the text is invalid or the file
    /// is refused, else 74.
    #[must_use]
    pub const fn status(&self) -> ExitStatus {
        match self {
            Self::Parse { .. } | Self::OutsideRoot { .. } | Self::TooLarge { .. } => {
                ExitStatus::Config
            }
            Self::Read { .. } | Self::NotRegularFile { .. } | Self::Write { .. } => ExitStatus::Io,
        }
    }
}

/// The next step after reading the lockfile `shown` (already shell-quoted) failed with `kind`.
fn read_help(kind: ErrorKind, shown: &str) -> String {
    match kind {
        ErrorKind::PermissionDenied => {
            format!("Give the current user read access, for example with `chmod u+r {shown}`")
        }
        ErrorKind::InvalidData => format!(
            "The file is not UTF-8 text, so supdoc didn't write it. Restore it with `git checkout \
             -- {shown}`, or delete it and run `supdoc update --yes` to record the links again"
        ),
        _ => format!(
            "Fix the cause above, then run the same command again. `ls -l {shown}` shows the \
             file's type and permissions"
        ),
    }
}

/// The next step after writing a lockfile in `directory` (already shell-quoted) failed with
/// `kind`.
fn write_help(kind: ErrorKind, directory: &str) -> String {
    match kind {
        ErrorKind::PermissionDenied => format!(
            "Make its directory writable, for example with `chmod u+w {directory}`, then run the \
             same command again"
        ),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => format!(
            "The disk holding `{directory}` is full or over quota. Free some space, then run the \
             same command again"
        ),
        ErrorKind::ReadOnlyFilesystem => format!(
            "`{directory}` is on a read-only filesystem. Run supdoc in a writable checkout, then \
             run the same command again"
        ),
        ErrorKind::AlreadyExists => format!(
            "Every temporary name supdoc tried was taken. Remove leftover `supdoc.lock.*.tmp` \
             files from `{directory}`, then run the same command again"
        ),
        _ => format!(
            "Fix the cause above, then run the same command again. `ls -ld {directory}` shows \
             the directory's permissions"
        ),
    }
}

impl Display for LockError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message_naming(&self.path().display()))
    }
}

impl std::error::Error for LockError {}

/// `supdoc.lock` was written with different hash settings than the config resolves to (02 L10).
///
/// Comparing hashes across settings would report links as changed when only the rule changed, so
/// every command except a full `supdoc update` stops here, before the scan and before any write.
///
/// `Display` names the config file by its absolute path; [`SettingsMismatch::message_from`] names
/// it relative to the project root.
#[derive(Debug)]
pub struct SettingsMismatch {
    /// The setting the lock records.
    pub recorded: LineEndings,
    /// The setting the config resolves to.
    pub configured: LineEndings,
    /// The config file that set it, or `None` when the default applies.
    pub configured_in: Option<AbsolutePath>,
}

impl Display for SettingsMismatch {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let file = self.configured_in.as_ref().map(AbsolutePath::display);
        formatter.write_str(&self.message_naming(file.as_ref().map(|file| file as &dyn Display)))
    }
}

impl std::error::Error for SettingsMismatch {}

impl SettingsMismatch {
    /// The message, naming the config file that set the value relative to `root`, the project
    /// root, so no message carries a machine-specific absolute path.
    #[must_use]
    pub fn message_from(&self, root: &AbsolutePath) -> String {
        let file = self
            .configured_in
            .as_ref()
            .map(|file| relative_to(root, file));
        self.message_naming(file.as_ref().map(|file| file as &dyn Display))
    }

    /// The message, naming the config file that set the value as `file`, or the default when no
    /// file did.
    fn message_naming(&self, file: Option<&dyn Display>) -> String {
        let configured_by = file.map_or_else(
            || "the default is".to_owned(),
            |file| format!("{file} sets"),
        );
        format!(
            "supdoc.lock was written with line-endings = \"{}\", but {configured_by} \"{}\"",
            self.recorded, self.configured
        )
    }

    /// A stable identifier for machine-readable output.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        "lock-settings-mismatch"
    }

    /// What to do about it: migrate the lock to the config, or set the config to the lock.
    #[must_use]
    pub fn help(&self) -> String {
        format!(
            "Run `supdoc update` with no paths and no --state to preview re-recording every link \
             with line-endings = \"{}\", then add --yes. To keep the lockfile as it is, set \
             [features.hash] line-endings = \"{}\" in supdoc.config.toml",
            self.configured, self.recorded
        )
    }

    /// The process exit status (06 X7): 78, a config or lockfile problem.
    #[must_use]
    pub const fn status(&self) -> ExitStatus {
        ExitStatus::Config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_code_and_help() {
        let key: LinkKey = LinkKey {
            source: "a.md".parse().expect("the fixture source should be valid"),
            target: "b.rs".parse().expect("the fixture target should be valid"),
            symbol: None,
        };
        let errors = [
            ParseLockError::ConflictMarkers { line: 1 },
            ParseLockError::NewerVersion {
                found: 2,
                supported: 1,
            },
            ParseLockError::ConflictingDuplicate {
                key,
                first_line: 1,
                second_line: 2,
            },
        ];

        let codes: Vec<_> = errors.iter().map(ParseLockError::code).collect();

        assert_eq!(
            codes,
            ["lock-conflict", "lock-newer-version", "lock-duplicate"]
        );
    }

    #[test]
    fn header_errors_say_how_to_fix_the_header_not_to_delete_an_entry() {
        for text in [
            "",
            "[features.hash]\nline-endings = \"normalize\"\n",
            "version = 0\n",
            "version = \"1\"\n",
            "version = 1.0\n",
            "version = -1\n",
        ] {
            let error = crate::lock::model::Lockfile::parse(text)
                .expect_err("the fixture lockfile header should be rejected");

            let help = error.help();

            assert!(
                help.contains("version = 1") && !help.contains("delete the entry"),
                "help for {text:?} should name the header fix, got: {help}"
            );
        }
    }

    fn mismatch(configured_in: Option<&str>) -> SettingsMismatch {
        SettingsMismatch {
            recorded: LineEndings::Exact,
            configured: LineEndings::Normalize,
            configured_in: configured_in.map(|path| {
                AbsolutePath::new(std::path::Path::new(path))
                    .expect("the fixture config path should be absolute")
            }),
        }
    }

    #[test]
    fn settings_mismatch_is_lock_settings_mismatch_with_exit_78() {
        let error = mismatch(None);

        assert_eq!(error.code(), "lock-settings-mismatch");
        assert_eq!(error.status(), ExitStatus::Config);
    }

    #[test]
    fn settings_mismatch_names_the_file_that_set_the_value() {
        assert_eq!(
            mismatch(Some("/repo/supdoc.config.toml")).to_string(),
            "supdoc.lock was written with line-endings = \"exact\", but \
             /repo/supdoc.config.toml sets \"normalize\""
        );
    }

    #[test]
    fn settings_mismatch_message_names_the_config_relative_to_the_root() {
        let root = AbsolutePath::new(std::path::Path::new("/repo"))
            .expect("the fixture root should be absolute");

        assert_eq!(
            mismatch(Some("/repo/supdoc.config.toml")).message_from(&root),
            "supdoc.lock was written with line-endings = \"exact\", but supdoc.config.toml sets \
             \"normalize\""
        );
    }

    fn absolute(raw: &str) -> AbsolutePath {
        AbsolutePath::new(std::path::Path::new(raw)).expect("the fixture path should be absolute")
    }

    #[test]
    fn lock_messages_name_the_lockfile_relative_to_the_root() {
        let root = absolute("/repo");
        let path = absolute("/repo/supdoc.lock");
        let errors = [
            LockError::Read {
                path: path.clone(),
                error: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            },
            LockError::OutsideRoot { path: path.clone() },
            LockError::Parse {
                path: path.clone(),
                error: Box::new(ParseLockError::ConflictMarkers { line: 3 }),
            },
            LockError::Write {
                path,
                error: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            },
        ];

        let messages: Vec<String> = errors
            .iter()
            .map(|error| error.message_from(&root))
            .collect();

        assert_eq!(
            messages,
            [
                "cannot read supdoc.lock: permission denied",
                "supdoc.lock is a symlink that doesn't lead to a supdoc.lock inside the project root",
                "supdoc.lock: unresolved merge conflict at line 3",
                "cannot write supdoc.lock: permission denied",
            ]
        );
    }

    #[test]
    fn lock_helps_name_the_lockfile_relative_to_the_current_directory() {
        let cwd = absolute("/repo/docs");
        let path = absolute("/repo/supdoc.lock");
        let read = LockError::Read {
            path: path.clone(),
            error: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let outside = LockError::OutsideRoot { path };

        let helps = [read.help_from(&cwd), outside.help_from(&cwd)];

        assert!(
            helps[0].contains("`chmod u+r ../supdoc.lock`"),
            "the read help should run from docs/: {}",
            helps[0]
        );
        assert!(
            helps[1].contains("`rm ../supdoc.lock`"),
            "the symlink help should run from docs/: {}",
            helps[1]
        );
    }

    fn read_error(kind: std::io::ErrorKind) -> LockError {
        LockError::Read {
            path: absolute("/repo/supdoc.lock"),
            error: std::io::Error::from(kind),
        }
    }

    fn write_error(kind: std::io::ErrorKind) -> LockError {
        LockError::Write {
            path: absolute("/repo/supdoc.lock"),
            error: std::io::Error::from(kind),
        }
    }

    #[test]
    fn unreadable_lockfile_help_gives_chmod_on_the_file() {
        let help = read_error(ErrorKind::PermissionDenied).help_from(&absolute("/repo/docs"));

        assert!(help.contains("`chmod u+r ../supdoc.lock`"), "got: {help}");
    }

    #[test]
    fn non_utf8_lockfile_help_says_so_without_chmod() {
        let help = read_error(ErrorKind::InvalidData).help_from(&absolute("/repo"));

        assert!(
            help.contains("not UTF-8") && !help.contains("chmod"),
            "got: {help}"
        );
    }

    #[test]
    fn other_read_failure_help_offers_no_chmod() {
        let help = read_error(ErrorKind::Other).help_from(&absolute("/repo"));

        assert!(
            help.contains("`ls -l supdoc.lock`") && !help.contains("chmod"),
            "got: {help}"
        );
    }

    #[test]
    fn non_regular_lockfile_is_lock_read_with_exit_74_and_a_move_aside_help() {
        let error = LockError::NotRegularFile {
            path: absolute("/repo/supdoc.lock"),
        };

        let help = error.help_from(&absolute("/repo"));

        assert_eq!(error.code(), "lock-read");
        assert_eq!(error.status(), ExitStatus::Io);
        assert_eq!(
            error.message_from(&absolute("/repo")),
            "cannot read supdoc.lock: it is not a regular file"
        );
        assert!(
            help.contains("`mv supdoc.lock supdoc.lock.old`") && !help.contains("chmod"),
            "got: {help}"
        );
    }

    #[test]
    fn unwritable_directory_help_gives_chmod_on_the_quoted_directory() {
        let help = write_error(ErrorKind::PermissionDenied).help_from(&absolute("/repo/docs"));

        assert!(
            help.starts_with("The existing lockfile was not changed.")
                && help.contains("`chmod u+w ..`"),
            "got: {help}"
        );
    }

    #[test]
    fn unwritable_directory_with_a_space_is_quoted_in_the_chmod() {
        let error = LockError::Write {
            path: absolute("/my repo/supdoc.lock"),
            error: std::io::Error::from(ErrorKind::PermissionDenied),
        };

        let help = error.help_from(&absolute("/"));

        assert!(help.contains("`chmod u+w 'my repo'`"), "got: {help}");
    }

    #[test]
    fn full_disk_help_says_the_disk_is_full_without_chmod() {
        let help = write_error(ErrorKind::StorageFull).help_from(&absolute("/repo"));

        assert!(
            help.contains("is full") && !help.contains("chmod"),
            "got: {help}"
        );
    }

    #[test]
    fn read_only_filesystem_help_says_so_without_chmod() {
        let help = write_error(ErrorKind::ReadOnlyFilesystem).help_from(&absolute("/repo"));

        assert!(
            help.contains("read-only filesystem") && !help.contains("chmod"),
            "got: {help}"
        );
    }

    #[test]
    fn exhausted_temporary_names_help_names_the_leftover_files() {
        let help = write_error(ErrorKind::AlreadyExists).help_from(&absolute("/repo"));

        assert!(
            help.contains("`supdoc.lock.*.tmp`") && !help.contains("chmod"),
            "got: {help}"
        );
    }

    #[test]
    fn settings_mismatch_names_the_default_when_no_file_sets_it() {
        assert_eq!(
            mismatch(None).to_string(),
            "supdoc.lock was written with line-endings = \"exact\", but the default is \
             \"normalize\""
        );
    }

    #[test]
    fn settings_mismatch_help_offers_update_or_keeping_the_lock_setting() {
        assert_eq!(
            mismatch(None).help(),
            "Run `supdoc update` with no paths and no --state to preview re-recording every link \
             with line-endings = \"normalize\", then add --yes. To keep the lockfile as it is, \
             set [features.hash] line-endings = \"exact\" in supdoc.config.toml"
        );
    }
}
