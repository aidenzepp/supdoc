//! Walking the project tree to find the files a scan reads links from (04 S1-S6, S10).

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, BufRead as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ignore::gitignore::{Gitignore, GitignoreBuilder, Glob};
use ignore::{DirEntry, Match, WalkBuilder};

use crate::config::{CONFIG_FILE_NAME, Config, config_entry};
use crate::finding::{Finding, Severity};
use crate::link::SourcePath;
use crate::output::paths::relative_to;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Cartridge, Languages};
use crate::scan::model::Scope;

/// Directories of version-control systems, never walked (04 S2).
const VCS_DIRS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];

/// An ignore file a directory can hold that a checkout can plant. Inside the project root, one
/// that is a symlink is not read, wherever it points, as git doesn't read one in its work tree
/// (04 S15). Git's exclude files, reached through `.git`, are not among them: git refuses to check
/// out any `.git` path, so they are followed as git follows them, symlinks included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IgnoreFile {
    /// `.ignore`.
    Ignore,
    /// `.gitignore`.
    Gitignore,
}

impl IgnoreFile {
    /// Its path, relative to the directory it applies to.
    const fn path(self) -> &'static str {
        match self {
            Self::Ignore => ".ignore",
            Self::Gitignore => ".gitignore",
        }
    }
}

/// The files a scan reads links from, and the problems met while finding them.
#[derive(Debug)]
pub struct Walk {
    /// Sorted by path (04 S8). Every entry is under the root, passes [`Config::scans`], is inside
    /// the [`Scope`], and has a cartridge (04 S6).
    pub sources: Vec<(AbsolutePath, &'static dyn Cartridge)>,
    /// Unreadable directories, symlinked ignore files that weren't read, and explicitly named
    /// paths that are excluded or ignored, sorted.
    pub findings: Vec<Finding>,
    /// The directories the walk couldn't read, sorted by path. Nothing beneath one was walked, so
    /// its sources and their links are unknown (05 C5).
    pub unread_dirs: Vec<UnreadDir>,
}

/// A directory the walk couldn't read, so nothing beneath it was walked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnreadDir {
    /// The directory, as the walk met it.
    pub path: AbsolutePath,
    /// Why it wasn't read.
    pub cause: UnreadCause,
}

/// Why the walk didn't read a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnreadCause {
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
    fn finding(&self, root: &AbsolutePath) -> Finding {
        Finding::new(
            "scan-unreadable-dir",
            Severity::Warning,
            format!("skipped {}: {}", relative_to(root, &self.path), self.cause),
        )
        .with_help(
            "Links in that directory weren't checked. Fix its permissions, or add it to `exclude` \
             in `supdoc.config.toml`",
        )
    }
}

