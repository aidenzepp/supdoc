//! The process exit status: the one place the exit values 0, 1, 2, 70, 74 and 78 are defined (06 X7).

/// How a supdoc run ends, as the process exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Nothing is wrong. Exit status 0.
    Clean,
    /// The command ran, and what it checks is failing or a change was refused. Exit status 1.
    Failed,
    /// A usage error, such as a missing `[PATH]`. Exit status 2.
    Usage,
    /// A supdoc bug. Exit status 70.
    Bug,
    /// An I/O failure. Exit status 74.
    Io,
    /// A config or lockfile problem. Exit status 78.
    Config,
}

impl ExitStatus {
    /// The number the process exits with.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Clean => 0,
            Self::Failed => 1,
            Self::Usage => 2,
            Self::Bug => 70,
            Self::Io => 74,
            Self::Config => 78,
        }
    }
}
