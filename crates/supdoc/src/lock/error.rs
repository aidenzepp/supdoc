//! Lockfile errors: what failed, a stable code, and what to do next (02 Errors).

use crate::link::LinkKey;
use crate::path::AbsolutePath;

/// Exit status for a lockfile the user fixes by editing it (06 X7).
const EXIT_CONFIG: u8 = 78;

/// Exit status for a lockfile that can't be read or written (06 X7).
const EXIT_IO: u8 = 74;

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

impl ParseLockError {
    /// A stable identifier for machine-readable output.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ConflictMarkers { .. } => "lock/conflict",
            Self::NewerVersion { .. } => "lock/newer-version",
            Self::Toml { .. } => "lock/parse",
            Self::ConflictingDuplicate { .. } => "lock/duplicate",
        }
    }

    /// What to do about it.
    #[must_use]
    pub const fn help(&self) -> &'static str {
        match self {
            Self::ConflictMarkers { .. } => {
                "Resolve the conflict, keeping both sides' entries, then run `supdoc check`. \
                 supdoc re-sorts the file on its next write"
            }
            Self::NewerVersion { .. } => {
                "Upgrade supdoc. This lockfile was written by a newer release"
            }
            Self::Toml { .. } => {
                "Fix that line or delete the entry, then run `supdoc check`. \
                 A deleted entry is re-added the next time links are locked"
            }
            Self::ConflictingDuplicate { .. } => {
                "A merge probably kept both sides. Delete one entry and run `supdoc check`. \
                 If the doc is now stale, `check` will say so"
            }
        }
    }
}

/// Why the lockfile on disk could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// The file exists but could not be read.
    #[error("cannot read {}: {error}", .path.display())]
    Read {
        /// The lockfile.
        path: AbsolutePath,
        /// The underlying I/O failure.
        error: std::io::Error,
    },

    /// The file was read but its text is not a valid lockfile.
    #[error("{}: {error}", .path.display())]
    Parse {
        /// The lockfile.
        path: AbsolutePath,
        /// Why the text was rejected.
        error: Box<ParseLockError>,
    },

    /// The lockfile could not be written. The previous one, if any, is unchanged.
    #[error("cannot write {}: {error}", .path.display())]
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
            Self::Read { .. } => "lock/read",
            Self::Parse { error, .. } => error.code(),
            Self::Write { .. } => "lock/write",
        }
    }

    /// What to do about it.
    #[must_use]
    pub fn help(&self) -> &'static str {
        match self {
            Self::Read { .. } => "Give the current user read access",
            Self::Parse { error, .. } => error.help(),
            Self::Write { .. } => {
                "The existing lockfile was not changed. Make the directory writable and rerun"
            }
        }
    }

    /// The process exit status for this error (06 X7): 78 when the text is invalid, else 74.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Parse { .. } => EXIT_CONFIG,
            Self::Read { .. } | Self::Write { .. } => EXIT_IO,
        }
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
            ["lock/conflict", "lock/newer-version", "lock/duplicate"]
        );
        assert!(
            errors.iter().all(|error| !error.help().is_empty()),
            "every parse error should say what to do next"
        );
    }
}