/// Walks the project beneath `config.root()` and returns its sources.
///
/// The rules apply in order to each entry (04, The walk): ignore files ([`IgnoreRules`]: each
/// directory's `.ignore`, `.gitignore` and `.git/info/exclude`, those of the directories above the
/// root up to the top of its git work tree, and the global excludes file, even outside a git
/// repository), version-control directories, nested projects, symlinks, then `Config::scans` and
/// the cartridge check. Hidden files are walked. Targets are never looked up here, so none of
/// these rules apply to them.
///
/// A directory that can't be read becomes a finding and the walk continues, including when it is
/// the root itself: the caller decides whether an unreadable root is fatal. A `.ignore` or
/// `.gitignore` inside the root that is a symlink is not read, and becomes a finding too.
///
/// Effect: reads directory listings, ignore files and file metadata.
#[must_use]
pub fn walk(config: &Config, languages: &Languages, scope: &Scope) -> Walk {
    let root = config.root();
    let rules = Arc::new(IgnoreRules::new(root));
    let mut sources = Vec::new();
    let mut unread_dirs = Vec::new();

    for entry in walker(config, scope, &rules) {
        match entry {
            Ok(entry) => sources.extend(as_source(config, languages, scope, &entry)),
            Err(error) => unread_dirs.extend(unreadable_dir(&error)),
        }
    }
    sources.sort_by(|(left, _), (right, _)| left.cmp(right));
    unread_dirs.sort();

    let mut findings: Vec<Finding> = unread_dirs.iter().map(|dir| dir.finding(root)).collect();
    // Taken before the explicit paths are judged, which may read directories the walk never
    // entered: only an ignore file the walk met is worth a warning.
    findings.extend(
        rules
            .symlinked_files()
            .iter()
            .map(|file| symlinked_ignore_file(root, file)),
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

/// The ignore rules the walk applies and the explicit-path warnings quote from, so the rule a
/// warning names is the rule that decided (04 S15).
///
/// They follow git and the walker's defaults: for each kind of file, `.ignore`, then `.gitignore`,
/// then the git exclude file, the deepest directory with a rule that matches decides, from the
/// entry's own directory up through the root and on through the directories above it. The first
/// kind with a match wins, whitelist or not, and the global excludes file comes last. So a
/// `!rule` in a `.gitignore` overrides the exclude file and the global excludes file, as in git.
///
/// The directories above the root apply only up to the top of its git work tree, the nearest
/// directory holding a `.git`, the root itself included, as git reads no `.gitignore` above that
/// top. With the root at the top, or no `.git` in the root or above it, no directory above applies.
/// The global excludes file's anchored patterns count from that top, or from the root with no
/// `.git`, whatever directory supdoc runs from.
///
/// A `.git` marks the top as git decides ([`holds_git`]): a `.git` file always does, and a `.git`
/// directory only with git's repository layout, so a missing or dangling `.git`, or a directory
/// git rejects, is passed over and the search goes on upward.
///
/// Known differences from git: rules match case-sensitively whatever `core.ignorecase` says, and
/// `core.excludesFile` is read only from the user and system git config, never from a repository's
/// own config or the files it includes. A `.git` directory's `HEAD` isn't read, so one with
/// `objects` and `refs` but a corrupt `HEAD` still marks the top here, where git searches further
/// up.
///
/// Inside the root, a `.ignore` or `.gitignore` that is a symlink is not read, whatever it points
/// to. Above the root, every ignore file is followed, symlinks included, and none is ever quoted.
struct IgnoreRules {
    /// The project root, as the walk reaches it.
    root: PathBuf,
    /// The root with every symlink resolved, which the directories above it are found from and
    /// matched against. `None` when it can't be resolved, and then no directory above applies.
    canonical_root: Option<PathBuf>,
    /// The rules of the directory just above the root, linked to those above it up to the top of
    /// the git work tree. `None` when no directory above applies.
    above: Option<Arc<Level>>,
    /// The global excludes file, such as `~/.config/git/ignore`.
    global: Gitignore,
    /// The rules of the root and of each directory beneath it read so far, by path.
    inside: Mutex<HashMap<PathBuf, Arc<Level>>>,
}

/// One directory's ignore rules, linked to those of the directory above it.
struct Level {
    /// The directory, as the walk reaches it.
    dir: PathBuf,
    /// The rules of its `.ignore`.
    ignore: Gitignore,
    /// The rules of its `.gitignore`.
    gitignore: Gitignore,
    /// The rules of its git exclude file ([`git_exclude`]).
    exclude: Gitignore,
    /// Where it lies, which decides what paths its rules see and which may be quoted.
    place: Place,
    /// Its ignore files that are symlinks, which were not read. Always empty above the root.
    symlinked: Vec<IgnoreFile>,
    /// The directory above it, if one applies.
    parent: Option<Arc<Self>>,
}

/// Where a directory of ignore rules lies relative to the project root.
#[derive(Debug, Clone, Copy)]
enum Place {
    /// The root or a directory beneath it. Its `.ignore` and `.gitignore` were read only if they
    /// aren't symlinks, so their rules may be quoted; its exclude file's only when
    /// `quote_exclude` says it lies inside.
    Inside {
        /// Whether the exclude file's rules may be quoted.
        quote_exclude: bool,
    },
    /// A directory above the root: its rules see paths beneath the resolved root, and none of
    /// them is ever quoted.
    Above,
}

/// The rule that decided whether a path is ignored, and whether its line may be quoted.
#[derive(Debug, Clone, Copy)]
struct Rule<'a> {
    /// The rule.
    glob: &'a Glob,
    /// Whether it comes from an ignore file inside the root that isn't a symlink.
    quotable: bool,
}

impl Rule<'_> {
    /// How a warning names this rule: the line and its file, relative to `root`, when it may be
    /// quoted, and "an ignore file" otherwise, so no line from outside the root is ever printed.
    fn describe(self, root: &Path) -> String {
        if !self.quotable {
            return "an ignore file".to_owned();
        }
        let rule = self.glob.original();
        match self.glob.from() {
            Some(file) => format!(
                "`{rule}` in {}",
                file.strip_prefix(root).unwrap_or(file).display()
            ),
            None => format!("`{rule}`"),
        }
    }
}

