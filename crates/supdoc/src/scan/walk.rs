//! Walking the project tree to find the files a scan reads links from.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::{DirEntry, WalkBuilder};

use crate::config::{CONFIG_FILE_NAME, Config, config_entry};
use crate::finding::{Finding, Severity};
use crate::link::{LinkPathError, SourcePath};
use crate::lock::error::shell_quote;
use crate::output::paths::relative_to;
use crate::path::{AbsolutePath, Pointer};
use crate::read::cartridge::Languages;
use crate::scan::explicit::explicit_path_warnings;
pub(crate) use crate::scan::ignore::CODE_IGNORE_TOO_LARGE;
use crate::scan::ignore::{IgnoreRules, symlinked_ignore_file, unreadable_ignore_file};
use crate::scan::model::Scope;

/// Directories of version-control systems, never walked.
const VCS_DIRS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];

/// The files a scan reads links from, and the problems met while finding them.
#[derive(Debug)]
pub(crate) struct Walk {
    /// Sorted by path. Every entry is under the root, is wanted by linking or code-blocks
    /// ([`Wants`]), is inside the [`Scope`], and has a cartridge.
    pub(crate) sources: Vec<WalkedSource>,
    /// Unreadable directories, symlinked ignore files that weren't read, and explicitly named
    /// paths that are excluded or ignored, sorted.
    pub(crate) findings: Vec<Finding>,
    /// The directories the walk couldn't read and linking would have walked, sorted by path.
    /// Nothing beneath one was walked, so its sources and their links are unknown.
    pub(crate) unread_dirs: Vec<UnreadDir>,
    /// The directories the walk couldn't read that only code-blocks walks, because linking's
    /// `exclude` names them, sorted by path. Kept apart, with no finding in `findings`, so a wider
    /// code-blocks scope never changes linking's results.
    pub(crate) code_blocks_unread_dirs: Vec<UnreadDir>,
}

/// A file the walk found to read links from.
#[derive(Debug)]
pub(crate) struct WalkedSource {
    /// Its path relative to the root, as links name it.
    pub(crate) source: SourcePath,
    /// Its path on disk.
    pub(crate) path: AbsolutePath,
    /// Which features read it: at least one does.
    pub(crate) wants: Wants,
}

/// Which features read a walked source. Each feature has its own `include` and `exclude`, and the
/// walk covers the union of their scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wants {
    /// Linking reads its links: [`Config::scans`] holds.
    pub(crate) links: bool,
    /// Code-blocks reads its code blocks: the feature is on and
    /// [`CodeBlocks::scans`](crate::config::code_blocks::CodeBlocks::scans) holds.
    pub(crate) code_blocks: bool,
}

impl Wants {
    /// Which features read `path`, by their `include` and `exclude` alone.
    fn of(config: &Config, path: &AbsolutePath) -> Self {
        Self {
            links: config.scans(path),
            code_blocks: config
                .code_blocks()
                .is_some_and(|code_blocks| code_blocks.scans(path)),
        }
    }
}

/// A directory the walk couldn't read, so nothing beneath it was walked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnreadDir {
    /// The directory, as the walk met it.
    pub(crate) path: AbsolutePath,
    /// Why it wasn't read.
    pub(crate) cause: UnreadCause,
}

/// Why the walk didn't read a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UnreadCause {
    /// Listing it failed, with what the operating system reported, such as `permission denied`.
    Io(io::ErrorKind),
}

impl fmt::Display for UnreadCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(kind) => write!(formatter, "{kind}"),
        }
    }
}

impl UnreadDir {
    /// The `scan-unreadable-dir` warning for this directory.
    ///
    /// The message names the directory relative to the project `root`, as the read messages name
    /// files, so no finding carries a machine-specific absolute path.
    pub(crate) fn finding(&self, root: &AbsolutePath) -> Finding {
        let shown = relative_to(root, &self.path);
        self.warning(&shown).with_help(format!(
            "Links in that directory weren't checked. Run `chmod u+rx {dir}`, or add `exclude = \
             [{pattern:?}]` to {CONFIG_FILE_NAME}",
            dir = shell_quote(&shown),
            pattern = shown,
        ))
    }

    /// The `scan-unreadable-dir` warning for this directory when only code-blocks walks it:
    /// linking's `exclude` already names it, so the help offers code-blocks' own `exclude`.
    pub(crate) fn code_blocks_finding(&self, root: &AbsolutePath) -> Finding {
        let shown = relative_to(root, &self.path);
        self.warning(&shown).with_help(format!(
            "Code blocks in that directory weren't checked. Run `chmod u+rx {dir}`, or add \
             {pattern:?} to `[features.code-blocks] exclude` in {CONFIG_FILE_NAME}",
            dir = shell_quote(&shown),
            pattern = shown,
        ))
    }

    /// The `scan-unreadable-dir` warning, with no help, naming the directory as `shown`.
    fn warning(&self, shown: &str) -> Finding {
        Finding::new(
            "scan-unreadable-dir",
            Severity::Warning,
            format!("skipped {shown}: {}", self.cause),
        )
    }
}

/// The git global excludes file, such as `~/.config/git/ignore`, found once by `main` so the walk
/// reads no git config itself and an in-process run reads none of the user's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GlobalExcludes {
    /// The file, whether or not it exists. `None` when there is no home directory to find it in.
    /// Kept as git names it, not lexically normalized, so the operating system resolves a `..`
    /// after a symlink as git does.
    pub(super) file: Option<PathBuf>,
}

impl GlobalExcludes {
    /// No global excludes file, for runs that must not depend on the user's git config.
    #[must_use]
    pub(crate) const fn none() -> Self {
        Self { file: None }
    }

    /// The file git reads as the global excludes, found as the walker's own lookup finds it:
    /// `core.excludesFile` from `GIT_CONFIG_GLOBAL`, `~/.gitconfig`, the XDG git config or the
    /// system git config, else `$XDG_CONFIG_HOME/git/ignore`, else `~/.config/git/ignore`. A
    /// relative path counts from `cwd`, joined without removing `..`, as git opens it. Whether it
    /// exists is left to the walk, where a missing file has no rules.
    ///
    /// Effect: reads environment variables and the user's and system git config.
    #[must_use]
    pub(crate) fn find(cwd: &AbsolutePath) -> Self {
        Self {
            file: ignore::gitignore::gitconfig_excludes_path().map(|file| {
                // A path the typed pointer refuses, such as one holding a NUL byte, is kept so
                // opening it fails and the walk warns that the global excludes file is unreadable.
                match Pointer::from_path(file.clone()) {
                    Some(pointer) => pointer.resolve_from(cwd.as_path()),
                    None => cwd.as_path().join(file),
                }
            }),
        }
    }
}

/// Walks the project beneath `config.root()` and returns its sources.
///
/// The rules apply in order to each entry: ignore files ([`IgnoreRules`]: each
/// directory's `.ignore`, `.gitignore` and `.git/info/exclude`, those of the directories above the
/// root up to the top of its git work tree, and the global `excludes` file, even outside a git
/// repository), version-control directories, nested projects, symlinks, then `Config::scans` and
/// the cartridge check. Hidden files are walked. Targets are never looked up here, so none of
/// these rules apply to them.
///
/// A directory that can't be read becomes a finding and the walk continues, including when it is
/// the root itself: the caller decides whether an unreadable root is fatal. A `.ignore` or
/// `.gitignore` inside the root that is a symlink is not read, and becomes a finding too, as does
/// an ignore file that exists but can't be read, inside the root or above it.
///
/// Effect: reads directory listings, ignore files and file metadata.
#[must_use]
pub(crate) fn walk(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    excludes: &GlobalExcludes,
) -> Walk {
    let root = config.root();
    let rules = Arc::new(IgnoreRules::new(
        root,
        excludes,
        config.limits_in_effect().max_ignore_size.value,
    ));
    let mut sources = Vec::new();
    let mut unread_dirs = Vec::new();
    let mut invalid_names = Vec::new();

    for entry in walker(config, scope, &rules) {
        match entry {
            Ok(entry) => match as_source(config, languages, scope, &entry) {
                Some(Ok(source)) => sources.push(source),
                Some(Err(finding)) => invalid_names.push(finding),
                None => {}
            },
            Err(error) => unread_dirs.extend(unreadable_dir(&error)),
        }
    }
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    unread_dirs.sort();
    let (unread_dirs, code_blocks_unread_dirs): (Vec<UnreadDir>, Vec<UnreadDir>) = unread_dirs
        .into_iter()
        .partition(|dir| config.excluded_by(&dir.path).is_none());

    let mut findings: Vec<Finding> = unread_dirs.iter().map(|dir| dir.finding(root)).collect();
    findings.extend(invalid_names);
    // Taken before the explicit paths are judged, which may read directories the walk never
    // entered: only an ignore file the walk met is worth a warning.
    findings.extend(
        rules
            .symlinked_files()
            .iter()
            .map(|file| symlinked_ignore_file(root, file)),
    );
    findings.extend(
        rules
            .unreadable_files()
            .iter()
            .map(|file| unreadable_ignore_file(root, file)),
    );
    findings.extend(explicit_path_warnings(
        config,
        languages,
        scope,
        &rules,
        &sources,
        &unread_dirs,
    ));
    findings.sort_by(|left, right| {
        left.code
            .cmp(right.code)
            .then_with(|| left.message.cmp(&right.message))
    });

    Walk {
        sources,
        findings,
        unread_dirs,
        code_blocks_unread_dirs,
    }
}

