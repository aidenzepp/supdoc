//! The choices that shape output: its format, how chatty it is, and what text rendering needs.

use crate::finding::Severity;
use crate::output::paths::PathStyle;

/// The format data is written in (06 X5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable, with source excerpts.
    Text,
    /// One JSON document on stdout (see `supdoc schema`).
    Json,
    /// One JSON object per line, for streaming.
    Jsonl,
    /// GitHub Actions annotations, shown on the PR diff.
    Github,
}

impl OutputFormat {
    /// Whether this format is machine-readable JSON, one way or another.
    #[must_use]
    pub const fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::Jsonl)
    }
}

/// How much to say (07 G6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verbosity {
    /// `-q`: errors only, with no summary line.
    Quiet,
    /// The default: errors, warnings, notes and the summary.
    Normal,
    /// `-v`: also lists passed links.
    Verbose,
    /// `-vv`: also run details on stderr.
    Trace,
}

impl Verbosity {
    /// Combines `--quiet` and the number of `--verbose` flags. Quiet wins.
    #[must_use]
    pub const fn from_flags(quiet: bool, verbose: u8) -> Self {
        match (quiet, verbose) {
            (true, _) => Self::Quiet,
            (false, 0) => Self::Normal,
            (false, 1) => Self::Verbose,
            (false, _) => Self::Trace,
        }
    }

    /// Whether a finding of `severity` is shown. Quiet shows only errors.
    #[must_use]
    pub const fn shows(self, severity: Severity) -> bool {
        match severity {
            Severity::Error => true,
            Severity::Warning | Severity::Note => !matches!(self, Self::Quiet),
        }
    }

    /// Whether the summary line is printed.
    #[must_use]
    pub const fn shows_summary(self) -> bool {
        !matches!(self, Self::Quiet)
    }

    /// Whether passed links are listed.
    #[must_use]
    pub const fn lists_passed(self) -> bool {
        matches!(self, Self::Verbose | Self::Trace)
    }

    /// Whether run details are printed to stderr.
    #[must_use]
    pub const fn shows_run_details(self) -> bool {
        matches!(self, Self::Trace)
    }
}

/// Everything text rendering needs, passed in explicitly so rendering touches no global state
/// (07 "Rust design").
#[derive(Debug, Clone)]
pub struct TextContext {
    /// Whether to emit ANSI color.
    pub color: bool,
    /// Whether to draw with Unicode box characters instead of ASCII.
    pub unicode: bool,
    /// The terminal width in columns.
    pub width: usize,
    /// How paths are shown.
    pub paths: PathStyle,
    /// How much to say.
    pub verbosity: Verbosity,
}

/// Whether text output may draw with Unicode box characters: when color is on, and the terminal
/// isn't `TERM=dumb` (07 G12).
#[must_use]
pub fn supports_unicode(color: bool, term: Option<&str>) -> bool {
    color && term != Some("dumb")
}
