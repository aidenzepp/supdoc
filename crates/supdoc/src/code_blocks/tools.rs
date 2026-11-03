//! The tools supdoc knows how to call on a block, the environment they get, and finding each
//! one's binary on `PATH`.
//!
//! A tool is chosen only by its name in [`TOOLS`]; no path or command ever comes from config.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::config::code_blocks::Language;

/// The variables a tool gets from supdoc's own environment, and no others.
const PASSED: [&str; 5] = [
    "PATH",
    "HOME",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "XDG_CACHE_HOME",
];

/// The environment tools get, read once by `main` and passed down like the skill directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolEnv {
    /// Each passed variable that is set, with its value, in [`PASSED`] order.
    vars: Vec<(&'static str, OsString)>,
}

impl ToolEnv {
    /// Reads `PATH`, `HOME`, `RUSTUP_HOME`, `CARGO_HOME` and `XDG_CACHE_HOME` through `var`.
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            vars: PASSED
                .into_iter()
                .filter_map(|name| var(name).map(|value| (name, value)))
                .collect(),
        }
    }

    /// `PATH`, when it is set.
    fn path(&self) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(name, _)| *name == "PATH")
            .map(|(_, value)| value.as_os_str())
    }
}

/// Which check a tool does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Rewrites the block into its formatted form.
    Format,
}

/// What a tool loads, which decides whether it may run in a checkout supdoc doesn't trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Reads only the block on stdin and plain-data config: runs in any checkout.
    Static,
}

/// One tool supdoc knows how to call safely.
#[derive(Debug)]
pub(crate) struct Tool {
    /// The name config uses for it.
    pub(crate) name: &'static str,
    /// The language whose blocks it checks.
    pub(crate) language: Language,
    /// The check it does.
    pub(crate) step: Step,
    /// The binary looked up on `PATH`.
    pub(crate) binary: &'static str,
    /// What it loads.
    pub(crate) tier: Tier,
    /// The command that installs it.
    pub(crate) install: &'static str,
}

/// The known tools. A language's first row for a step is its default for that step.
pub(crate) const TOOLS: [Tool; 4] = [
    Tool {
        name: "rustfmt",
        language: Language::Rust,
        step: Step::Format,
        binary: "rustfmt",
        tier: Tier::Static,
        install: "rustup component add rustfmt",
    },
    Tool {
        name: "shfmt",
        language: Language::Shell,
        step: Step::Format,
        binary: "shfmt",
        tier: Tier::Static,
        install: "go install mvdan.cc/sh/v3/cmd/shfmt@latest",
    },
    Tool {
        name: "ruff-format",
        language: Language::Python,
        step: Step::Format,
        binary: "ruff",
        tier: Tier::Static,
        install: "pipx install ruff",
    },
    Tool {
        name: "black",
        language: Language::Python,
        step: Step::Format,
        binary: "black",
        tier: Tier::Static,
        install: "pipx install black",
    },
];

/// `language`'s default formatter: its first static format row. No trust exists yet, so only
/// the static tier is chosen.
#[must_use]
pub(crate) fn formatter(language: Language) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| {
        tool.language == language && tool.step == Step::Format && tool.tier == Tier::Static
    })
}

/// Where a tool's binary was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    /// On `PATH`, outside the project root, at the real path held: the path a tool is run by.
    OnPath(PathBuf),
    /// On `PATH`, but its real path is inside the project root, so the checkout chose it.
    InsideRoot(PathBuf),
    /// Not on `PATH`.
    Missing,
}

/// Looks `binary` up on `env`'s `PATH`, judging the first match's real path against `root`.
///
/// Effect: reads file metadata along `PATH` and resolves symlinks.
#[must_use]
pub(crate) fn find(binary: &str, env: &ToolEnv, root: &Path) -> Found {
    let Some(real) = env.path().and_then(|path| first_on_path(binary, path)) else {
        return Found::Missing;
    };
    if inside(&real, root) {
        Found::InsideRoot(real)
    } else {
        Found::OnPath(real)
    }
}

/// The `PATH` entries a lookup searches, in order: the absolute ones. An empty entry (the
/// current directory to a shell) and every relative one are skipped.
fn search_dirs(path: &OsStr) -> impl Iterator<Item = PathBuf> {
    std::env::split_paths(path).filter(|dir| dir.is_absolute())
}

/// The real path of the first executable regular file named `binary` in [`search_dirs`].
///
/// Effect: reads file metadata and resolves symlinks.
fn first_on_path(binary: &str, path: &OsStr) -> Option<PathBuf> {
    search_dirs(path)
        .map(|dir| dir.join(binary))
        .filter(|candidate| is_executable_file(candidate))
        .find_map(|candidate| candidate.canonicalize().ok())
}

/// Whether `real`, a resolved path, is inside `root`: under the root's real path by whole
/// components, or under a directory that is the root's own file however it is spelled.
///
/// Effect: reads the metadata of `root` and of each ancestor of `real`.
fn inside(real: &Path, root: &Path) -> bool {
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    real.starts_with(&real_root) || under_same_file(real, root)
}

/// Whether some ancestor of `real` has the same device and inode as `root`, so a firmlink or
/// bind-mount spelling of the root counts as the root.
///
/// Effect: reads the metadata of `root` and of each ancestor of `real`.
#[cfg(unix)]
fn under_same_file(real: &Path, root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(root) = std::fs::metadata(root) else {
        return false;
    };
    real.ancestors()
        .filter_map(|ancestor| std::fs::metadata(ancestor).ok())
        .any(|meta| meta.dev() == root.dev() && meta.ino() == root.ino())
}

