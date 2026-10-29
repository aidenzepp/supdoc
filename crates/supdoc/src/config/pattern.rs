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
#[derive(Debug, serde::Deserialize)]
#[serde(try_from = "Vec<String>")]
pub(crate) struct PatternList(Matcher);

/// What a [`PatternList`] matches with.
#[derive(Debug)]
enum Matcher {
    /// Every path. The default `include` (`**`), without compiling a glob that could fail.
    Everything,
    /// The compiled patterns. An empty set matches nothing.
    Globs(GlobSet),
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
            Matcher::Globs(set) => set.is_match(relative),
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
        Ok(Self(Matcher::Globs(set)))
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
#[derive(Debug)]
pub(crate) struct AnchoredPatterns {
    /// The directory of the config that wrote the patterns.
    base: AbsolutePath,
    /// The patterns, relative to `base`.
    list: PatternList,
}

impl AnchoredPatterns {
    /// Anchors `list` at `base`.
    pub(crate) fn new(base: AbsolutePath, list: PatternList) -> Self {
        Self { base, list }
    }

    /// Whether `path` is under `base` and matches a pattern there.
    pub(crate) fn matches(&self, path: &AbsolutePath) -> bool {
        path.strip_prefix(&self.base)
            .is_some_and(|relative| self.list.is_match(relative))
    }
}