impl IgnoreRules {
    /// The rules for a walk of `root`. Reads the ignore files of the directories above it, up to
    /// the top of its git work tree, and the global excludes file now; the root's own, and those
    /// beneath it, as the walk asks.
    ///
    /// Effect: resolves `root`'s symlinks, reads file metadata in it and the directories above
    /// it, and reads ignore files, git files and git config.
    fn new(root: &AbsolutePath) -> Self {
        let canonical_root = fs::canonicalize(root.as_path()).ok();
        let top = canonical_root
            .as_deref()
            .and_then(|canonical| canonical.ancestors().position(holds_git));
        let above = canonical_root
            .as_deref()
            .zip(top)
            .and_then(|(canonical, top)| {
                let parents: Vec<&Path> = canonical.ancestors().take(top + 1).skip(1).collect();
                parents.into_iter().rev().fold(None, |parent, dir| {
                    Some(Arc::new(Level::above(dir, parent)))
                })
            });
        // The global excludes anchor at the top of the git work tree, or at the root with none, so
        // an anchored pattern means the same wherever supdoc runs from.
        let anchor = canonical_root
            .as_deref()
            .zip(top)
            .and_then(|(canonical, top)| canonical.ancestors().nth(top))
            .or(canonical_root.as_deref())
            .unwrap_or_else(|| root.as_path());
        let (global, _unreadable) = GitignoreBuilder::new(anchor).build_global();
        Self {
            root: root.as_path().to_path_buf(),
            canonical_root,
            above,
            global,
            inside: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the walk skips `path`, an entry of the root or of a directory beneath it.
    ///
    /// Effect: reads the ignore files of `path`'s directory and those above it, once each.
    fn ignores(&self, path: &Path, is_dir: bool) -> bool {
        let dir = path.parent().and_then(|dir| self.level(dir));
        self.matched(dir.as_deref(), path, is_dir).is_ignore()
    }

    /// The rules `dir` and the directories above it hold, read once and kept. A directory outside
    /// the root gets only those above the root.
    ///
    /// Effect: reads the ignore files and file metadata of `dir` and its parents within the root,
    /// the first time each is asked for.
    fn level(&self, dir: &Path) -> Option<Arc<Level>> {
        if !dir.starts_with(&self.root) {
            return self.above.clone();
        }
        if let Some(level) = self.read_so_far().get(dir) {
            return Some(Arc::clone(level));
        }
        let parent = if dir == self.root {
            self.above.clone()
        } else {
            dir.parent().and_then(|parent| self.level(parent))
        };
        let level = Arc::new(Level::inside(dir, parent));
        Some(Arc::clone(
            self.read_so_far().entry(dir.to_path_buf()).or_insert(level),
        ))
    }

    /// The deciding rule for `path` beneath the directory whose rules are `level`, as a
    /// [`Match`]: ignored, whitelisted, or untouched.
    fn matched<'a>(
        &'a self,
        level: Option<&'a Level>,
        path: &Path,
        is_dir: bool,
    ) -> Match<Rule<'a>> {
        let resolved = self
            .canonical_root
            .as_deref()
            .zip(path.strip_prefix(&self.root).ok())
            .map(|(canonical, relative)| canonical.join(relative));
        let (mut ignore, mut gitignore, mut exclude) = (Match::None, Match::None, Match::None);
        let mut current = level;
        while let Some(level) = current {
            let (seen, quote_files, quote_exclude) = match level.place {
                Place::Inside { quote_exclude } => (path, true, quote_exclude),
                Place::Above => (resolved.as_deref().unwrap_or(path), false, false),
            };
            if ignore.is_none() {
                ignore = rule(&level.ignore, seen, is_dir, quote_files);
            }
            if gitignore.is_none() {
                gitignore = rule(&level.gitignore, seen, is_dir, quote_files);
            }
            if exclude.is_none() {
                exclude = rule(&level.exclude, seen, is_dir, quote_exclude);
            }
            current = level.parent.as_deref();
        }
        let global = rule(
            &self.global,
            resolved.as_deref().unwrap_or(path),
            is_dir,
            false,
        );
        ignore.or(gitignore).or(exclude).or(global)
    }