/// A walker over the root that applies every directory-level rule, in sorted order.
///
/// The walker's own ignore-file handling is off: it reads a `.gitignore` through a symlink and
/// can't be told to skip one, so `rules` decides instead, for every entry the walk meets.
fn walker(config: &Config, scope: &Scope, rules: &Arc<IgnoreRules>) -> ignore::Walk {
    let config_for_filter = config.clone();
    let scope_for_filter = scope.clone();
    let rules = Arc::clone(rules);
    WalkBuilder::new(config.root().as_path())
        .standard_filters(false)
        .follow_links(false)
        .sort_by_file_path(Path::cmp)
        .filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            !rules.ignores(entry.path(), is_dir)
                && descend_into(&config_for_filter, &scope_for_filter, entry)
        })
        .build()
}

/// Whether the walk keeps `entry`. Only directories are refused here; files are judged by
/// [`as_source`]. A directory that every feature's `exclude` names is never entered, so nothing
/// under it is read and no finding can come from inside it.
fn descend_into(config: &Config, scope: &Scope, entry: &DirEntry) -> bool {
    let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
    if !is_dir || entry.depth() == 0 {
        return true;
    }
    !is_vcs_dir(entry.path())
        && !is_nested_project(entry.path())
        && !is_excluded_dir(config, entry.path())
        && may_hold_sources(config.root(), scope, entry.path())
}

/// Whether the directory `dir` is excluded from both scopes: linking's `exclude` names it, and
/// code-blocks is off or its `exclude` names it too. A directory one scope still wants is walked.
fn is_excluded_dir(config: &Config, dir: &Path) -> bool {
    AbsolutePath::new(dir).is_some_and(|dir| {
        config.excluded_by(&dir).is_some()
            && config
                .code_blocks()
                .is_none_or(|code_blocks| code_blocks.excludes(&dir))
    })
}

/// Whether `dir` is named like a version-control directory.
pub(super) fn is_vcs_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| VCS_DIRS.contains(&name))
}

/// Whether `dir` has its own config, which makes it a separate project.
///
/// The same presence rule as the config search: an entry named `supdoc.config.toml` counts only
/// when a no-follow check shows a regular file or a symlink, a dangling one included. A
/// directory or a FIFO with that name does not make a nested project.
pub(super) fn is_nested_project(dir: &Path) -> bool {
    config_entry(dir).is_some()
}