/// Whether some ancestor of `real` is `root` by file identity: never known off Unix.
#[cfg(not(unix))]
fn under_same_file(_real: &Path, _root: &Path) -> bool {
    false
}

/// Whether `path`, following symlinks, is a regular file someone may execute.
///
/// Effect: reads the file's metadata.
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && executable(&meta))
}

/// Whether `meta`'s permissions let someone execute the file.
#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

/// Whether `meta`'s permissions let someone execute the file: any regular file off Unix.
#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::Path;

    use super::{Found, ToolEnv, find, search_dirs, under_same_file};

    /// Writes an executable `#!/bin/sh` stub named `name` in `dir`.
    fn stub(dir: &Path, name: &str) {
        fs::create_dir_all(dir).expect("the stub directory should be creatable");
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").expect("the stub should be writable");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("the stub should be made executable");
    }

    /// A [`ToolEnv`] whose `PATH` is `entries` joined.
    fn env_with_path(entries: &[&Path]) -> ToolEnv {
        let path = std::env::join_paths(entries).expect("the fixture PATH should join");
        ToolEnv::from_vars(|name| (name == "PATH").then(|| path.clone()))
    }

    /// A fresh directory holding a `root` project directory and an `outside` one.
    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().expect("a temporary directory should be creatable");
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).expect("the root should be creatable");
        fs::create_dir_all(&outside).expect("the outside directory should be creatable");
        (dir, root, outside)
    }

    #[test]
    fn a_relative_entry_is_skipped() {
        let (_dir, root, _outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        let env = ToolEnv::from_vars(|name| (name == "PATH").then(|| OsString::from("bin")));

        assert_eq!(find("shfmt", &env, &root), Found::Missing);
    }

    #[test]
    fn a_file_that_is_not_executable_is_skipped() {
        let (_dir, root, outside) = dirs();
        fs::write(outside.join("shfmt"), "#!/bin/sh\n").expect("the file should be writable");

        assert_eq!(
            find("shfmt", &env_with_path(&[&outside]), &root),
            Found::Missing
        );
    }

    #[test]
    fn a_symlink_outside_whose_real_path_is_in_the_root_is_refused() {
        let (_dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        symlink(root.join("bin/shfmt"), outside.join("shfmt"))
            .expect("the fixture symlink should be creatable");

        let found = find("shfmt", &env_with_path(&[&outside]), &root);

        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
    }

    #[test]
    fn a_tool_on_no_entry_is_missing() {
        let (_dir, root, outside) = dirs();

        assert_eq!(
            find("shfmt", &env_with_path(&[&outside]), &root),
            Found::Missing
        );
        assert_eq!(
            find("shfmt", &ToolEnv::from_vars(|_| None), &root),
            Found::Missing
        );
    }

    #[test]
    fn the_first_match_on_path_wins() {
        let (_dir, root, outside) = dirs();
        stub(&outside, "shfmt");
        stub(&root.join("bin"), "shfmt");
        let real = outside
            .join("shfmt")
            .canonicalize()
            .expect("the stub should resolve");

        assert_eq!(
            find(
                "shfmt",
                &env_with_path(&[&outside, &root.join("bin")]),
                &root
            ),
            Found::OnPath(real)
        );
        assert!(matches!(
            find(
                "shfmt",
                &env_with_path(&[&root.join("bin"), &outside]),
                &root
            ),
            Found::InsideRoot(_)
        ));
    }

    #[test]
    fn an_empty_entry_and_dot_are_not_searched() {
        let dirs: Vec<_> = search_dirs(OsStr::new(":.::./bin:/usr/bin:")).collect();

        assert_eq!(dirs, [Path::new("/usr/bin")]);
    }

    #[test]
    fn a_path_directory_that_is_a_symlink_into_the_root_is_refused() {
        let (_dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        symlink(root.join("bin"), outside.join("bin"))
            .expect("the fixture symlink should be creatable");

        let found = find("shfmt", &env_with_path(&[&outside.join("bin")]), &root);

        assert!(matches!(found, Found::InsideRoot(_)), "{found:?}");
    }

    #[test]
    fn a_root_reached_through_a_symlink_is_judged_by_its_real_path() {
        let (dir, root, outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        stub(&outside, "ruff");
        let alias = dir.path().join("alias");
        symlink(&root, &alias).expect("the fixture symlink should be creatable");

        let inside = find("shfmt", &env_with_path(&[&root.join("bin")]), &alias);
        let elsewhere = find("ruff", &env_with_path(&[&outside]), &alias);

        assert!(matches!(inside, Found::InsideRoot(_)), "{inside:?}");
        assert!(matches!(elsewhere, Found::OnPath(_)), "{elsewhere:?}");
    }

    #[test]
    fn a_sibling_root_sharing_the_name_prefix_is_not_inside() {
        let (dir, root, _outside) = dirs();
        let sibling = dir.path().join("root2");
        stub(&sibling.join("bin"), "shfmt");

        let found = find("shfmt", &env_with_path(&[&sibling.join("bin")]), &root);

        assert!(matches!(found, Found::OnPath(_)), "{found:?}");
    }

    #[test]
    fn a_same_inode_alias_of_the_root_is_inside() {
        let (dir, root, _outside) = dirs();
        stub(&root.join("bin"), "shfmt");
        let alias = dir.path().join("alias");
        symlink(&root, &alias).expect("the fixture symlink should be creatable");
        let real = root
            .join("bin/shfmt")
            .canonicalize()
            .expect("the stub should resolve");

        assert!(
            !real.starts_with(&alias),
            "the alias should not be a path prefix"
        );
        assert!(under_same_file(&real, &alias));
    }
}