    /// Every ignore file inside the root that the walk met as a symlink, and so didn't read.
    fn symlinked_files(&self) -> Vec<AbsolutePath> {
        self.read_so_far()
            .values()
            .flat_map(|level| {
                level
                    .symlinked
                    .iter()
                    .filter_map(|file| AbsolutePath::new(&level.dir.join(file.path())))
            })
            .collect()
    }

    /// The directories read so far. A panic while one was being added leaves the map whole, so a
    /// poisoned lock is still safe to use.
    fn read_so_far(&self) -> MutexGuard<'_, HashMap<PathBuf, Arc<Level>>> {
        self.inside.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How `matcher` judges `path`, with the rule marked `quotable` or not.
fn rule<'a>(matcher: &'a Gitignore, path: &Path, is_dir: bool, quotable: bool) -> Match<Rule<'a>> {
    matcher
        .matched(path, is_dir)
        .map(|glob| Rule { glob, quotable })
}

impl Level {
    /// The rules of `dir`, the root or a directory beneath it. A `.ignore` or `.gitignore` there
    /// that is a symlink is not read, wherever it points (04 S15): only an `lstat` looks at it.
    ///
    /// Effect: reads file metadata in `dir`, its ignore files and its git exclude file.
    fn inside(dir: &Path, parent: Option<Arc<Self>>) -> Self {
        let mut symlinked = Vec::new();
        let mut own = |file: IgnoreFile| {
            let path = dir.join(file.path());
            match fs::symlink_metadata(&path) {
                Err(_) => Gitignore::empty(),
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    symlinked.push(file);
                    Gitignore::empty()
                }
                Ok(_) => rules_in(dir, &path),
            }
        };
        let ignore = own(IgnoreFile::Ignore);
        let gitignore = own(IgnoreFile::Gitignore);
        let (exclude, quote_exclude) = git_exclude(dir);
        Self {
            dir: dir.to_path_buf(),
            ignore,
            gitignore,
            exclude,
            place: Place::Inside { quote_exclude },
            symlinked,
            parent,
        }
    }

    /// The rules of `dir`, a directory above the root and at most the top of its git work tree.
    /// Its ignore files lie outside the checkout, as a monorepo's shared `.gitignore` does, so
    /// they are followed wherever they lead.
    ///
    /// Effect: reads `dir`'s ignore files and its git exclude file.
    fn above(dir: &Path, parent: Option<Arc<Self>>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            ignore: rules_in(dir, &dir.join(IgnoreFile::Ignore.path())),
            gitignore: rules_in(dir, &dir.join(IgnoreFile::Gitignore.path())),
            exclude: git_exclude(dir).0,
            place: Place::Above,
            symlinked: Vec::new(),
            parent,
        }
    }
}

