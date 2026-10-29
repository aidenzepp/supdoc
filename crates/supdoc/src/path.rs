//! Absolute and relative filesystem paths with their invariants checked once, at construction.
//!
//! [`AbsolutePath`] is supdoc's one absolute path type: it is always absolute and lexically
//! normalized, so `.` and `..` never reach code that compares or prefixes paths.
//! [`RelativePath`] is a path written relative to some file, such as a namespace directory or an
//! `extends` target in `supdoc.config.toml`.

use std::fmt;
use std::path::{Component, Path, PathBuf};

/// An absolute path with `.` and `..` removed lexically (no filesystem access).
///
/// Lexical normalization is supdoc's defined meaning of a path; symlinks are not resolved.
/// A `..` at the root stays at the root, the same way the operating system treats `/..`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbsolutePath(PathBuf);

impl AbsolutePath {
    /// Normalizes `path`, or returns `None` if it is relative.
    #[must_use]
    pub fn new(path: &Path) -> Option<Self> {
        if !path.is_absolute() {
            return None;
        }
        let mut normalized = PathBuf::new();
        push_normalized(&mut normalized, path);
        Some(Self(normalized))
    }

    /// Appends `rel` and normalizes the result.
    ///
    /// Infallible: absolute plus relative is always absolute.
    #[must_use]
    pub fn join(&self, rel: &RelativePath) -> Self {
        let mut joined = self.0.clone();
        push_normalized(&mut joined, rel.as_path());
        Self(joined)
    }

    /// The directory containing this path, or `None` at the filesystem root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        self.0.parent().map(|parent| Self(parent.to_path_buf()))
    }

    /// The path as a standard library [`Path`], for filesystem calls.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The part of `self` beneath `base`, or `None` if `self` is not under it.
    ///
    /// Matching is by whole components, so `/repo/docsy` is not under `/repo/docs`.
    #[must_use]
    pub fn strip_prefix(&self, base: &AbsolutePath) -> Option<&Path> {
        self.0.strip_prefix(&base.0).ok()
    }

    /// A displayable form of the path, for messages.
    #[must_use]
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl AsRef<Path> for AbsolutePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Appends `path` to `base` one component at a time, dropping `.` and resolving `..` lexically.
///
/// `base` must already be normalized. A `..` that would climb above the root is dropped, because
/// `PathBuf::pop` leaves a bare root unchanged.
fn push_normalized(base: &mut PathBuf, path: &Path) {
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                base.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let _popped = base.pop();
            }
        }
    }
}

/// A path with no root or prefix component, written relative to some file. May contain `..`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct RelativePath(PathBuf);

impl RelativePath {
    /// The path as a standard library [`Path`].
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// A displayable form of the path, for messages.
    #[must_use]
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl TryFrom<String> for RelativePath {
    type Error = RelativePathError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let path = PathBuf::from(raw);
        let anchored = path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)));
        if anchored {
            return Err(RelativePathError { path });
        }
        Ok(Self(path))
    }
}

/// A path that was expected to be relative was anchored to a root or drive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{}` is an absolute path; write it relative to the file that contains it",
    .path.display()
)]
pub struct RelativePathError {
    /// The rejected path, as written.
    path: PathBuf,
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(raw: &str) -> AbsolutePath {
        AbsolutePath::new(Path::new(raw)).expect("the fixture path should be absolute")
    }

    fn relative(raw: &str) -> RelativePath {
        RelativePath::try_from(raw.to_owned()).expect("the fixture path should be relative")
    }

    #[test]
    fn absolute_path_rejects_relative_input() {
        assert_eq!(AbsolutePath::new(Path::new("src/lib.rs")), None);
    }

    #[test]
    fn absolute_path_new_removes_dot_segments() {
        assert_eq!(
            absolute("/repo/./docs/../src/lib.rs").as_path(),
            Path::new("/repo/src/lib.rs")
        );
    }

    #[test]
    fn absolute_path_join_removes_dot_segments() {
        let joined = absolute("/repo/apps/web").join(&relative("../../services/./auth"));

        assert_eq!(joined.as_path(), Path::new("/repo/services/auth"));
    }

    #[test]
    fn absolute_path_parent_dot_dot_stays_at_root() {
        assert_eq!(
            absolute("/repo").join(&relative("../../..")).as_path(),
            Path::new("/")
        );
    }

    #[test]
    fn absolute_path_strip_prefix_is_none_outside_base() {
        let base = absolute("/repo/docs");

        assert_eq!(absolute("/repo/docsy/a.md").strip_prefix(&base), None);
        assert_eq!(absolute("/elsewhere/a.md").strip_prefix(&base), None);
        assert_eq!(
            absolute("/repo/docs/guide/a.md").strip_prefix(&base),
            Some(Path::new("guide/a.md")),
        );
    }

    #[test]
    fn absolute_path_parent_is_normalized_and_none_at_root() {
        assert_eq!(
            absolute("/repo/src/lib.rs").parent(),
            Some(absolute("/repo/src"))
        );
        assert_eq!(absolute("/").parent(), None);
    }

    #[test]
    fn relative_path_rejects_absolute() {
        let error = RelativePath::try_from("/etc/supdoc.config.toml".to_owned())
            .expect_err("a path with a root should not be accepted as relative");

        assert_eq!(
            error.to_string(),
            "`/etc/supdoc.config.toml` is an absolute path; write it relative to the file that contains it",
        );
    }

    #[test]
    fn relative_path_accepts_parent_segments() {
        assert_eq!(
            relative("../../supdoc.config.toml").as_path(),
            Path::new("../../supdoc.config.toml")
        );
    }

    #[test]
    fn relative_path_deserializes_through_its_check() {
        #[derive(Debug, serde::Deserialize)]
        struct Holder {
            /// The field under test.
            path: RelativePath,
        }

        let ok: Holder = toml::from_str(r#"path = "services/auth""#)
            .expect("a relative path should deserialize");
        let rejected = toml::from_str::<Holder>(r#"path = "/services/auth""#);

        assert_eq!(ok.path, relative("services/auth"));
        assert!(
            rejected.is_err(),
            "an absolute path should fail to deserialize: {rejected:?}"
        );
    }
}
