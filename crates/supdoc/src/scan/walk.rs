//! Walking the project tree to find the files a scan reads links from (04 S1-S6, S10).

use std::fs;
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, Match, WalkBuilder};

use crate::config::{CONFIG_FILE_NAME, Config};
use crate::finding::{Finding, Severity};
use crate::link::SourcePath;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, Languages};
use crate::scan::model::Scope;

/// Directories of version-control systems, never walked (04 S2).
const VCS_DIRS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];

/// The files a scan reads links from, and the problems met while finding them.
#[derive(Debug)]
pub struct Walk {
    /// Sorted by path (04 S8). Every entry is under the root, passes [`Config::scans`], is inside
    /// the [`Scope`], and has a cartridge (04 S6).
    pub sources: Vec<(AbsolutePath, &'static dyn Cartridge)>,
    /// Unreadable directories and explicitly named paths that are excluded or ignored, sorted.
    pub findings: Vec<Finding>,
}

/// Walks the project beneath `config.root()` and returns its sources.
///
/// The rules apply in order to each entry (04, The walk): version-control directories, ignore files
/// (`.gitignore`, `.git/info/exclude`, the global excludes file and `.ignore`, even outside a git
/// repository), nested projects, symlinks, then `Config::scans` and the cartridge check. Hidden
/// files are walked. Targets are never looked up here, so none of these rules apply to them.
///
/// A directory that can't be read becomes a finding and the walk continues, including when it is
/// the root itself: the caller decides whether an unreadable root is fatal.
///
/// Effect: reads directory listings and ignore files.
#[must_use]
pub fn walk(config: &Config, languages: &Languages, scope: &Scope) -> Walk {
    let mut sources = Vec::new();
    let mut findings = Vec::new();

    for entry in walker(config, scope) {
        match entry {
            Ok(entry) => {
                if let Some(source) = as_source(config, languages, scope, &entry) {
                    sources.push(source);
                }
            }
            Err(error) => findings.extend(unreadable_dir(&error)),
        }
    }
    sources.sort_by(|(left, _), (right, _)| left.cmp(right));

    findings.extend(explicit_path_warnings(config, languages, scope, &sources));
    findings.sort_by(|left, right| {
        left.code
            .cmp(right.code)
            .then_with(|| left.message.cmp(&right.message))
    });

    Walk { sources, findings }
}

/// A walker over the root that applies every directory-level rule, in sorted order.
fn walker(config: &Config, scope: &Scope) -> ignore::Walk {
    let root = config.root();
    let config_for_filter = config.clone();
    let scope_for_filter = scope.clone();
    WalkBuilder::new(root.as_path())
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(Path::cmp)
        .filter_entry(move |entry| descend_into(&config_for_filter, &scope_for_filter, entry))
        .build()
}

/// Whether the walk keeps `entry`. Only directories are refused here; files are judged by
/// [`as_source`]. A directory that `exclude` names is never entered, so nothing under it is read
/// and no finding can come from inside it.
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

/// Whether the config's `exclude` names the directory `dir`.
fn is_excluded_dir(config: &Config, dir: &Path) -> bool {
    AbsolutePath::new(dir).is_some_and(|dir| config.excluded_by(&dir).is_some())
}

/// Whether `dir` is named like a version-control directory (04 S2).
fn is_vcs_dir(dir: &Path) -> bool {
    dir.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| VCS_DIRS.contains(&name))
}

/// Whether `dir` has its own config, which makes it a separate project (04 S4).
fn is_nested_project(dir: &Path) -> bool {
    dir.join(CONFIG_FILE_NAME).exists()
}

/// Whether anything under `dir` can be inside `scope`, so narrowing the walk (04 S10) never
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

/// `entry` as a source, or `None` when a rule skips it.
fn as_source(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    entry: &DirEntry,
) -> Option<(AbsolutePath, &'static dyn Cartridge)> {
    // A symlink reports its own type, not its target's, so links are skipped here (04 S3).
    if !entry.file_type()?.is_file() {
        return None;
    }
    let path = AbsolutePath::new(entry.path())?;
    if !config.scans(&path) {
        return None;
    }
    let cartridge = languages.for_path(path.as_path())?;
    let relative = SourcePath::from_relative(path.strip_prefix(config.root())?).ok()?;
    scope.contains(&relative).then_some((path, cartridge))
}

