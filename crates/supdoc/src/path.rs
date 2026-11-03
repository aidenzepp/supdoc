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
pub(crate) struct AbsolutePath(PathBuf);

impl AbsolutePath {
    /// Normalizes `path`, or returns `None` if it is relative.
    #[must_use]
    pub(crate) fn new(path: &Path) -> Option<Self> {
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
    pub(crate) fn join(&self, rel: &RelativePath) -> Self {
        let mut joined = self.0.clone();
        push_normalized(&mut joined, rel.as_path());
        Self(joined)
    }

    /// Appends `path` and normalizes the result. An absolute `path` replaces `self`, as
    /// [`Path::join`] does.
    ///
    /// Infallible: the result is absolute either way, so no caller needs a fallback.
    #[must_use]
    pub(crate) fn join_path(&self, path: &Path) -> Self {
        let mut joined = if path.is_absolute() {
            PathBuf::new()
        } else {
            self.0.clone()
        };
        push_normalized(&mut joined, path);
        Self(joined)
    }

    /// The directory containing this path, or `None` at the filesystem root.
    #[must_use]
    pub(crate) fn parent(&self) -> Option<Self> {
        self.0.parent().map(|parent| Self(parent.to_path_buf()))
    }

    /// The path as a standard library [`Path`], for filesystem calls.
    #[must_use]
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    /// The part of `self` beneath `base`, or `None` if `self` is not under it.
    ///
    /// Matching is by whole components, so `/repo/docsy` is not under `/repo/docs`.
    #[must_use]
    pub(crate) fn strip_prefix(&self, base: &AbsolutePath) -> Option<&Path> {
        self.0.strip_prefix(&base.0).ok()
    }

    /// A displayable form of the path, for messages.
    #[must_use]
    pub(crate) fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl serde::Serialize for AbsolutePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0.display())
    }
}

impl schemars::JsonSchema for AbsolutePath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AbsolutePath".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "An absolute, lexically normalized filesystem path.",
        })
    }
}

impl AsRef<Path> for AbsolutePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Whether `path` is a regular file once symlinks are followed, so a directory, FIFO, socket or
/// device is refused before it is opened. Opening a FIFO blocks until a writer appears, so a
/// reader that skips this check can hang forever.
///
/// Effect: reads the metadata of `path` and of every symlink on the way to it.
///
/// # Errors
///
/// Fails if that metadata can't be read, including `NotFound` when nothing is there.
pub(crate) fn is_regular_file(path: &Path) -> std::io::Result<bool> {
    std::fs::metadata(path).map(|metadata| metadata.is_file())
}

/// Whether `dir` is the top of a git work tree, as git decides it: `dir/.git`, followed through
/// symlinks, is a file (a gitfile, which git either uses or refuses to run past), or a git
/// directory ([`is_git_dir`]). A missing or dangling `.git`, or a directory git rejects, isn't, and
/// git searches further up.
///
/// Effect: reads file metadata in `dir` and its `.git` directory.
pub(crate) fn holds_git(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    match std::fs::metadata(&dot_git) {
        Err(_) => false,
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => is_git_dir(&dot_git),
    }
}

/// Whether `git_dir` has the layout git accepts as a repository: a `HEAD` that is a file or a
/// symlink, not a directory, and `objects` and `refs` directories, followed through symlinks.
/// `HEAD` isn't read, so whether it names a commit, which git also checks, goes unchecked: above
/// the root it lies outside the project, and supdoc reads no outside file but its config and
/// ignore inputs.
///
/// Effect: reads file metadata in `git_dir`.
pub(crate) fn is_git_dir(git_dir: &Path) -> bool {
    std::fs::symlink_metadata(git_dir.join("HEAD"))
        .is_ok_and(|metadata| metadata.is_file() || metadata.is_symlink())
        && std::fs::metadata(git_dir.join("objects")).is_ok_and(|metadata| metadata.is_dir())
        && std::fs::metadata(git_dir.join("refs")).is_ok_and(|metadata| metadata.is_dir())
}

/// Where a project file such as `supdoc.config.toml` or `supdoc.lock` really is, judged against
/// the project root: the directory the file's path names it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectFile {
    /// Nothing is at the path.
    Absent,
    /// The file to read and write: the path itself, or the real file a symlink there leads to
    /// inside the root. Writing here keeps the symlink.
    Inside(AbsolutePath),
    /// A symlink whose real file is outside the root, with that real file, or `None` when the
    /// symlink leads nowhere (dangling or looping).
    Outside(Option<AbsolutePath>),
}

/// Resolves the project file at `path`, whose directory is the project root, so a command reads
/// and rewrites the same real file and never writes outside the root. A path that isn't a
/// symlink is its own file, whatever kind it is; a symlink is followed and its real file kept
/// only when it lies inside the root once both are resolved.
///
/// Effect: reads the metadata of `path` and, for a symlink, every link along it and its root.
///
/// # Errors
///
/// Fails if `path`'s own metadata can't be read for a reason other than its absence.
pub(crate) fn resolve_project_file(path: &AbsolutePath) -> std::io::Result<ProjectFile> {
    let metadata = match std::fs::symlink_metadata(path.as_path()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectFile::Absent);
        }
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_symlink() {
        return Ok(ProjectFile::Inside(path.clone()));
    }
    let real = std::fs::canonicalize(path.as_path())
        .ok()
        .and_then(|real| AbsolutePath::new(&real));
    let root = path
        .as_path()
        .parent()
        .and_then(|root| std::fs::canonicalize(root).ok());
    Ok(match (real, root) {
        (Some(real), Some(root)) if real.as_path().starts_with(&root) => ProjectFile::Inside(real),
        (real, _) => ProjectFile::Outside(real),
    })
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
pub(crate) struct RelativePath(PathBuf);

