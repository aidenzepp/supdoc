//! Include and exclude patterns: globs checked while the config is parsed.
//!
//! A [`PatternList`] holds only patterns already known to be valid, so matching never fails.
//! [`AnchoredPatterns`] pairs a list with the directory of the config that wrote it.

use std::path::Path;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

use crate::path::AbsolutePath;

/// Compiled globs, relative to the directory of the config that wrote them (01 D3).
///
/// Every pattern also matches everything beneath what it matches, and `*` never crosses `/`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(try_from = "Vec<String>")]
pub(crate) struct PatternList(Matcher);

/// What a [`PatternList`] matches with.
#[derive(Debug, Clone)]
enum Matcher {
    /// Every path. The default `include` (`**`), without compiling a glob that could fail.
    Everything,
    /// The compiled patterns. An empty set matches nothing.
    Globs {
        /// Two globs per pattern: the pattern, then the pattern with everything beneath it.
        set: GlobSet,
        /// The patterns as written, in order.
        written: Vec<String>,
    },
}

impl PatternList {
    /// The list that matches every path.
    pub(crate) fn everything() -> Self {
        Self(Matcher::Everything)
    }

    /// Whether `relative`, a path beneath the config's directory, matches any pattern.
    pub(crate) fn is_match(&self, relative: &Path) -> bool {
        match &self.0 {
            Matcher::Everything => true,
            Matcher::Globs { set, .. } => set.is_match(relative),
        }
    }

    /// The patterns as written, in order. Empty for the list that matches everything, which no
    /// config file wrote.
    pub(crate) fn written(&self) -> &[String] {
        match &self.0 {
            Matcher::Everything => &[],
            Matcher::Globs { written, .. } => written,
        }
    }

    /// The first pattern, as written, that `relative` matches, or `None` when none does.
    ///
    /// The list that matches everything has no written pattern, so it reports `None` too.
    pub(crate) fn first_match(&self, relative: &Path) -> Option<&str> {
        match &self.0 {
            Matcher::Everything => None,
            Matcher::Globs { set, written } => set
                .matches(relative)
                .into_iter()
                .min()
                .and_then(|glob| written.get(glob / 2))
                .map(String::as_str),
        }
    }
}

impl TryFrom<Vec<String>> for PatternList {
    type Error = PatternError;

    /// For each pattern: rejects a leading `/` and any `..` segment, strips trailing `/`, then
    /// compiles the pattern and the pattern followed by `/**`, both with `*` stopping at `/`.
    fn try_from(raw: Vec<String>) -> Result<Self, Self::Error> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &raw {
            for glob in compile(pattern)? {
                let _added = builder.add(glob);
            }
        }
        let set = builder.build().map_err(|error| PatternError::Invalid {
            pattern: error.glob().unwrap_or_default().to_owned(),
            reason: error.kind().to_string(),
        })?;
        Ok(Self(Matcher::Globs { set, written: raw }))
    }
}

/// Checks one pattern and compiles it together with its "everything beneath" twin.
///
/// # Errors
///
/// A [`PatternError`] naming the pattern and the rule it broke.
fn compile(pattern: &str) -> Result<[Glob; 2], PatternError> {
    if pattern.starts_with('/') {
        return Err(PatternError::Absolute {
            pattern: pattern.to_owned(),
        });
    }
    if pattern.split('/').any(|segment| segment == "..") {
        return Err(PatternError::LeavesDirectory {
            pattern: pattern.to_owned(),
        });
    }
    let trimmed = pattern.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(PatternError::Empty);
    }
    Ok([
        glob(pattern, trimmed)?,
        glob(pattern, &format!("{trimmed}/**"))?,
    ])
}

/// Compiles `source` with `*` unable to cross `/`, reporting errors against `pattern` as written.
///
/// # Errors
///
/// [`PatternError::Invalid`] when the glob syntax is wrong.
fn glob(pattern: &str, source: &str) -> Result<Glob, PatternError> {
    GlobBuilder::new(source)
        .literal_separator(true)
        .build()
        .map_err(|error| PatternError::Invalid {
            pattern: pattern.to_owned(),
            reason: error.kind().to_string(),
        })
}

/// Why a pattern is not accepted in `include` or `exclude`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PatternError {
    /// The pattern starts with `/`.
    #[error("pattern `{pattern}` starts with `/`; write it relative to this config's directory")]
    Absolute {
        /// The rejected pattern, as written.
        pattern: String,
    },
    /// The pattern has a `..` segment.
    #[error("pattern `{pattern}` leaves this config's directory")]
    LeavesDirectory {
        /// The rejected pattern, as written.
        pattern: String,
    },
    /// The pattern is the empty string.
    #[error("pattern is empty; write a path or glob such as `docs`")]
    Empty,
    /// The glob syntax is wrong.
    #[error("invalid glob `{pattern}`: {reason}")]
    Invalid {
        /// The rejected pattern, as written.
        pattern: String,
        /// What the glob compiler reported.
        reason: String,
    },
}

/// A pattern list anchored where it was written.
///
/// Matching strips `base` and tests the rest, so the directory itself is never compiled into a
/// glob. That keeps directories named like `[id]` from being read as glob syntax.
#[derive(Debug, Clone)]
pub(crate) struct AnchoredPatterns {
    /// The config file that wrote the patterns. `None` for the built-in default.
    file: Option<AbsolutePath>,
    /// The directory of the config that wrote the patterns.
    base: AbsolutePath,
    /// The patterns, relative to `base`.
    list: PatternList,
}

impl AnchoredPatterns {
    /// Anchors `list`, written by `file`, at `base`.
    pub(crate) fn written_by(file: AbsolutePath, base: AbsolutePath, list: PatternList) -> Self {
        Self {
            file: Some(file),
            base,
            list,
        }
    }

    /// Anchors the built-in default `list` at `base`, with no file behind it.
    pub(crate) fn built_in(base: AbsolutePath, list: PatternList) -> Self {
        Self {
            file: None,
            base,
            list,
        }
    }

    /// The patterns as written and the file that wrote them, or `None` for the built-in default.
    pub(crate) fn definition(&self) -> Option<PatternDefinition<'_>> {
        self.file.as_ref().map(|file| PatternDefinition {
            patterns: self.list.written(),
            file,
        })
    }

    /// Whether `path` is under `base` and matches a pattern there.
    pub(crate) fn matches(&self, path: &AbsolutePath) -> bool {
        path.strip_prefix(&self.base)
            .is_some_and(|relative| self.list.is_match(relative))
    }

    /// The first pattern, as written, that matches `path`, or `None` when `path` is outside
    /// `base` or no pattern matches.
    pub(crate) fn first_match(&self, path: &AbsolutePath) -> Option<&str> {
        path.strip_prefix(&self.base)
            .and_then(|relative| self.list.first_match(relative))
    }
}

/// The raw `include` or `exclude` patterns of a config, and the file that wrote them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternDefinition<'a> {
    /// The patterns exactly as written, in order.
    patterns: &'a [String],
    /// The config file that set the key, which may be a parent reached through `extends`.
    file: &'a AbsolutePath,
}

impl<'a> PatternDefinition<'a> {
    /// The patterns exactly as written, in order.
    #[must_use]
    pub fn patterns(&self) -> &'a [String] {
        self.patterns
    }

    /// The config file that set the key.
    #[must_use]
    pub fn file(&self) -> &'a AbsolutePath {
        self.file
    }
}