/// A `scan-unreadable-dir` warning for a directory the walker couldn't read.
///
/// Other walker errors, such as a malformed ignore-file line, say nothing about what was skipped,
/// so they produce no finding.
fn unreadable_dir(error: &ignore::Error) -> Option<Finding> {
    let path = path_of(error)?;
    let cause = error.io_error()?;
    Some(
        Finding::new(
            "scan-unreadable-dir",
            Severity::Warning,
            format!("skipped {}: {}", path.display(), cause.kind()),
        )
        .with_help(
            "Links in that directory weren't checked. Fix its permissions, or add it to \
             `exclude` in `supdoc.config.toml`",
        ),
    )
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

/// Warnings for each explicitly named path that the walk skipped because of the config or an
/// ignore file (04 S10).
fn explicit_path_warnings(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    sources: &[(AbsolutePath, &'static dyn Cartridge)],
) -> Vec<Finding> {
    let Scope::Paths(listed) = scope else {
        return Vec::new();
    };
    listed
        .iter()
        .filter_map(|path| explicit_path_warning(config, languages, sources, path))
        .collect()
}

/// The warning for one explicitly named path, or `None` when it was walked or is skipped for a
/// reason that isn't a user setting (missing, a symlink, a version-control or nested-project
/// directory).
fn explicit_path_warning(
    config: &Config,
    languages: &Languages,
    sources: &[(AbsolutePath, &'static dyn Cartridge)],
    listed: &SourcePath,
) -> Option<Finding> {
    let root = config.root();
    let path = AbsolutePath::new(&root.as_path().join(listed.as_str()))?;
    let kind = fs::symlink_metadata(path.as_path()).ok()?.file_type();
    if (!kind.is_file() && !kind.is_dir()) || is_inside_skipped_dir(root, listed) {
        return None;
    }

    let ignored_by = ignore_rule(root, &path, kind.is_dir());
    let is_source_file = kind.is_file() && languages.for_path(path.as_path()).is_some();
    let walked = sources
        .binary_search_by(|(source, _)| source.cmp(&path))
        .is_ok();

    if let Some(rule) = ignored_by {
        return Some(ignored(listed, &rule));
    }
    let excluded_by = config.excluded_by(&path);
    if let Some(pattern) = excluded_by.filter(|_| is_source_file || kind.is_dir()) {
        return Some(excluded(listed, Some(pattern)));
    }
    if is_source_file && !config.scans(&path) {
        return Some(excluded(listed, None));
    }
    // Skipped by something the matchers above can't see, like the global git excludes file.
    (is_source_file && !walked).then(|| ignored(listed, "an ignore file"))
}

/// The finding for an explicit path that the config excludes, naming the `exclude` pattern when
/// one matched and otherwise blaming `include`.
fn excluded(listed: &SourcePath, pattern: Option<&str>) -> Finding {
    let listed = listed.as_str();
    match pattern {
        Some(pattern) => Finding::new(
            "scan-explicit-path-excluded",
            Severity::Warning,
            format!(
                "{listed} was named on the command line but is excluded by `{pattern}` in `exclude` in {CONFIG_FILE_NAME}",
            ),
        )
        .with_help(format!(
            "Remove `{pattern}` from `exclude` to check it, or stop passing it"
        )),
        None => Finding::new(
            "scan-explicit-path-excluded",
            Severity::Warning,
            format!(
                "{listed} was named on the command line but is not matched by `include` in {CONFIG_FILE_NAME}",
            ),
        )
        .with_help(format!(
            "Add `{listed}` to `include` to check it, or stop passing it"
        )),
    }
}

/// The finding for an explicit path that an ignore file skips, naming that `rule`.
fn ignored(listed: &SourcePath, rule: &str) -> Finding {
    Finding::new(
        "scan-explicit-path-ignored",
        Severity::Warning,
        format!(
            "{listed} was named on the command line but is ignored by {rule}",
            listed = listed.as_str(),
        ),
    )
    .with_help(format!(
        "Remove that rule from its ignore file to check `{listed}`, or stop passing it",
        listed = listed.as_str(),
    ))
}

/// Whether `listed` or one of its parent directories is a version-control directory or a nested
/// project, which the walk skips whatever the user names (04 S2, S4).
fn is_inside_skipped_dir(root: &AbsolutePath, listed: &SourcePath) -> bool {
    let mut current = root.as_path().to_path_buf();
    listed.as_str().split('/').any(|segment| {
        current.push(segment);
        is_vcs_dir(&current) || (current.is_dir() && is_nested_project(&current))
    })
}

/// Describes the first ignore-file rule that skips `path`, or `None` when no ignore file the walk
/// reads (other than the global excludes file) does.
///
/// Matchers run from the deepest directory to the root and from `.ignore` to `.gitignore`, the
/// order the walker gives them precedence in, and the first rule that speaks decides.
fn ignore_rule(root: &AbsolutePath, path: &AbsolutePath, is_dir: bool) -> Option<String> {
    let relative = path.strip_prefix(root)?;
    let mut dirs = vec![root.as_path().to_path_buf()];
    let mut current = root.as_path().to_path_buf();
    for component in relative.parent()?.components() {
        current.push(component);
        dirs.push(current.clone());
    }

    let mut matchers = vec![matcher(
        root.as_path(),
        &root.as_path().join(".git/info/exclude"),
    )];
    for dir in &dirs {
        matchers.push(matcher(dir, &dir.join(".gitignore")));
        matchers.push(matcher(dir, &dir.join(".ignore")));
    }

    matchers
        .into_iter()
        .rev()
        .flatten()
        .find_map(
            |ignore| match ignore.matched_path_or_any_parents(path.as_path(), is_dir) {
                Match::Ignore(rule) => {
                    let file = rule
                        .from()
                        .map(|from| from.strip_prefix(root.as_path()).unwrap_or(from));
                    Some(Some(match file {
                        Some(file) => format!("`{}` in {}", rule.original(), file.display()),
                        None => format!("`{}`", rule.original()),
                    }))
                }
                Match::Whitelist(_) => Some(None),
                Match::None => None,
            },
        )
        .flatten()
}

/// The matcher for `ignore_file`, whose patterns apply beneath `dir`.
fn matcher(dir: &Path, ignore_file: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(dir);
    let _partial_error = builder.add(ignore_file);
    builder.build().ok()
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

    use tempfile::TempDir;

    use super::*;

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
            walk(&config, &Languages::builtin(), scope)
        }

        /// The walked sources, relative to the root.
        fn sources(&self, scope: &Scope) -> Vec<String> {
            self.walk(scope)
                .sources
                .iter()
                .map(|(path, _)| self.relative(path))
                .collect()
        }

        fn relative(&self, path: &AbsolutePath) -> String {
            path.strip_prefix(
                &AbsolutePath::new(self.dir.path())
                    .expect("the temporary directory should be absolute"),
            )
            .expect("a walked source should be under the root")
            .to_string_lossy()
            .into_owned()
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
    fn each_source_carries_the_cartridge_for_its_extension() {
        let project = Project::new();
        project.write("lib.rs", "");

        let walk = project.walk(&Scope::All);
        let languages: Vec<_> = walk
            .sources
            .iter()
            .map(|(_, cartridge)| cartridge.id().as_str())
            .collect();
        assert_eq!(languages, ["rust"]);
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
                .map(|(path, _)| project.relative(path))
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

    #[test]
    fn explicit_paths_the_walk_never_enters_give_no_warning() {
        let project = Project::new();
        project.write("inner/supdoc.config.toml", "");
        project.write("inner/a.rs", "");
        project.write(".git/stray.rs", "");

        let walk = project.walk(&paths(&["inner/a.rs", ".git/stray.rs", "missing.rs"]));

        assert!(walk.sources.is_empty());
        assert!(
            walk.findings.is_empty(),
            "unexpected findings: {:?}",
            walk.findings
        );
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
                .map(|(path, _)| project.relative(path))
                .collect::<Vec<_>>(),
            ["open.rs"],
        );
        assert_eq!(codes(&walk), ["scan-unreadable-dir"]);
        let message = &walk
            .findings
            .first()
            .expect("one warning was expected")
            .message;
        assert!(
            message.starts_with("skipped ") && message.ends_with("private: permission denied"),
            "message was: {message}"
        );
    }
}