impl RelativePath {
    /// The path as a standard library [`Path`].
    #[must_use]
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
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
pub(crate) struct RelativePathError {
    /// The rejected path, as written.
    path: PathBuf,
}

impl serde::Serialize for RelativePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0.display())
    }
}

impl schemars::JsonSchema for RelativePath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RelativePath".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A filesystem path with no root or drive, written relative to some file.",
        })
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

/// The path grammar a link destination and a code block's `from(...)` or `uses(...)` share.
///
/// Splits `prefix:path` when `prefix` is written before any `/`; `docs/a:b.md` has no prefix.
#[must_use]
pub(crate) fn split_namespace(text: &str) -> Option<(&str, &str)> {
    let (prefix, path) = text.split_once(':')?;
    (!prefix.contains('/')).then_some((prefix, path))
}

/// Decodes `%XX` escapes. A `%` not followed by two hex digits stays as written, as in `CommonMark`.
/// `None` when the decoded bytes are not UTF-8.
pub(crate) fn percent_decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut remaining = text.bytes();
    while let Some(byte) = remaining.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let mut lookahead = remaining.clone();
        let decoded = lookahead
            .next()
            .and_then(hex_value)
            .zip(lookahead.next().and_then(hex_value))
            .map(|(high, low)| high * 16 + low);
        match decoded {
            Some(value) => {
                bytes.push(value);
                remaining = lookahead;
            }
            None => bytes.push(byte),
        }
    }
    String::from_utf8(bytes).ok()
}

/// The value of one ASCII hex digit.
fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
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
    fn join_path_appends_a_relative_path_and_normalizes_it() {
        assert_eq!(
            absolute("/repo/docs").join_path(Path::new("../src/./lib.rs")),
            absolute("/repo/src/lib.rs")
        );
    }

    #[test]
    fn join_path_with_an_absolute_path_replaces_the_base() {
        assert_eq!(
            absolute("/repo").join_path(Path::new("/elsewhere/../etc/x.toml")),
            absolute("/etc/x.toml")
        );
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

    /// A fresh directory holding `project/` and `outside/`, both resolved, so paths compare
    /// equal to what `canonicalize` returns.
    fn two_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("a fresh temp directory should be created");
        let base = std::fs::canonicalize(dir.path()).expect("the temp directory should resolve");
        let (project, outside) = (base.join("project"), base.join("outside"));
        for made in [&project, &outside] {
            std::fs::create_dir_all(made.join("state")).expect("the fixture dirs should be made");
            std::fs::write(made.join("state/supdoc.lock"), "version = 1\n")
                .expect("the fixture file should be written");
        }
        (dir, project, outside)
    }

    fn resolved(path: &Path) -> ProjectFile {
        let path = AbsolutePath::new(path).expect("the fixture path should be absolute");
        resolve_project_file(&path).expect("the fixture path's metadata should be readable")
    }

    #[cfg(unix)]
    #[test]
    fn resolve_project_file_follows_an_in_root_symlink_to_its_real_file() {
        let (_dir, project, _outside) = two_dirs();
        let link = project.join("supdoc.lock");
        std::os::unix::fs::symlink("state/supdoc.lock", &link)
            .expect("the symlink should be creatable");

        assert_eq!(
            resolved(&link),
            ProjectFile::Inside(
                AbsolutePath::new(&project.join("state/supdoc.lock"))
                    .expect("the real path should be absolute")
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_project_file_names_the_real_file_of_a_symlink_out_of_the_root() {
        let (_dir, project, outside) = two_dirs();
        let link = project.join("supdoc.lock");
        std::os::unix::fs::symlink(outside.join("state/supdoc.lock"), &link)
            .expect("the symlink should be creatable");

        assert_eq!(
            resolved(&link),
            ProjectFile::Outside(AbsolutePath::new(&outside.join("state/supdoc.lock")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_project_file_counts_a_dangling_symlink_as_outside_with_no_real_file() {
        let (_dir, project, _outside) = two_dirs();
        let link = project.join("supdoc.lock");
        std::os::unix::fs::symlink("state/missing.lock", &link)
            .expect("the symlink should be creatable");

        assert_eq!(resolved(&link), ProjectFile::Outside(None));
    }

    #[test]
    fn resolve_project_file_keeps_a_plain_file_and_reports_a_missing_one_absent() {
        let (_dir, project, _outside) = two_dirs();
        let plain = project.join("state/supdoc.lock");

        assert_eq!(
            resolved(&plain),
            ProjectFile::Inside(AbsolutePath::new(&plain).expect("the path should be absolute"))
        );
        assert_eq!(resolved(&project.join("supdoc.lock")), ProjectFile::Absent);
    }
}