/// Whether `dir` is the top of a git work tree, as git decides it: `dir/.git`, followed through
/// symlinks, is a file (a gitfile, which git either uses or refuses to run past), or a git
/// directory ([`is_git_dir`]). A missing or dangling `.git`, or a directory git rejects, isn't, and
/// git searches further up.
///
/// Effect: reads file metadata in `dir` and its `.git` directory.
fn holds_git(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    match fs::metadata(&dot_git) {
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
fn is_git_dir(git_dir: &Path) -> bool {
    fs::symlink_metadata(git_dir.join("HEAD"))
        .is_ok_and(|metadata| metadata.is_file() || metadata.is_symlink())
        && fs::metadata(git_dir.join("objects")).is_ok_and(|metadata| metadata.is_dir())
        && fs::metadata(git_dir.join("refs")).is_ok_and(|metadata| metadata.is_dir())
}

/// The rules of `file`, which apply beneath `dir`. A missing or unreadable file has none, and a
/// line that isn't a valid pattern is left out, as the walker treats them.
///
/// Effect: reads `file`, following symlinks.
fn rules_in(dir: &Path, file: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(dir);
    let _unread_or_invalid = builder.add(file);
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// The rules of `dir`'s git exclude file, and whether they may be quoted.
///
/// The file is `info/exclude` in the git directory `dir/.git` leads to: the directory itself or
/// where a `.git` symlink points, when git accepts it as one ([`is_git_dir`]), or for a `.git` file
/// the shared directory ([`common_dir`]) of the git directory it names. Each is followed wherever
/// it leads, as git follows it: none can come from a checkout's content, since git refuses to
/// check out any `.git` path (04 S15).
///
/// Its rules may be quoted only when `.git`, `.git/info` and the exclude file are a real
/// directory, directory and file, no symlink among them, so the file lies inside `dir`.
///
/// Effect: reads file metadata in `dir`, a `.git` file and its `commondir`, and the exclude file.
fn git_exclude(dir: &Path) -> (Gitignore, bool) {
    let dot_git = dir.join(".git");
    let Ok(metadata) = fs::metadata(&dot_git) else {
        return (Gitignore::empty(), false);
    };
    let git_dir = if metadata.is_file() {
        let Some(common) = common_dir(dir, &dot_git) else {
            return (Gitignore::empty(), false);
        };
        common
    } else if is_git_dir(&dot_git) {
        dot_git.clone()
    } else {
        return (Gitignore::empty(), false);
    };
    let exclude = git_dir.join("info").join("exclude");
    let quotable = is_real_dir(&dot_git)
        && is_real_dir(&dot_git.join("info"))
        && fs::symlink_metadata(&exclude).is_ok_and(|metadata| metadata.is_file());
    (rules_in(dir, &exclude), quotable)
}

/// The shared git directory of the worktree whose `.git` file in `dir` is `dot_git`: its
/// `gitdir:` line names the worktree's own git directory, whose `commondir` file names the shared
/// one. With no `commondir` file, as in a submodule or a `--separate-git-dir` repository, the
/// named git directory is itself the shared one, as in git. Each relative path counts from where
/// git reads it, `dir` and the worktree's git directory. `None` when `dot_git` is unreadable or has
/// no `gitdir: ` line, or a `commondir` file is there but unreadable or empty, so no exclude file
/// applies.
///
/// Effect: reads `dot_git` and the `commondir` file.
fn common_dir(dir: &Path, dot_git: &Path) -> Option<PathBuf> {
    let line = first_line(dot_git)?;
    let git_dir = dir.join(line.strip_prefix("gitdir: ")?);
    let commondir = git_dir.join("commondir");
    if fs::symlink_metadata(&commondir).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        return Some(git_dir);
    }
    let common = first_line(&commondir)?;
    Some(git_dir.join(common))
}

/// The first line of `file`, without its line ending, or `None` when it can't be read.
///
/// Effect: reads `file`.
fn first_line(file: &Path) -> Option<String> {
    let file = fs::File::open(file).ok()?;
    io::BufReader::new(file).lines().next()?.ok()
}

/// Whether `path` is a directory itself, not a symlink to one.
///
/// Effect: reads `path`'s metadata.
fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The warning for `file`, an ignore file inside the root that is a symlink and so wasn't read.
/// The message names it relative to `root`, as the other walk findings name paths.
fn symlinked_ignore_file(root: &AbsolutePath, file: &AbsolutePath) -> Finding {
    Finding::new(
        "scan-ignore-file-symlink",
        Severity::Warning,
        format!(
            "{} is a symlink, so its rules don't apply",
            relative_to(root, file)
        ),
    )
    .with_help(format!(
        "Like git, supdoc doesn't read an ignore file that is a symlink inside the project, so \
         the files its rules would hide are checked. Replace the symlink with a regular file \
         holding those rules, or add the paths to `exclude` in `{CONFIG_FILE_NAME}`"
    ))
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
///
/// The same presence rule as the config search: an entry named `supdoc.config.toml` counts only
/// when a no-follow check shows a regular file or a symlink, a dangling one included. A
/// directory or a FIFO with that name does not make a nested project.
fn is_nested_project(dir: &Path) -> bool {
    config_entry(dir).is_some()
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

/// Warnings for each explicitly named path that the walk skipped because of the config or an
/// ignore file (04 S10). A path beneath one of `unread_dirs` gets none: that directory's own
/// warning already explains it.
fn explicit_path_warnings(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    rules: &IgnoreRules,
    sources: &[(AbsolutePath, &'static dyn Cartridge)],
    unread_dirs: &[UnreadDir],
) -> Vec<Finding> {
    let Scope::Paths(listed) = scope else {
        return Vec::new();
    };
    listed
        .iter()
        .filter(|path| {
            let path = config.root().as_path().join(path.as_str());
            !unread_dirs
                .iter()
                .any(|dir| path.starts_with(dir.path.as_path()))
        })
        .filter_map(|path| explicit_path_warning(config, languages, rules, sources, path))
        .collect()
}

/// The warning for one explicitly named path, or `None` when it was walked or is skipped for a
/// reason that isn't a user setting (missing, a symlink, a version-control or nested-project
/// directory).
fn explicit_path_warning(
    config: &Config,
    languages: &Languages,
    rules: &IgnoreRules,
    sources: &[(AbsolutePath, &'static dyn Cartridge)],
    listed: &SourcePath,
) -> Option<Finding> {
    let root = config.root();
    let path = AbsolutePath::new(&root.as_path().join(listed.as_str()))?;
    let kind = fs::symlink_metadata(path.as_path()).ok()?.file_type();
    if (!kind.is_file() && !kind.is_dir()) || is_inside_skipped_dir(root, listed) {
        return None;
    }

    let ignored_by = ignore_rule(rules, root, &path, kind.is_dir());
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
    // Not reached for a reason the checks above don't name, such as a symlinked directory on the
    // way to it, which the walk never enters.
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

/// Describes the ignore rule that keeps the walk from reaching `path`, or `None` when none does.
///
/// It asks what the walk asked, with the same [`IgnoreRules`] and in the order the walk would:
/// each directory on the way down from the root, then `path` itself. The walk never enters a
/// symlinked directory, so nothing beneath one is judged. A rule is quoted with its file only when
/// that file lies inside the root and is no symlink; any other is blamed on "an ignore file", so
/// no line from outside the root is ever printed (04 S15).
///
/// Effect: reads ignore files and file metadata beneath the root.
fn ignore_rule(
    rules: &IgnoreRules,
    root: &AbsolutePath,
    path: &AbsolutePath,
    is_dir: bool,
) -> Option<String> {
    let mut current = root.as_path().to_path_buf();
    let mut components = path.strip_prefix(root)?.components().peekable();
    while let Some(component) = components.next() {
        let is_last = components.peek().is_none();
        let dir = rules.level(&current);
        current.push(component);
        if !is_last && is_symlink(&current) {
            return None;
        }
        if let Match::Ignore(rule) = rules.matched(dir.as_deref(), &current, !is_last || is_dir) {
            return Some(rule.describe(root.as_path()));
        }
    }
    None
}

/// Whether `path` is itself a symlink.
///
/// Effect: reads `path`'s metadata.
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
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

    #[cfg(unix)]
    #[test]
    fn ignore_file_above_the_root_symlinked_elsewhere_applies_unquoted() {
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
        // A monorepo's shared `.ignore`, say: outside the checkout, so followed as git would.
        std::os::unix::fs::symlink(outside.path().join("ignore"), above.path().join(".ignore"))
            .expect("the fixture symlink should be creatable");
        let config_path = AbsolutePath::new(&above.path().join("project").join(CONFIG_FILE_NAME))
            .expect("the temporary directory should be absolute");
        let config = Config::load_file(&config_path).expect("the fixture config should be valid");

        let all = walk(&config, &Languages::builtin(), &Scope::All);
        let one = walk(&config, &Languages::builtin(), &paths(&["out.rs"]));

        let walked: Vec<_> = all
            .sources
            .iter()
            .filter_map(|(path, _)| path.as_path().file_name())
            .collect();
        assert_eq!(walked, ["kept.rs"], "the parent's rule applies");
        assert!(all.unread_dirs.is_empty(), "{:?}", all.unread_dirs);
        assert!(
            all.findings.is_empty(),
            "a symlink above the root is followed without a warning: {:?}",
            all.findings
        );
        assert_eq!(codes(&one), ["scan-explicit-path-ignored"]);
        assert!(
            !messages(&one).contains(RULE),
            "a line from outside the root was printed:\n{}",
            messages(&one)
        );
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
}
