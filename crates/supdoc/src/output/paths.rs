//! How a path is shown in each format (07 G7).

use std::path::{Component, Path};

use crate::link::SourcePath;
use crate::path::AbsolutePath;

/// Turns project-relative paths into the form each consumer can use directly.
///
/// Built once in `main` from the current directory and `$GITHUB_WORKSPACE`, with the current
/// directory standing in for the project root until [`PathStyle::set_root`] is told the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStyle {
    /// The current directory: text output is relative to it so terminals can link it.
    cwd: AbsolutePath,
    /// The project root, which source paths are relative to.
    root: AbsolutePath,
    /// `$GITHUB_WORKSPACE`: GitHub annotations are relative to it.
    github_workspace: Option<AbsolutePath>,
}

impl PathStyle {
    /// A style for a run in `cwd` on the project at `root`.
    #[must_use]
    pub fn new(
        cwd: AbsolutePath,
        root: AbsolutePath,
        github_workspace: Option<AbsolutePath>,
    ) -> Self {
        Self {
            cwd,
            root,
            github_workspace,
        }
    }

    /// The project root, shown in the JSON envelope.
    #[must_use]
    pub fn root(&self) -> &AbsolutePath {
        &self.root
    }

    /// Replaces the project root once a command has found it, keeping the current directory and
    /// `$GITHUB_WORKSPACE`.
    pub fn set_root(&mut self, root: AbsolutePath) {
        self.root = root;
    }

    /// A source file as text output shows it: relative to the current directory.
    #[must_use]
    pub fn text(&self, source: &SourcePath) -> String {
        self.text_absolute(&self.absolute(source))
    }

    /// An absolute path as text output shows it: relative to the current directory.
    #[must_use]
    pub fn text_absolute(&self, path: &AbsolutePath) -> String {
        relative_to(&self.cwd, path)
    }

    /// A source file as a GitHub annotation shows it: relative to `$GITHUB_WORKSPACE`, or to the
    /// project root when the variable isn't set.
    ///
    /// A path outside the workspace stays absolute, because a relative one could not name it.
    #[must_use]
    pub fn github(&self, source: &SourcePath) -> String {
        let base = self.github_workspace.as_ref().unwrap_or(&self.root);
        let absolute = self.absolute(source);
        match absolute.strip_prefix(base) {
            Some(inside) => shown(inside),
            None => shown(absolute.as_path()),
        }
    }

    /// A source file as JSON shows it: relative to the project root, like `supdoc.lock`.
    #[must_use]
    pub fn json(source: &SourcePath) -> String {
        source.as_str().to_owned()
    }

    /// The absolute location of a project-relative source file.
    fn absolute(&self, source: &SourcePath) -> AbsolutePath {
        // A source path has no `..`, so joining it cannot leave the root.
        AbsolutePath::new(&self.root.as_path().join(source.as_str()))
            .unwrap_or_else(|| self.root.clone())
    }
}

/// `target` written relative to the directory `base`, climbing with `..` where it lies outside,
/// with `/` between segments on every platform. `.` when the two are the same.
///
/// The one place supdoc turns an absolute path into a relative one for display: text output
/// passes the current directory as `base`, and JSON and finding messages pass the project root.
#[must_use]
pub(crate) fn relative_to(base: &AbsolutePath, target: &AbsolutePath) -> String {
    let base_parts: Vec<Component<'_>> = base.as_path().components().collect();
    let target_parts: Vec<Component<'_>> = target.as_path().components().collect();
    let shared = base_parts
        .iter()
        .zip(&target_parts)
        .take_while(|(left, right)| left == right)
        .count();
    let climbs = std::iter::repeat_n("..".to_owned(), base_parts.len() - shared);
    let descents = target_parts
        .iter()
        .skip(shared)
        .map(|part| part.as_os_str().to_string_lossy().into_owned());
    let parts: Vec<String> = climbs.chain(descents).collect();
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

/// A path as text. Paths are shown as the operating system writes them.
fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(raw: &str) -> AbsolutePath {
        AbsolutePath::new(Path::new(raw)).expect("the fixture path should be absolute")
    }

    #[test]
    fn relative_to_descends_climbs_and_joins_with_slashes() {
        let root = absolute("/repo");

        assert_eq!(
            relative_to(&root, &absolute("/repo/src/lib.rs")),
            "src/lib.rs"
        );
        assert_eq!(
            relative_to(&absolute("/repo/docs"), &absolute("/repo/src/lib.rs")),
            "../src/lib.rs"
        );
        assert_eq!(
            relative_to(&root, &absolute("/elsewhere/a.md")),
            "../elsewhere/a.md"
        );
        assert_eq!(relative_to(&root, &root), ".");
    }
}