/// Whether anything under `dir` can be inside `scope`, so narrowing the walk never
/// skips a listed path.
fn may_hold_sources(root: &AbsolutePath, scope: &Scope, dir: &Path) -> bool {
    let Scope::Paths(listed) = scope else {
        return true;
    };
    let Some(dir) = dir
        .strip_prefix(root.as_path())
        .ok()
        .and_then(|relative| SourcePath::from_relative(relative).ok())
    else {
        return true;
    };
    scope.contains(&dir)
        || listed.iter().any(|path| {
            path.as_str()
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// `entry` as a source, `None` when a rule skips it, or a `scan-invalid-source-path` warning when
/// its name can't be a source path, so the skipped file is never silent.
fn as_source(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    entry: &DirEntry,
) -> Option<Result<WalkedSource, Finding>> {
    // A symlink reports its own type, not its target's, so links are skipped here.
    if !entry.file_type()?.is_file() {
        return None;
    }
    let path = AbsolutePath::new(entry.path())?;
    let wants = Wants::of(config, &path);
    if !wants.links && !wants.code_blocks {
        return None;
    }
    let cartridge = languages.for_path(path.as_path())?;
    if !cartridge.reads_links() {
        return None;
    }
    let relative_path = path.strip_prefix(config.root())?;
    let relative = match SourcePath::from_relative(relative_path) {
        Ok(relative) => relative,
        Err(error) => return Some(Err(invalid_source_path(relative_path, &error))),
    };
    scope.contains(&relative).then_some(Ok(WalkedSource {
        source: relative,
        path,
        wants,
    }))
}

/// The warning for a file the walk would read but whose name isn't a valid source path.
fn invalid_source_path(relative: &Path, error: &LinkPathError) -> Finding {
    Finding::new(
        "scan-invalid-source-path",
        Severity::Warning,
        format!(
            "skipped {}: its name can't be written as a source path ({error})",
            relative.display()
        ),
    )
    .with_help(format!(
        "Links in that file weren't checked. Rename the file so its path is valid UTF-8 and \
         joins names with `/`, or add it to `exclude` in {CONFIG_FILE_NAME}"
    ))
}

/// The directory the walker couldn't read.
///
/// Other walker errors, such as a malformed ignore-file line, say nothing about what was skipped,
/// so they produce none.
fn unreadable_dir(error: &ignore::Error) -> Option<UnreadDir> {
    // The walk starts at the absolute root, so every path it reports is absolute.
    let path = AbsolutePath::new(path_of(error)?)?;
    let cause = UnreadCause::Io(error.io_error()?.kind());
    Some(UnreadDir { path, cause })
}

/// The path a walker error names, if it names one.
fn path_of(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            path_of(err)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![cfg_attr(
        test,
        allow(
            clippy::missing_errors_doc,
            clippy::missing_panics_doc,
            reason = "tests report failures rather than document callable contracts"
        )
    )]

    use std::process::Command;
    use std::sync::OnceLock;

    use tempfile::TempDir;

    use super::*;
    use crate::read::cartridge::{Cartridge, LanguageId, MarkdownRegion, SourceFile, SymbolTable};
    use crate::read::error::ReadError;
    use std::fs;
    use std::path::Path;

    /// A project on disk whose config file starts empty (scan everything).
    struct Project {
        dir: TempDir,
    }

    impl Project {
        fn new() -> Self {
            let project = Self {
                dir: TempDir::new().expect("a temporary project directory should be creatable"),
            };
            project.write(CONFIG_FILE_NAME, "");
            project
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be creatable");
            }
            fs::write(&path, text).expect("the fixture file should be writable");
        }

        fn walk(&self, scope: &Scope) -> Walk {
            let config_path = AbsolutePath::new(&self.dir.path().join(CONFIG_FILE_NAME))
                .expect("the temporary directory should be absolute");
            let config =
                Config::load_file(&config_path).expect("the fixture config should be valid");
            walk(
                &config,
                Languages::builtin().expect("the built-in cartridges should load"),
                scope,
                &GlobalExcludes::none(),
            )
        }

        /// The walked sources, relative to the root, written with `/` on every platform.
        fn sources(&self, scope: &Scope) -> Vec<String> {
            self.walk(scope)
                .sources
                .iter()
                .map(|walked| self.relative(&walked.path))
                .collect()
        }

        fn relative(&self, path: &AbsolutePath) -> String {
            path.strip_prefix(
                &AbsolutePath::new(self.dir.path())
                    .expect("the temporary directory should be absolute"),
            )
            .expect("a walked source should be under the root")
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
        }
    }

    fn paths(raw: &[&str]) -> Scope {
        Scope::Paths(
            raw.iter()
                .map(|path| {
                    path.parse()
                        .expect("the fixture scope path should be valid")
                })
                .collect(),
        )
    }

    fn codes(walk: &Walk) -> Vec<&'static str> {
        walk.findings.iter().map(|finding| finding.code).collect()
    }

    /// Makes `git_dir` a git directory git accepts as a repository, without running git: a `HEAD`
    /// naming a branch, and empty `objects` and `refs` directories.
    fn make_git_dir(git_dir: &Path) {
        for dir in ["objects", "refs"] {
            fs::create_dir_all(git_dir.join(dir))
                .expect("the fixture git directory should be creatable");
        }
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("the fixture git HEAD should be writable");
    }

    #[cfg(unix)]
    #[test]
    fn a_named_path_under_a_symlinked_directory_is_not_blamed_on_an_ignore_file() {
        let project = Project::new();
        project.write("real/a.rs", "");
        std::os::unix::fs::symlink("real", project.dir.path().join("alias"))
            .expect("the fixture symlink should be creatable");

        let walk = project.walk(&paths(&["alias/a.rs"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-symlinked"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert_eq!(
            finding.help.as_deref(),
            Some("Pass the real path `real/a.rs` instead")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_named_symlinked_directory_warns_with_its_real_path() {
        let project = Project::new();
        project.write("real/a.md", "");
        std::os::unix::fs::symlink("real", project.dir.path().join("alias"))
            .expect("the fixture symlink should be creatable");

        let walk = project.walk(&paths(&["alias"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-symlinked"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert_eq!(
            finding.message,
            "alias was named on the command line but is a symlink, which supdoc doesn't follow"
        );
        assert_eq!(
            finding.help.as_deref(),
            Some("Pass the real path `real` instead")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_named_directory_under_a_symlinked_directory_warns_with_its_real_path() {
        let project = Project::new();
        project.write("real/sub/a.md", "");
        std::os::unix::fs::symlink("real", project.dir.path().join("alias"))
            .expect("the fixture symlink should be creatable");

        let walk = project.walk(&paths(&["alias/sub"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-symlinked"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert_eq!(
            finding.help.as_deref(),
            Some("Pass the real path `real/sub` instead")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_named_symlinked_file_warns_with_its_real_path() {
        let project = Project::new();
        project.write("real/a.md", "");
        std::os::unix::fs::symlink("real/a.md", project.dir.path().join("link.md"))
            .expect("the fixture symlink should be creatable");

        let walk = project.walk(&paths(&["link.md"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-symlinked"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert_eq!(
            finding.help.as_deref(),
            Some("Pass the real path `real/a.md` instead")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_named_symlink_out_of_the_root_suggests_no_real_path() {
        let outside = outside_dir();
        let project = Project::new();
        symlink_in(&project, "build", &outside.path().join("build"));

        let walk = project.walk(&paths(&["build"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-symlinked"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert_eq!(
            finding.help.as_deref(),
            Some("Pass the real path inside the project instead")
        );
    }

    #[test]
    fn gitignored_source_is_not_scanned() {
        let project = Project::new();
        project.write(".gitignore", "generated.rs\n");
        project.write("generated.rs", "");
        project.write("kept.rs", "");

        assert_eq!(project.sources(&Scope::All), ["kept.rs"]);
    }

    #[test]
    fn gitignore_is_honored_outside_a_git_repo() {
        let project = Project::new();
        project.write(".gitignore", "build/\n");
        project.write("build/out.rs", "");
        project.write("src/lib.rs", "");

        assert!(!project.dir.path().join(".git").exists());
        assert_eq!(project.sources(&Scope::All), ["src/lib.rs"]);
    }

    #[test]
    fn nested_gitignore_applies_to_its_subtree() {
        let project = Project::new();
        project.write("docs/.gitignore", "draft.rs\n");
        project.write("docs/draft.rs", "");
        project.write("docs/final.rs", "");
        project.write("draft.rs", "");

        assert_eq!(project.sources(&Scope::All), ["docs/final.rs", "draft.rs"]);
    }

    #[test]
    fn dot_ignore_file_is_honored() {
        let project = Project::new();
        project.write(".ignore", "scratch.rs\n");
        project.write("scratch.rs", "");
        project.write("real.rs", "");

        assert_eq!(project.sources(&Scope::All), ["real.rs"]);
    }

    #[test]
    fn git_info_exclude_is_honored_in_a_real_repo() {
        let project = Project::new();
        let init = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(project.dir.path())
            .status()
            .expect("git should be runnable to build the fixture repository");
        assert!(init.success(), "git init failed in the fixture directory");
        project.write(".git/info/exclude", "private.rs\n");
        project.write("private.rs", "");
        project.write("public.rs", "");

        assert_eq!(project.sources(&Scope::All), ["public.rs"]);
    }

    #[test]
    fn hidden_directory_is_scanned_but_dot_git_is_not() {
        let project = Project::new();
        project.write(".github/hooks.rs", "");
        project.write(".git/objects/stray.rs", "");
        project.write(".hg/stray.rs", "");
        project.write(".svn/stray.rs", "");
        project.write(".jj/stray.rs", "");

        assert_eq!(project.sources(&Scope::All), [".github/hooks.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_is_not_followed() {
        let project = Project::new();
        project.write("real/a.rs", "");
        std::os::unix::fs::symlink(
            project.dir.path().join("real"),
            project.dir.path().join("alias"),
        )
        .expect("the fixture symlink should be creatable");

        assert_eq!(project.sources(&Scope::All), ["real/a.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_file_is_not_a_source() {
        let project = Project::new();
        project.write("real.rs", "");
        std::os::unix::fs::symlink(
            project.dir.path().join("real.rs"),
            project.dir.path().join("alias.rs"),
        )
        .expect("the fixture symlink should be creatable");

        assert_eq!(project.sources(&Scope::All), ["real.rs"]);
    }

    #[test]
    fn a_directory_named_like_the_config_does_not_make_a_nested_project() {
        let project = Project::new();
        project.write("inner/supdoc.config.toml/placeholder", "");
        project.write("inner/a.rs", "");

        assert_eq!(project.sources(&Scope::All), ["inner/a.rs"]);
    }

    #[test]
    fn nested_project_directory_is_skipped_with_everything_under_it() {
        let project = Project::new();
        project.write("inner/supdoc.config.toml", "");
        project.write("inner/a.rs", "");
        project.write("inner/deeper/b.rs", "");
        project.write("outer.rs", "");

        assert_eq!(project.sources(&Scope::All), ["outer.rs"]);
    }

    #[test]
    fn excluded_source_is_not_scanned() {
        let project = Project::new();
        project.write(CONFIG_FILE_NAME, "exclude = [\"vendor\", \"skip.rs\"]\n");
        project.write("vendor/deep/a.rs", "");
        project.write("skip.rs", "");
        project.write("keep.rs", "");

        assert_eq!(project.sources(&Scope::All), ["keep.rs"]);
    }

    #[test]
    fn include_limits_sources() {
        let project = Project::new();
        project.write(CONFIG_FILE_NAME, "include = [\"docs/**/*.rs\"]\n");
        project.write("docs/guide/a.rs", "");
        project.write("docs/b.rs", "");
        project.write("src/c.rs", "");

        assert_eq!(
            project.sources(&Scope::All),
            ["docs/b.rs", "docs/guide/a.rs"]
        );
    }

    #[test]
    fn file_without_cartridge_is_not_a_source() {
        let project = Project::new();
        project.write("logo.png", "");
        project.write("notes.txt", "");
        project.write("README", "");
        project.write("lib.rs", "");

        assert_eq!(project.sources(&Scope::All), ["lib.rs"]);
    }

    #[test]
    fn sources_come_back_sorted_and_identical_across_runs() {
        let project = Project::new();
        for name in ["z.rs", "a/b.rs", "a.rs", "m/n/o.rs", "B.rs"] {
            project.write(name, "");
        }

        let first = project.sources(&Scope::All);
        assert_eq!(first, ["B.rs", "a/b.rs", "a.rs", "m/n/o.rs", "z.rs"]);
        assert_eq!(project.sources(&Scope::All), first);
    }

    #[test]
    fn explicit_path_narrows_scan() {
        let project = Project::new();
        project.write("docs/a.rs", "");
        project.write("docs/guide/b.rs", "");
        project.write("docs2/c.rs", "");
        project.write("src/d.rs", "");

        assert_eq!(
            project.sources(&paths(&["docs", "src/d.rs"])),
            ["docs/a.rs", "docs/guide/b.rs", "src/d.rs"],
        );
    }

    #[test]
    fn explicit_path_that_is_walked_gives_no_warning() {
        let project = Project::new();
        project.write("docs/a.rs", "");

        let walk = project.walk(&paths(&["docs", "docs/a.rs"]));
        assert!(
            walk.findings.is_empty(),
            "unexpected findings: {:?}",
            walk.findings
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_whose_name_is_not_a_source_path_is_a_finding_not_a_silent_skip() {
        let project = Project::new();
        project.write(CONFIG_FILE_NAME, "");
        project.write("docs/a\\b.md", "");

        let walk = project.walk(&Scope::All);

        assert!(walk.sources.is_empty());
        assert_eq!(codes(&walk), ["scan-invalid-source-path"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(
            finding.message.contains("uses `\\`"),
            "got: {}",
            finding.message
        );
        assert!(
            finding.message.contains("docs/a\\b.md"),
            "message was: {}",
            finding.message
        );
    }

    #[test]
    fn explicit_excluded_path_warns_with_the_rule() {
        let project = Project::new();
        project.write(CONFIG_FILE_NAME, "exclude = [\"docs/old.rs\"]\n");
        project.write("docs/old.rs", "");

        let walk = project.walk(&paths(&["docs/old.rs"]));

        assert!(walk.sources.is_empty());
        assert_eq!(codes(&walk), ["scan-explicit-path-excluded"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert_eq!(finding.severity, Severity::Warning);
        assert!(
            finding
                .message
                .starts_with("docs/old.rs was named on the command line"),
            "message was: {}",
            finding.message
        );
        assert!(
            finding.message.contains(CONFIG_FILE_NAME),
            "message was: {}",
            finding.message
        );
    }

    #[test]
    fn explicit_excluded_path_names_the_exact_pattern() {
        let project = Project::new();
        project.write(
            CONFIG_FILE_NAME,
            "exclude = [\"other\", \"docs/*.rs\", \"docs\"]\n",
        );
        project.write("docs/old.rs", "");

        let walk = project.walk(&paths(&["docs/old.rs"]));

        let message = &walk
            .findings
            .first()
            .expect("one warning was expected")
            .message;
        assert!(
            message.contains("excluded by `docs/*.rs` in `exclude`"),
            "message was: {message}"
        );
    }

    #[test]
    fn explicit_path_missing_from_include_blames_include() {
        let project = Project::new();
        project.write(CONFIG_FILE_NAME, "include = [\"src\"]\n");
        project.write("docs/a.rs", "");

        let walk = project.walk(&paths(&["docs/a.rs"]));

        let message = &walk
            .findings
            .first()
            .expect("one warning was expected")
            .message;
        assert!(
            message.contains("not matched by `include`"),
            "message was: {message}"
        );
    }

    #[test]
    fn explicit_path_code_blocks_reads_draws_no_warning_though_linking_excludes_it() {
        let project = Project::new();
        project.write(
            CONFIG_FILE_NAME,
            "exclude = [\"skills\"]\n[features.code-blocks]\nexclude = []\n",
        );
        project.write("skills/a.md", "");

        let walk = project.walk(&paths(&["skills/a.md"]));

        assert!(walk.findings.is_empty(), "{:?}", walk.findings);
        assert_eq!(walk.sources.len(), 1, "code-blocks should still read it");
    }

    #[test]
    fn explicit_path_both_scopes_leave_out_names_both() {
        let project = Project::new();
        project.write(
            CONFIG_FILE_NAME,
            "exclude = [\"skills\"]\n[features.code-blocks]\nexclude = [\"skills\"]\n",
        );
        project.write("skills/a.md", "");

        let walk = project.walk(&paths(&["skills/a.md"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-excluded"]);
        let finding = walk.findings.first().expect("one warning was expected");
        assert!(
            finding.message.contains(
                "excluded by `skills` in `exclude` in supdoc.config.toml, and by \
                 `[features.code-blocks] exclude`"
            ),
            "message was: {}",
            finding.message
        );
        let help = finding.help.as_deref().unwrap_or_default();
        assert!(
            help.contains("change `[features.code-blocks] exclude` to check its code blocks"),
            "help was: {help}"
        );
    }

    #[test]
    fn explicit_excluded_directory_names_the_exact_pattern() {
        let project = Project::new();
        project.write(CONFIG_FILE_NAME, "exclude = [\"vendor\"]\n");
        project.write("vendor/a.rs", "");

        let walk = project.walk(&paths(&["vendor"]));

        assert!(walk.sources.is_empty());
        assert_eq!(codes(&walk), ["scan-explicit-path-excluded"]);
        let message = &walk
            .findings
            .first()
            .expect("one warning was expected")
            .message;
        assert!(
            message.contains("excluded by `vendor` in `exclude`"),
            "message was: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn excluded_directory_is_never_descended() {
        use std::os::unix::fs::PermissionsExt;

        let project = Project::new();
        project.write(CONFIG_FILE_NAME, "exclude = [\"vendor\"]\n");
        project.write("vendor/locked/a.rs", "");
        project.write("open/b.rs", "");
        let locked = project.dir.path().join("vendor/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("the fixture directory permissions should be settable");

        let walk = project.walk(&Scope::All);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
            .expect("the fixture directory permissions should be restorable");

        assert_eq!(
            walk.sources
                .iter()
                .map(|walked| project.relative(&walked.path))
                .collect::<Vec<_>>(),
            ["open/b.rs"],
        );
        assert!(
            walk.findings.is_empty(),
            "unexpected findings: {:?}",
            walk.findings
        );
    }

    #[test]
    fn explicit_gitignored_path_warns_with_the_ignore_rule() {
        let project = Project::new();
        project.write(".gitignore", "# generated\nout.rs\n");
        project.write("build/out.rs", "");
        project.write("build/.gitignore", "out.rs\n");

        let walk = project.walk(&paths(&["build/out.rs"]));

        assert!(walk.sources.is_empty());
        assert_eq!(codes(&walk), ["scan-explicit-path-ignored"]);
        let message = &walk
            .findings
            .first()
            .expect("one warning was expected")
            .message;
        assert!(
            message.contains("`out.rs` in build/.gitignore"),
            "message was: {message}"
        );
    }

    #[test]
    fn explicit_directory_inside_an_ignored_directory_warns() {
        let project = Project::new();
        project.write(".gitignore", "build/\n");
        project.write("build/sub/out.rs", "");

        let walk = project.walk(&paths(&["build/sub"]));

        assert!(walk.sources.is_empty());
        assert_eq!(codes(&walk), ["scan-explicit-path-ignored"]);
    }

    #[test]
    fn whitelisted_explicit_path_gives_no_warning() {
        let project = Project::new();
        project.write(".gitignore", "*.rs\n!keep.rs\n");
        project.write("keep.rs", "");

        let walk = project.walk(&paths(&["keep.rs"]));

        assert_eq!(project.sources(&paths(&["keep.rs"])), ["keep.rs"]);
        assert!(
            walk.findings.is_empty(),
            "unexpected findings: {:?}",
            walk.findings
        );
    }

    /// An ignore-file line that matches `out.rs`, distinct from any path the tests name.
    const RULE: &str = "ou*.rs";

    /// A directory outside the project root holding `ignore` (a file of [`RULE`]) and
    /// `build/.gitignore` (the same) beside `build/out.rs`.
    #[cfg(unix)]
    fn outside_dir() -> TempDir {
        let outside = TempDir::new().expect("a temporary outside directory should be creatable");
        fs::create_dir(outside.path().join("build"))
            .expect("the outside fixture directory should be creatable");
        for file in ["ignore", "build/.gitignore"] {
            fs::write(outside.path().join(file), format!("{RULE}\n"))
                .expect("the outside ignore file should be writable");
        }
        fs::write(outside.path().join("build/out.rs"), "")
            .expect("the outside source should be writable");
        outside
    }

    /// Symlinks `link`, relative to the project root, to `target`.
    #[cfg(unix)]
    fn symlink_in(project: &Project, link: &str, target: &Path) {
        std::os::unix::fs::symlink(target, project.dir.path().join(link))
            .expect("the fixture symlink should be creatable");
    }

    /// Every finding message of `walk`, one per line.
    fn messages(walk: &Walk) -> String {
        walk.findings
            .iter()
            .map(|finding| finding.message.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(unix)]
    #[test]
    fn explicit_path_warning_never_quotes_a_gitignore_symlinked_out_of_the_root() {
        let outside = outside_dir();
        let project = Project::new();
        project.write("out.rs", "");
        symlink_in(&project, ".gitignore", &outside.path().join("ignore"));

        let walk = project.walk(&paths(&["out.rs"]));

        assert!(
            !messages(&walk).contains(RULE),
            "a line from outside the root was printed:\n{}",
            messages(&walk)
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_path_warning_never_quotes_an_ignore_file_behind_a_directory_symlinked_out() {
        let outside = outside_dir();
        let project = Project::new();
        symlink_in(&project, "build", &outside.path().join("build"));

        let walk = project.walk(&paths(&["build/out.rs"]));

        assert!(
            !messages(&walk).contains(RULE),
            "a line from outside the root was printed:\n{}",
            messages(&walk)
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_path_warning_never_quotes_a_git_exclude_symlinked_out_of_the_root() {
        let outside = outside_dir();
        let project = Project::new();
        project.write("out.rs", "");
        make_git_dir(&project.dir.path().join(".git"));
        fs::create_dir(project.dir.path().join(".git/info"))
            .expect("the fixture git directory should be creatable");
        symlink_in(
            &project,
            ".git/info/exclude",
            &outside.path().join("ignore"),
        );

        let walk = project.walk(&paths(&["out.rs"]));

        assert!(walk.unread_dirs.is_empty(), "{:?}", walk.unread_dirs);
        assert_eq!(
            codes(&walk),
            ["scan-explicit-path-ignored"],
            "the exclude file applies as in git"
        );
        assert!(
            !messages(&walk).contains(RULE),
            "a line from outside the root was printed:\n{}",
            messages(&walk)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_root_gitignore_symlinked_inside_the_root_is_not_read_and_warns_once() {
        let project = Project::new();
        project.write("out.rs", "");
        project.write("rules/ignore", &format!("{RULE}\n"));
        symlink_in(
            &project,
            ".gitignore",
            &project.dir.path().join("rules/ignore"),
        );

        let walk = project.walk(&paths(&["out.rs"]));

        assert_eq!(
            project.sources(&paths(&["out.rs"])),
            ["out.rs"],
            "git doesn't read a symlinked `.gitignore`, so its rule hides nothing"
        );
        assert_eq!(codes(&walk), ["scan-ignore-file-symlink"]);
        assert_eq!(
            messages(&walk),
            ".gitignore is a symlink, so its rules don't apply"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_whose_gitignore_is_symlinked_out_is_walked_without_its_rules() {
        let outside = outside_dir();
        let project = Project::new();
        project.write("docs/a.rs", "");
        project.write("docs/out.rs", "");
        project.write("src/out.rs", "");
        symlink_in(&project, "docs/.gitignore", &outside.path().join("ignore"));

        let walk = project.walk(&Scope::All);

        assert_eq!(
            project.sources(&Scope::All),
            ["docs/a.rs", "docs/out.rs", "src/out.rs"],
            "the directory is walked, and the symlink's rule hides nothing"
        );
        assert!(walk.unread_dirs.is_empty(), "{:?}", walk.unread_dirs);
        assert_eq!(codes(&walk), ["scan-ignore-file-symlink"]);
        assert_eq!(
            messages(&walk),
            "docs/.gitignore is a symlink, so its rules don't apply"
        );
        let help = walk
            .findings
            .first()
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default();
        assert!(
            help.contains("Replace the symlink with a regular file") && !help.contains("rm"),
            "the help should keep the rules, not delete them: {help}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn named_path_beside_a_symlinked_gitignore_gets_only_the_symlink_warning() {
        let outside = outside_dir();
        let project = Project::new();
        project.write("docs/out.rs", "");
        symlink_in(&project, "docs/.gitignore", &outside.path().join("ignore"));

        let walk = project.walk(&paths(&["docs/out.rs"]));

        assert_eq!(project.sources(&paths(&["docs/out.rs"])), ["docs/out.rs"]);
        assert_eq!(
            codes(&walk),
            ["scan-ignore-file-symlink"],
            "no ignore rule skipped it:\n{}",
            messages(&walk)
        );
        assert!(!messages(&walk).contains(RULE), "{}", messages(&walk));
    }

    /// A git directory outside the root, one git accepts, whose `info/exclude` holds [`RULE`].
    #[cfg(unix)]
    fn outside_exclude() -> TempDir {
        let outside = TempDir::new().expect("a temporary outside directory should be creatable");
        make_git_dir(&outside.path().join("git"));
        fs::create_dir(outside.path().join("git/info"))
            .expect("the outside git directory should be creatable");
        fs::write(outside.path().join("git/info/exclude"), format!("{RULE}\n"))
            .expect("the outside exclude file should be writable");
        outside
    }

    #[cfg(unix)]
    #[test]
    fn git_dir_symlinked_out_of_the_root_brings_its_exclude_file_unquoted() {
        let outside = outside_exclude();
        let project = Project::new();
        project.write("out.rs", "");
        project.write("vendor/out.rs", "");
        project.write("vendor/kept.rs", "");
        symlink_in(&project, ".git", &outside.path().join("git"));
        symlink_in(&project, "vendor/.git", &outside.path().join("git"));

        let walk = project.walk(&paths(&["out.rs", "vendor/out.rs", "vendor/kept.rs"]));

        assert_eq!(
            project.sources(&Scope::All),
            ["vendor/kept.rs"],
            "git follows a symlinked `.git` to its exclude file, and so does the walk"
        );
        assert!(walk.unread_dirs.is_empty(), "{:?}", walk.unread_dirs);
        assert_eq!(
            codes(&walk),
            ["scan-explicit-path-ignored", "scan-explicit-path-ignored"]
        );
        assert!(
            !messages(&walk).contains(RULE),
            "a line from outside the root was printed:\n{}",
            messages(&walk)
        );
    }

    /// A worktree's git directory outside the root, whose shared directory (`commondir`, here
    /// itself) holds an `info/exclude` of [`RULE`].
    fn outside_git_dir() -> TempDir {
        let outside = TempDir::new().expect("a temporary outside directory should be creatable");
        fs::create_dir_all(outside.path().join("info"))
            .expect("the outside git directory should be creatable");
        fs::write(outside.path().join("commondir"), ".\n")
            .expect("the outside commondir file should be writable");
        fs::write(outside.path().join("info/exclude"), format!("{RULE}\n"))
            .expect("the outside exclude file should be writable");
        outside
    }

    #[test]
    fn git_file_pointing_out_of_the_root_brings_its_exclude_file_unquoted() {
        let outside = outside_git_dir();
        let project = Project::new();
        project.write(".git", &format!("gitdir: {}\n", outside.path().display()));
        project.write(
            "vendor/.git",
            &format!("gitdir: {}\n", outside.path().display()),
        );
        project.write("out.rs", "");
        project.write("vendor/out.rs", "");
        project.write("vendor/kept.rs", "");

        let walk = project.walk(&paths(&["out.rs", "vendor/out.rs", "vendor/kept.rs"]));

        assert_eq!(
            project.sources(&Scope::All),
            ["vendor/kept.rs"],
            "git follows a `.git` file to its exclude file, and so does the walk"
        );
        assert!(walk.unread_dirs.is_empty(), "{:?}", walk.unread_dirs);
        assert_eq!(
            codes(&walk),
            ["scan-explicit-path-ignored", "scan-explicit-path-ignored"]
        );
        assert!(
            !messages(&walk).contains(RULE),
            "a line from outside the root was printed:\n{}",
            messages(&walk)
        );
    }

    /// Asserts that a `.git` file naming `gitdir` brings the exclude file of `.hidden-git`, which
    /// hides `out.rs`, without quoting its line.
    fn assert_gitfile_exclude_applies_unquoted(gitdir: &str) {
        let project = Project::new();
        project.write(".git", &format!("gitdir: {gitdir}\n"));
        project.write(".hidden-git/info/exclude", &format!("{RULE}\n"));
        project.write("out.rs", "");
        project.write("kept.rs", "");

        let walk = project.walk(&paths(&["out.rs", "kept.rs"]));

        assert_eq!(
            project.sources(&Scope::All),
            ["kept.rs"],
            "git reads the exclude file of the git directory the `.git` file names"
        );
        assert_eq!(codes(&walk), ["scan-explicit-path-ignored"]);
        assert!(
            !messages(&walk).contains(RULE),
            "an exclude line reached through a `.git` file was printed:\n{}",
            messages(&walk)
        );
    }

    #[test]
    fn a_submodule_git_file_without_commondir_brings_its_exclude_file_unquoted() {
        assert_gitfile_exclude_applies_unquoted(".hidden-git");
    }

    #[test]
    fn a_git_file_with_a_roundabout_relative_gitdir_is_counted_from_its_own_directory() {
        assert_gitfile_exclude_applies_unquoted(".hidden-git/../.hidden-git");
    }

    #[test]
    fn a_worktree_git_file_with_a_relative_gitdir_follows_commondir() {
        let project = Project::new();
        project.write(".git", "gitdir: .hidden-git/worktrees/w\n");
        project.write(".hidden-git/worktrees/w/commondir", "../..\n");
        project.write(".hidden-git/info/exclude", &format!("{RULE}\n"));
        project.write("out.rs", "");
        project.write("kept.rs", "");

        let walk = project.walk(&paths(&["out.rs", "kept.rs"]));

        assert_eq!(
            project.sources(&Scope::All),
            ["kept.rs"],
            "the worktree's `commondir`, counted from its git directory, holds the exclude file"
        );
        assert!(
            !messages(&walk).contains(RULE),
            "an exclude line reached through a `.git` file was printed:\n{}",
            messages(&walk)
        );
    }

    #[test]
    fn a_git_file_whose_commondir_is_empty_brings_no_exclude_file() {
        let project = Project::new();
        project.write(".git", "gitdir: .hidden-git\n");
        project.write(".hidden-git/commondir", "");
        project.write(".hidden-git/info/exclude", &format!("{RULE}\n"));
        project.write("out.rs", "");

        assert_eq!(project.sources(&Scope::All), ["out.rs"]);
    }

    #[test]
    fn a_git_file_whose_commondir_holds_a_nul_warns_naming_the_commondir_file() {
        let project = Project::new();
        project.write(".git", "gitdir: .hidden-git/worktrees/w\n");
        project.write(".hidden-git/worktrees/w/commondir", "../\0..\n");
        project.write(".hidden-git/info/exclude", &format!("{RULE}\n"));
        project.write("out.rs", "");

        let walk = project.walk(&Scope::All);

        assert_eq!(
            project.sources(&Scope::All),
            ["out.rs"],
            "a `commondir` line that names no path brings no exclude file"
        );
        assert_eq!(codes(&walk), ["scan-ignore-file-unreadable"]);
        assert_eq!(
            messages(&walk),
            "the git exclude file `.hidden-git/worktrees/w/commondir` could not be fully read, so \
             some of its rules may not apply"
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_whose_ignore_file_is_symlinked_out_is_walked_with_one_warning() {
        let outside = outside_dir();
        let project = Project::new();
        project.write("docs/out.rs", "");
        symlink_in(&project, ".ignore", &outside.path().join("ignore"));

        let walk = project.walk(&Scope::All);

        assert_eq!(project.sources(&Scope::All), ["docs/out.rs"]);
        assert!(walk.unread_dirs.is_empty(), "{:?}", walk.unread_dirs);
        assert_eq!(codes(&walk), ["scan-ignore-file-symlink"]);
        assert_eq!(
            messages(&walk),
            ".ignore is a symlink, so its rules don't apply"
        );
    }

    /// Builds a git work tree whose top and middle directories each hold a `.gitignore` hiding
    /// `top.rs` and `mid.rs`, with the project `mid/project` beneath them holding `k.rs`, `mid.rs`
    /// and `top.rs`. With `linked`, both `.gitignore` files are symlinks to files elsewhere.
    /// Returns the temporary directory, the outside directory and the project's config.
    #[cfg(unix)]
    fn stacked_gitignores(linked: bool) -> (TempDir, TempDir, Config) {
        let outside = outside_dir();
        let top = TempDir::new().expect("a temporary work tree should be creatable");
        let project = top.path().join("mid/project");
        fs::create_dir_all(&project).expect("the project directory should be creatable");
        for file in ["k.rs", "mid.rs", "top.rs", CONFIG_FILE_NAME] {
            fs::write(project.join(file), "").expect("the fixture file should be writable");
        }
        make_git_dir(&top.path().join(".git"));
        for (dir, rule) in [
            (top.path().to_path_buf(), "top.rs"),
            (top.path().join("mid"), "mid.rs"),
        ] {
            let gitignore = dir.join(".gitignore");
            if linked {
                let target = outside.path().join(format!("{rule}.gitignore"));
                fs::write(&target, format!("{rule}\n")).expect("the rules should be writable");
                std::os::unix::fs::symlink(target, gitignore)
                    .expect("the fixture symlink should be creatable");
            } else {
                fs::write(gitignore, format!("{rule}\n")).expect("the rules should be writable");
            }
        }
        let config_path = AbsolutePath::new(&project.join(CONFIG_FILE_NAME))
            .expect("the temporary directory should be absolute");
        let config = Config::load_file(&config_path).expect("the fixture config should be valid");
        (top, outside, config)
    }

    /// The names of the files `config`'s project walks, sorted, and its findings' codes.
    #[cfg(unix)]
    fn walked_names(config: &Config) -> (Vec<String>, Vec<&'static str>) {
        let walked = walk(
            config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes::none(),
        );
        let names = walked
            .sources
            .iter()
            .filter_map(|walked| walked.path.as_path().file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        (names, codes(&walked))
    }

    #[cfg(unix)]
    #[test]
    fn gitignores_above_the_root_symlinked_inside_the_work_tree_are_not_read() {
        let (_top, _outside, config) = stacked_gitignores(true);

        let (names, findings) = walked_names(&config);

        assert_eq!(
            names,
            ["k.rs", "mid.rs", "top.rs"],
            "git treats a symlinked `.gitignore` in its work tree as absent, wherever it points"
        );
        assert!(
            findings.is_empty(),
            "no warning is given above the root: {findings:?}"
        );
    }

    /// A git work tree `top` holding `sub/proj` with `a.rs`, `b.rs` and `c.rs`, and `alias`, a
    /// symlink to `top` in another directory, so the project root is reached through a symlink.
    /// Returns the directories (kept alive) and the project's config under `alias`.
    #[cfg(unix)]
    fn project_through_symlink(gitignore: &str) -> (TempDir, TempDir, Config) {
        let top = TempDir::new().expect("a temporary work tree should be creatable");
        let holder = TempDir::new().expect("a temporary link directory should be creatable");
        let project = top.path().join("sub/proj");
        fs::create_dir_all(&project).expect("the project directory should be creatable");
        for file in ["a.rs", "b.rs", "c.rs", CONFIG_FILE_NAME] {
            fs::write(project.join(file), "").expect("the fixture file should be writable");
        }
        make_git_dir(&top.path().join(".git"));
        fs::write(top.path().join(".gitignore"), gitignore)
            .expect("the gitignore should be writable");
        let alias = holder.path().join("alias");
        std::os::unix::fs::symlink(top.path(), &alias)
            .expect("the fixture symlink should be creatable");
        let config_path = AbsolutePath::new(&alias.join("sub/proj").join(CONFIG_FILE_NAME))
            .expect("the temporary directory should be absolute");
        let config = Config::load_file(&config_path).expect("the fixture config should be valid");
        (top, holder, config)
    }

    #[cfg(unix)]
    #[test]
    fn anchored_rules_above_a_root_reached_through_a_symlink_hide_their_files() {
        let (_top, holder, config) = project_through_symlink("/sub/proj/a.rs\n");
        let global = holder.path().join("global-excludes");
        fs::write(&global, "/sub/proj/b.rs\n").expect("the global excludes should be writable");

        let walked = walk(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes { file: Some(global) },
        );

        let names: Vec<_> = walked
            .sources
            .iter()
            .filter_map(|walked| walked.path.as_path().file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["c.rs"],
            "the `.gitignore` above the root hides a.rs and the global file hides b.rs, though the \
             root is reached through a symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_gitignores_above_the_root_still_hide_their_files() {
        let (_top, _outside, config) = stacked_gitignores(false);

        let (names, findings) = walked_names(&config);

        assert_eq!(names, ["k.rs"], "the control: regular files apply");
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[cfg(unix)]
    #[test]
    fn ignore_file_above_the_root_symlinked_is_not_read() {
        let outside = outside_dir();
        let above = TempDir::new().expect("a temporary parent directory should be creatable");
        for file in ["project/out.rs", "project/kept.rs"] {
            let path = above.path().join(file);
            fs::create_dir_all(path.parent().expect("the fixture file has a parent"))
                .expect("the project directory should be creatable");
            fs::write(path, "").expect("the fixture source should be writable");
        }
        fs::write(above.path().join("project").join(CONFIG_FILE_NAME), "")
            .expect("the fixture config should be writable");
        make_git_dir(&above.path().join(".git"));
        std::os::unix::fs::symlink(outside.path().join("ignore"), above.path().join(".ignore"))
            .expect("the fixture symlink should be creatable");
        let config_path = AbsolutePath::new(&above.path().join("project").join(CONFIG_FILE_NAME))
            .expect("the temporary directory should be absolute");
        let config = Config::load_file(&config_path).expect("the fixture config should be valid");

        let all = walk(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes::none(),
        );
        let one = walk(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &paths(&["out.rs"]),
            &GlobalExcludes::none(),
        );

        let walked: Vec<_> = all
            .sources
            .iter()
            .filter_map(|walked| walked.path.as_path().file_name())
            .collect();
        assert_eq!(
            walked,
            ["kept.rs", "out.rs"],
            "the symlinked rules are not read"
        );
        assert!(all.unread_dirs.is_empty(), "{:?}", all.unread_dirs);
        assert!(
            all.findings.is_empty(),
            "a symlink above the root is passed over without a warning: {:?}",
            all.findings
        );
        assert!(one.findings.is_empty(), "{:?}", one.findings);
    }

    #[cfg(unix)]
    #[test]
    fn gitignore_symlinked_inside_the_root_does_not_apply() {
        let project = Project::new();
        project.write("docs/out.rs", "");
        project.write("docs/keep.rs", "");
        project.write("rules/ignore", &format!("{RULE}\n"));
        symlink_in(
            &project,
            "docs/.gitignore",
            &project.dir.path().join("rules/ignore"),
        );

        let walk = project.walk(&Scope::All);

        assert_eq!(
            project.sources(&Scope::All),
            ["docs/keep.rs", "docs/out.rs"],
            "git treats a symlinked `.gitignore` as absent, wherever it points"
        );
        assert!(walk.unread_dirs.is_empty(), "{:?}", walk.unread_dirs);
        assert_eq!(codes(&walk), ["scan-ignore-file-symlink"]);
    }

    /// Writes `.git/info/exclude` in `project` as a real directory and file, holding `rules`, in a
    /// git directory git accepts.
    fn write_exclude(project: &Project, rules: &str) {
        make_git_dir(&project.dir.path().join(".git"));
        project.write(".git/info/exclude", rules);
    }

    #[test]
    fn a_gitignore_whitelist_overrides_the_git_exclude_file_as_in_git() {
        let project = Project::new();
        write_exclude(&project, "*.rs\n");
        project.write(".gitignore", "!keep.rs\n");
        project.write("keep.rs", "");
        project.write("drop.rs", "");

        assert_eq!(project.sources(&Scope::All), ["keep.rs"]);
    }

    #[test]
    fn a_deeper_gitignore_whitelist_overrides_a_shallower_rule() {
        let project = Project::new();
        project.write(".gitignore", "*.rs\n");
        project.write("docs/.gitignore", "!keep.rs\n");
        project.write("docs/keep.rs", "");
        project.write("docs/drop.rs", "");

        assert_eq!(project.sources(&Scope::All), ["docs/keep.rs"]);
    }

    #[test]
    fn a_dot_ignore_rule_outranks_a_gitignore_at_any_depth() {
        let project = Project::new();
        project.write(".ignore", "!docs/keep.rs\n");
        project.write("docs/.gitignore", "keep.rs\n");
        project.write("docs/keep.rs", "");

        assert_eq!(
            project.sources(&Scope::All),
            ["docs/keep.rs"],
            "as in the walker's defaults, `.ignore` is consulted before any `.gitignore`"
        );
    }

    #[test]
    fn explicit_path_warning_quotes_the_rule_that_decided() {
        let project = Project::new();
        write_exclude(&project, "out.rs\n");
        project.write(".gitignore", "!out.rs\n");
        project.write("docs/.gitignore", "o*.rs\n");
        project.write("docs/out.rs", "");

        let walk = project.walk(&paths(&["docs/out.rs"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-ignored"]);
        assert!(
            messages(&walk).ends_with("is ignored by `o*.rs` in docs/.gitignore"),
            "the deepest `.gitignore` decided:\n{}",
            messages(&walk)
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_gitignore_in_the_root_warns_naming_it_with_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let project = Project::new();
        project.write(".gitignore", "gen/\n");
        project.write("gen/out.rs", "");
        let file = project.dir.path().join(".gitignore");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000))
            .expect("the fixture ignore file permissions should be settable");
        if fs::read(&file).is_ok() {
            return; // Mode 000 doesn't keep this user (root) out.
        }

        let walk = project.walk(&Scope::All);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644))
            .expect("the fixture ignore file permissions should be restorable");

        assert_eq!(codes(&walk), ["scan-ignore-file-unreadable"]);
        let finding = walk
            .findings
            .first()
            .expect("the unreadable ignore file should be reported");
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(
            finding.message,
            "`.gitignore` can't be read, so its rules don't apply"
        );
        let help = finding.help.as_deref().unwrap_or_default();
        assert!(help.contains("`chmod u+r .gitignore`"), "{help}");
        assert_eq!(
            walk.sources
                .iter()
                .map(|walked| project.relative(&walked.path))
                .collect::<Vec<_>>(),
            ["gen/out.rs"],
            "the rules of an unreadable file don't apply, as in git"
        );
    }

    #[test]
    fn an_ignore_file_with_only_invalid_lines_is_not_unreadable() {
        let project = Project::new();
        project.write(".gitignore", "a/**b/[\n");

        let walk = project.walk(&Scope::All);

        assert!(walk.findings.is_empty(), "{:?}", walk.findings);
    }

    #[test]
    fn explicit_path_ignored_help_names_the_file_and_offers_git_check_ignore() {
        let project = Project::new();
        write_exclude(&project, "");
        project.write("docs/.gitignore", "o*.rs\n");
        project.write("docs/out.rs", "");

        let walk = project.walk(&paths(&["docs/out.rs"]));

        let help = walk
            .findings
            .first()
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default();
        assert_eq!(
            help,
            "Remove that rule from `docs/.gitignore` to check `docs/out.rs`, or stop passing it. \
             `git check-ignore -v -- docs/out.rs` shows the rule"
        );
    }

    #[test]
    fn explicit_path_ignored_help_offers_no_git_command_outside_a_work_tree() {
        let project = Project::new();
        project.write(".ignore", "out.rs\n");
        project.write("out.rs", "");

        let walk = project.walk(&paths(&["out.rs"]));

        let help = walk
            .findings
            .first()
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default();
        assert_eq!(
            help,
            "Remove that rule from `.ignore` to check `out.rs`, or stop passing it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_path_ignored_help_names_a_git_exclude_file_reached_through_a_symlink() {
        let outside = outside_dir();
        let project = Project::new();
        project.write("out.rs", "");
        make_git_dir(&project.dir.path().join(".git"));
        fs::create_dir(project.dir.path().join(".git/info"))
            .expect("the fixture git directory should be creatable");
        symlink_in(
            &project,
            ".git/info/exclude",
            &outside.path().join("ignore"),
        );

        let walk = project.walk(&paths(&["out.rs"]));

        let help = walk
            .findings
            .first()
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default();
        assert!(
            help.starts_with("Remove that rule from the git exclude file `.git/info/exclude` "),
            "{help}"
        );
        assert!(
            !help.contains(RULE),
            "a line from outside was printed: {help}"
        );
    }

    /// A git work tree whose top holds a `.gitignore` of `rules` above the project `project`,
    /// which holds `out.rs`. Returns the work tree and the project's config.
    fn project_below_gitignore(rules: &str) -> (TempDir, Config) {
        let top = TempDir::new().expect("a temporary work tree should be creatable");
        let project = top.path().join("project");
        fs::create_dir_all(&project).expect("the project directory should be creatable");
        for file in ["out.rs", CONFIG_FILE_NAME] {
            fs::write(project.join(file), "").expect("the fixture file should be writable");
        }
        make_git_dir(&top.path().join(".git"));
        fs::write(top.path().join(".gitignore"), rules)
            .expect("the rules above the root should be writable");
        let config_path = AbsolutePath::new(&project.join(CONFIG_FILE_NAME))
            .expect("the temporary directory should be absolute");
        let config = Config::load_file(&config_path).expect("the fixture config should be valid");
        (top, config)
    }

    /// The help of the only finding of `walk`, or "" when it has none.
    fn only_help(walk: &Walk) -> &str {
        walk.findings
            .first()
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default()
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_gitignore_above_the_root_warns_by_kind_without_its_path() {
        use std::os::unix::fs::PermissionsExt;

        let (top, config) = project_below_gitignore("out.rs\n");
        let file = top.path().join(".gitignore");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000))
            .expect("the fixture ignore file permissions should be settable");
        if fs::read(&file).is_ok() {
            return; // Mode 000 doesn't keep this user (root) out.
        }

        let walked = walk(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes::none(),
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644))
            .expect("the fixture ignore file permissions should be restorable");

        assert_eq!(codes(&walked), ["scan-ignore-file-unreadable"]);
        assert_eq!(
            messages(&walked),
            "an ignore file above the project root can't be read, so its rules don't apply"
        );
        assert_eq!(
            only_help(&walked),
            "The files its rules would hide were checked. Make it readable, or add those files \
             to `exclude` in `supdoc.config.toml`"
        );
    }

    #[test]
    fn explicit_path_ignored_above_the_root_names_the_file_by_kind_only() {
        let (_top, config) = project_below_gitignore("out.rs\n");

        let walked = walk(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &paths(&["out.rs"]),
            &GlobalExcludes::none(),
        );

        assert_eq!(codes(&walked), ["scan-explicit-path-ignored"]);
        assert_eq!(
            only_help(&walked),
            "Remove that rule from an ignore file above the project root to check `out.rs`, or \
             stop passing it. `git check-ignore -v -- out.rs` shows the rule"
        );
    }

    #[test]
    fn explicit_path_ignored_by_a_linked_worktrees_shared_exclude_names_it_by_kind_only() {
        let outside = outside_git_dir();
        let project = Project::new();
        project.write(".git", &format!("gitdir: {}\n", outside.path().display()));
        project.write("out.rs", "");

        let walk = project.walk(&paths(&["out.rs"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-ignored"]);
        assert_eq!(
            only_help(&walk),
            "Remove that rule from the git exclude file to check `out.rs`, or stop passing it. \
             `git check-ignore -v -- out.rs` shows the rule"
        );
    }

    #[test]
    fn explicit_path_ignored_by_a_dot_ignore_offers_no_git_check_ignore_in_a_work_tree() {
        let project = Project::new();
        write_exclude(&project, "");
        project.write(".ignore", "out.rs\n");
        project.write("out.rs", "");

        let walk = project.walk(&paths(&["out.rs"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-ignored"]);
        assert_eq!(
            only_help(&walk),
            "Remove that rule from `.ignore` to check `out.rs`, or stop passing it",
            "git doesn't read `.ignore`, so `git check-ignore` can't show its rule"
        );
    }

    #[test]
    fn a_gitignore_with_a_line_that_isnt_utf8_warns_that_some_rules_may_not_apply() {
        let project = Project::new();
        fs::write(
            project.dir.path().join(".gitignore"),
            b"first.rs\n\xe9t\xe9\nlast.rs\n",
        )
        .expect("the fixture ignore file should be writable");
        project.write("first.rs", "");
        project.write("last.rs", "");

        let walk = project.walk(&Scope::All);

        assert_eq!(
            project.sources(&Scope::All),
            ["last.rs"],
            "the rules before the line that isn't UTF-8 still apply"
        );
        assert_eq!(codes(&walk), ["scan-ignore-file-unreadable"]);
        assert_eq!(
            messages(&walk),
            "`.gitignore` could not be fully read, so some of its rules may not apply"
        );
        assert!(!only_help(&walk).contains("chmod"), "{}", only_help(&walk));
    }

    #[test]
    fn a_named_path_in_the_wrong_case_says_no_rule_was_found_on_a_case_insensitive_disk() {
        let project = Project::new();
        project.write("out.rs", "");
        if !project.dir.path().join("OUT.rs").exists() {
            return; // A case-sensitive file system: `OUT.rs` is simply missing.
        }

        let walk = project.walk(&paths(&["OUT.rs"]));

        assert_eq!(codes(&walk), ["scan-explicit-path-unscanned"]);
        assert_eq!(
            messages(&walk),
            "OUT.rs was named on the command line but the walk didn't reach it, and supdoc found \
             no rule that skips it"
        );
        assert_eq!(
            only_help(&walk),
            "Check that `OUT.rs` matches its name on disk, case included, and can be read, or \
             stop passing it"
        );
    }

    #[test]
    fn explicit_path_warning_quotes_a_real_git_exclude_file() {
        let project = Project::new();
        write_exclude(&project, "out.rs\n");
        project.write("out.rs", "");

        let walk = project.walk(&paths(&["out.rs"]));

        assert!(
            messages(&walk).ends_with("is ignored by `out.rs` in .git/info/exclude"),
            "{}",
            messages(&walk)
        );
    }

    #[test]
    fn explicit_paths_the_walk_never_enters_get_one_unscanned_warning() {
        let project = Project::new();
        project.write("inner/supdoc.config.toml", "");
        project.write("inner/a.rs", "");
        project.write(".git/stray.rs", "");
        project.write("notes.txt", "x\n");

        let walk = project.walk(&paths(&[
            "inner/a.rs",
            ".git/stray.rs",
            "notes.txt",
            "missing.rs",
        ]));

        assert!(walk.sources.is_empty());
        let codes: Vec<_> = walk.findings.iter().map(|finding| finding.code).collect();
        assert_eq!(
            codes, ["scan-explicit-path-unscanned"; 3],
            "findings: {:?}",
            walk.findings
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_a_nested_project_or_the_root_are_warned_about() {
        let project = Project::new();
        project.write("inner/supdoc.config.toml", "");
        std::os::unix::fs::symlink("inner", project.dir.path().join("to_inner"))
            .expect("the symlink should be creatable");
        std::os::unix::fs::symlink(".", project.dir.path().join("to_root"))
            .expect("the symlink should be creatable");

        let walk = project.walk(&paths(&["to_inner", "to_root"]));

        let codes: Vec<_> = walk.findings.iter().map(|finding| finding.code).collect();
        assert_eq!(
            codes, ["scan-explicit-path-symlinked"; 2],
            "findings: {:?}",
            walk.findings
        );
        let help = walk
            .findings
            .get(1)
            .and_then(|finding| finding.help.as_deref())
            .unwrap_or_default();
        assert!(!help.contains("``"), "empty suggestion in: {help}");
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_becomes_a_warning_and_the_walk_continues() {
        use std::os::unix::fs::PermissionsExt;

        let project = Project::new();
        project.write("private/secret.rs", "");
        project.write("open.rs", "");
        let private = project.dir.path().join("private");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o000))
            .expect("the fixture directory permissions should be settable");

        let walk = project.walk(&Scope::All);
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755))
            .expect("the fixture directory permissions should be restorable");

        assert_eq!(
            walk.sources
                .iter()
                .map(|walked| project.relative(&walked.path))
                .collect::<Vec<_>>(),
            ["open.rs"],
        );
        assert_eq!(codes(&walk), ["scan-unreadable-dir"]);
        assert_eq!(
            walk.unread_dirs
                .iter()
                .map(|dir| (project.relative(&dir.path), dir.cause))
                .collect::<Vec<_>>(),
            [(
                "private".to_owned(),
                UnreadCause::Io(io::ErrorKind::PermissionDenied)
            )],
            "the skipped directory is recorded so its sources count as unread",
        );
        let message = &walk
            .findings
            .first()
            .expect("one warning was expected")
            .message;
        assert_eq!(
            message, "skipped private: permission denied",
            "the warning should name the directory relative to the root, not absolutely"
        );
    }

    /// Each walked source, relative to the root, with whether linking and code-blocks want it.
    fn wanted(project: &Project) -> Vec<(String, bool, bool)> {
        project
            .walk(&Scope::All)
            .sources
            .iter()
            .map(|walked| {
                (
                    project.relative(&walked.path),
                    walked.wants.links,
                    walked.wants.code_blocks,
                )
            })
            .collect()
    }

    #[test]
    fn code_blocks_include_walks_beyond_a_narrower_link_include() {
        let project = Project::new();
        project.write(
            CONFIG_FILE_NAME,
            "include = [\"docs\"]\n[features.code-blocks]\ninclude = [\"skills/**\"]\n",
        );
        project.write("docs/a.md", "");
        project.write("skills/b.md", "");
        project.write("other/c.md", "");

        assert_eq!(
            wanted(&project),
            [
                ("docs/a.md".to_owned(), true, false),
                ("skills/b.md".to_owned(), false, true),
            ],
            "linking should keep `docs` and code-blocks should add `skills`"
        );
    }

    #[test]
    fn a_directory_excluded_by_one_scope_only_is_still_walked() {
        let project = Project::new();
        project.write(
            CONFIG_FILE_NAME,
            "exclude = [\"skills\"]\n[features.code-blocks]\nexclude = [\"docs\"]\n",
        );
        project.write("docs/a.md", "");
        project.write("skills/b.md", "");

        assert_eq!(
            wanted(&project),
            [
                ("docs/a.md".to_owned(), true, false),
                ("skills/b.md".to_owned(), false, true),
            ],
            "each directory is excluded by one scope only, so the other still reads it"
        );
    }

    #[test]
    fn links_are_not_read_from_code_blocks_only_files() {
        let project = Project::new();
        project.write(
            CONFIG_FILE_NAME,
            "include = [\"docs\"]\n[features.code-blocks]\ninclude = [\"skills\"]\n",
        );
        project.write("docs/a.md", "[a](../skills/b.md)\n");
        project.write("skills/b.md", "[b](../docs/a.md)\n");
        let config = Config::load_file(
            &AbsolutePath::new(&project.dir.path().join(CONFIG_FILE_NAME))
                .expect("the temporary directory should be absolute"),
        )
        .expect("the fixture config should be valid");
        let mut read = Vec::new();

        let _front = crate::scan::assemble::read_sources(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes::none(),
            &mut crate::scan::targets::Parsed::default(),
            |source, _file, raws| read.push((source.as_str().to_owned(), raws.len())),
            |source, error| panic!("{source} should be readable: {error:?}"),
        )
        .expect("the fixture project should be read");

        assert_eq!(
            read,
            [("docs/a.md".to_owned(), 1)],
            "only linking's sources should have their links read"
        );
    }

    #[test]
    fn gitignore_applies_to_both_scopes() {
        let project = Project::new();
        project.write(
            CONFIG_FILE_NAME,
            "include = [\"docs\"]\n[features.code-blocks]\ninclude = [\"skills\"]\n",
        );
        project.write(".gitignore", "docs/draft.md\nskills/draft.md\n");
        project.write("docs/draft.md", "");
        project.write("docs/final.md", "");
        project.write("skills/draft.md", "");
        project.write("skills/final.md", "");

        assert_eq!(
            project.sources(&Scope::All),
            ["docs/final.md", "skills/final.md"]
        );
    }

    /// A cartridge over `.tgt` files that is a link target only.
    #[derive(Debug)]
    struct TargetOnly {
        /// The grammar, created on first use.
        grammar: OnceLock<tree_sitter::Language>,
    }

    impl Cartridge for TargetOnly {
        fn id(&self) -> LanguageId {
            LanguageId::new("target-only")
        }

        fn extensions(&self) -> &'static [&'static str] {
            &["tgt"]
        }

        fn grammar(&self) -> &tree_sitter::Language {
            self.grammar
                .get_or_init(|| tree_sitter_rust::LANGUAGE.into())
        }

        fn reads_links(&self) -> bool {
            false
        }

        fn symbols(&self, _file: &SourceFile) -> Result<SymbolTable, ReadError> {
            Ok(SymbolTable::default())
        }

        fn markdown_regions(&self, _file: &SourceFile) -> Result<Vec<MarkdownRegion>, ReadError> {
            Ok(Vec::new())
        }
    }

    static TARGET_ONLY: TargetOnly = TargetOnly {
        grammar: OnceLock::new(),
    };

    #[test]
    fn a_target_only_cartridge_file_is_not_a_source_but_still_resolves_as_a_target() {
        let project = Project::new();
        project.write("a.md", "");
        project.write("data.tgt", "");
        let languages = Languages::new(vec![
            Languages::builtin()
                .expect("the built-in cartridges should load")
                .for_path(Path::new("a.md"))
                .expect("markdown should be built in"),
            &TARGET_ONLY,
        ]);
        let config_path = AbsolutePath::new(&project.dir.path().join(CONFIG_FILE_NAME))
            .expect("the temporary directory should be absolute");
        let config = Config::load_file(&config_path).expect("the fixture config should be valid");

        let walked = walk(&config, &languages, &Scope::All, &GlobalExcludes::none());

        let sources: Vec<String> = walked
            .sources
            .iter()
            .map(|source| project.relative(&source.path))
            .collect();
        assert_eq!(sources, ["a.md"], "only the markdown file is a source");
        assert!(
            languages.for_path(Path::new("data.tgt")).is_some(),
            "the target-only file should still resolve as a link target"
        );
    }
}
