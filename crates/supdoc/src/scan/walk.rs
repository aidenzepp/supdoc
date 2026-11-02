//! Walking the project tree to find the files a scan reads links from (04 S1-S6, S10).

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, BufRead as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ignore::gitignore::{Gitignore, GitignoreBuilder, Glob};
use ignore::{DirEntry, Match, WalkBuilder};

use crate::config::{CONFIG_FILE_NAME, Config, PatternDefinition, config_entry};
use crate::finding::{Finding, Severity};
use crate::limits::SizeLimit;
use crate::link::{LinkPathError, SourcePath};
use crate::lock::error::shell_quote;
use crate::output::paths::relative_to;
use crate::path::{AbsolutePath, holds_git, is_git_dir};
use crate::read::cartridge::{Cartridge, Languages};
use crate::scan::model::Scope;

/// Directories of version-control systems, never walked (04 S2).
const VCS_DIRS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];

/// An ignore file a directory can hold that a checkout can plant. One that is a symlink is not
/// read, wherever it points, as git doesn't read one in its work tree (04 S15): inside the project
/// root, and in the directories above it, which lie inside the work tree too. Git's exclude files,
/// reached through `.git`, are not among them: git refuses to check out any `.git` path, so they
/// are followed as git follows them, symlinks included.
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
        let shown = relative_to(root, &self.path);
        Finding::new(
            "scan-unreadable-dir",
            Severity::Warning,
            format!("skipped {shown}: {}", self.cause),
        )
        .with_help(format!(
            "Links in that directory weren't checked. Run `chmod u+rx {dir}`, or add `exclude = \
             [{pattern:?}]` to {CONFIG_FILE_NAME}",
            dir = shell_quote(&shown),
            pattern = shown,
        ))
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
/// `.gitignore` inside the root that is a symlink is not read, and becomes a finding too, as does
/// an ignore file that exists but can't be read, inside the root or above it.
///
/// Effect: reads directory listings, ignore files and file metadata.
#[must_use]
pub fn walk(config: &Config, languages: &Languages, scope: &Scope) -> Walk {
    let root = config.root();
    let rules = Arc::new(IgnoreRules::new(
        root,
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
    sources.sort_by(|(left, _), (right, _)| left.cmp(right));
    unread_dirs.sort();

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
/// A directory beneath the root holding a `.git`, a nested repository or a submodule, is the top
/// of the work tree its own files sit in, so for them the `.gitignore` and exclude rules stop
/// there, its own included, as git run in that repository and the walker stop. Every `.ignore`
/// still applies, up through the root and above it, and so does the global excludes file, still
/// anchored at the project's top. The directory itself is judged by the rules above it, so an
/// outer rule naming it still skips it.
///
/// A `.git` marks the top as git decides ([`holds_git`]): a `.git` file always does, and a `.git`
/// directory only with git's repository layout, so a missing or dangling `.git`, or a directory
/// git rejects, is passed over and the search goes on upward.
///
/// Known differences from git: rules match case-sensitively whatever `core.ignorecase` says, and
/// `core.excludesFile` is read only from the user and system git config, never from a repository's
/// own config or the files it includes. A `.git` directory's `HEAD` isn't read, so one with
/// `objects` and `refs` but a corrupt `HEAD` still marks the top here, where git searches further
/// up. The root and the directories above it are never judged themselves, so a rule above the root
/// that ignores the root, or a directory on the way to it, hides nothing beneath it, where git
/// ignores it all.
///
/// A `.ignore` or `.gitignore` that is a symlink is not read, whatever it points to, inside the
/// root or above it, as every directory above lies inside the git work tree. Only inside the root
/// is such a symlink reported, and no rule line from above the root is ever quoted.
struct IgnoreRules {
    /// The project root, as the walk reaches it.
    root: AbsolutePath,
    /// The root with every symlink resolved, which the directories above it are found from and
    /// matched against. `None` when it can't be resolved, and then no directory above applies.
    canonical_root: Option<PathBuf>,
    /// The rules of the directory just above the root, linked to those above it up to the top of
    /// the git work tree. `None` when no directory above applies.
    above: Option<Arc<Level>>,
    /// The global excludes file, such as `~/.config/git/ignore`.
    global: Gitignore,
    /// Whether the root lies in a git work tree, where `git check-ignore` can name a rule.
    in_work_tree: bool,
    /// The rules of the root and of each directory beneath it read so far, by path.
    inside: Mutex<HashMap<PathBuf, Arc<Level>>>,
    /// How large each ignore file may be. A larger one is not read and becomes an error.
    limit: SizeLimit,
}

/// One directory's ignore rules, linked to those of the directory above it.
struct Level {
    /// The directory, as the walk reaches it.
    dir: AbsolutePath,
    /// The rules of its `.ignore`.
    ignore: Gitignore,
    /// The rules of its `.gitignore`.
    gitignore: Gitignore,
    /// The rules of its git exclude file ([`git_exclude`]).
    exclude: Gitignore,
    /// Where it lies, which decides what paths its rules see and which may be quoted.
    place: Place,
    /// Its ignore files that are symlinks, which were not read. Left empty above the root, where
    /// they are passed over without a report.
    symlinked: Vec<IgnoreFile>,
    /// Its ignore files, the exclude file among them, that exist but couldn't be read, so their
    /// rules don't apply.
    unreadable: Vec<UnreadableFile>,
    /// Whether it is the top of a git work tree nested beneath the root, past which the
    /// `.gitignore` and exclude rules of the directories above don't apply. Never the root or a
    /// directory above it, where the chain already ends at the top.
    is_nested_top: bool,
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

/// The rule that decided whether a path is ignored, and where its file lies.
#[derive(Debug, Clone, Copy)]
struct Rule<'a> {
    /// The rule.
    glob: &'a Glob,
    /// Where its file lies, which decides whether its line may be quoted.
    origin: Origin,
    /// Whether git reads its file, as it reads a `.gitignore`, an exclude file and the global
    /// excludes file but not a `.ignore`, so `git check-ignore` can show the rule.
    git_reads: bool,
}

/// Where an ignore file lies, which decides how a warning may name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A `.ignore`, `.gitignore` or git exclude file inside the root that isn't reached through a
    /// symlink: its path and lines may be quoted.
    Inside,
    /// A git exclude file reached through a symlink or a `.git` file, which may lie anywhere, as in
    /// a linked worktree or a `--separate-git-dir` checkout: its lines are never printed, and its
    /// path only when it lies inside the root.
    Exclude,
    /// An ignore file in a directory above the root: neither its path nor its lines are printed.
    Above,
    /// The global excludes file: its path is named, never its lines.
    Global,
}

impl Origin {
    /// How a warning names `file`, an ignore file of this origin: by kind, and by path relative
    /// to `root` only when it lies inside the root, by whole components once `..` is resolved.
    /// The global excludes file is the user's own and is named by its path. Never by its
    /// contents (04 S15).
    fn name(self, root: &AbsolutePath, file: Option<&Path>) -> String {
        let within = file
            .and_then(AbsolutePath::new)
            .filter(|file| file.as_path().starts_with(root.as_path()))
            .map(|file| relative_to(root, &file));
        match self {
            Self::Inside => within.map_or_else(
                || "its ignore file".to_owned(),
                |shown| format!("`{shown}`"),
            ),
            Self::Exclude => within.map_or_else(
                || "the git exclude file".to_owned(),
                |shown| format!("the git exclude file `{shown}`"),
            ),
            Self::Above => "an ignore file above the project root".to_owned(),
            Self::Global => file.map_or_else(
                || "the global excludes file".to_owned(),
                |file| format!("the global excludes file `{}`", file.display()),
            ),
        }
    }
}

/// How an explicit-path warning names the rule that skipped the path and the file holding it.
struct IgnoredBy {
    /// The rule for the message: its line and file when it may be quoted, and "an ignore file"
    /// otherwise, so no line from outside the root is ever printed.
    rule: String,
    /// The file holding the rule for the help, by kind and path ([`Origin::name`]).
    file: String,
    /// Whether `git check-ignore` can show the rule: the root lies in a git work tree and the
    /// rule comes from a file git reads, never a `.ignore`.
    check_ignore: bool,
}

impl Rule<'_> {
    /// How a warning names this rule and its file, relative to `root`.
    fn describe(self, root: &AbsolutePath, in_work_tree: bool) -> IgnoredBy {
        let from = self.glob.from();
        let rule = match (self.origin, from) {
            (Origin::Inside, Some(file)) => format!(
                "`{}` in {}",
                self.glob.original(),
                file.strip_prefix(root.as_path()).unwrap_or(file).display()
            ),
            (Origin::Inside, None) => format!("`{}`", self.glob.original()),
            _ => "an ignore file".to_owned(),
        };
        IgnoredBy {
            rule,
            file: self.origin.name(root, from),
            check_ignore: in_work_tree && self.git_reads,
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
    fn new(root: &AbsolutePath, limit: SizeLimit) -> Self {
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
                    Some(Arc::new(Level::above(root.join_path(dir), parent, limit)))
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
            root: root.clone(),
            canonical_root,
            above,
            global,
            in_work_tree: top.is_some(),
            inside: Mutex::new(HashMap::new()),
            limit,
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
        if !dir.starts_with(self.root.as_path()) {
            return self.above.clone();
        }
        if let Some(level) = self.read_so_far().get(dir) {
            return Some(Arc::clone(level));
        }
        let (parent, is_nested_top) = if dir == self.root.as_path() {
            (self.above.clone(), false)
        } else {
            (
                dir.parent().and_then(|parent| self.level(parent)),
                holds_git(dir),
            )
        };
        let level = Arc::new(Level::inside(
            self.root.join_path(dir),
            parent,
            is_nested_top,
            self.limit,
        ));
        Some(Arc::clone(
            self.read_so_far().entry(dir.to_path_buf()).or_insert(level),
        ))
    }

    /// The deciding rule for `path` beneath the directory whose rules are `level`, as a
    /// [`Match`]: ignored, whitelisted, or untouched. Past a nested work tree's top, only `.ignore`
    /// rules and the global excludes file still apply.
    fn matched<'a>(
        &'a self,
        level: Option<&'a Level>,
        path: &Path,
        is_dir: bool,
    ) -> Match<Rule<'a>> {
        let resolved = self
            .canonical_root
            .as_deref()
            .zip(path.strip_prefix(self.root.as_path()).ok())
            .map(|(canonical, relative)| canonical.join(relative));
        let (mut ignore, mut gitignore, mut exclude) = (Match::None, Match::None, Match::None);
        let mut current = level;
        let mut passed_top = false;
        while let Some(level) = current {
            let (seen, files, exclude_file) = match level.place {
                Place::Inside { quote_exclude } => (
                    path,
                    Origin::Inside,
                    if quote_exclude {
                        Origin::Inside
                    } else {
                        Origin::Exclude
                    },
                ),
                Place::Above => (
                    resolved.as_deref().unwrap_or(path),
                    Origin::Above,
                    Origin::Above,
                ),
            };
            if ignore.is_none() {
                ignore = rule(&level.ignore, seen, is_dir, files, false);
            }
            if gitignore.is_none() && !passed_top {
                gitignore = rule(&level.gitignore, seen, is_dir, files, true);
            }
            if exclude.is_none() && !passed_top {
                exclude = rule(&level.exclude, seen, is_dir, exclude_file, true);
            }
            passed_top = passed_top || level.is_nested_top;
            current = level.parent.as_deref();
        }
        let global = rule(
            &self.global,
            resolved.as_deref().unwrap_or(path),
            is_dir,
            Origin::Global,
            true,
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
                    .map(|file| level.dir.join_path(Path::new(file.path())))
            })
            .collect()
    }

    /// Every ignore file the walk met that exists but couldn't be read: those above the root, all
    /// read up front, and those of the directories inside it read so far.
    fn unreadable_files(&self) -> Vec<UnreadableFile> {
        let mut files = Vec::new();
        let mut above = self.above.as_deref();
        while let Some(level) = above {
            files.extend(level.unreadable.iter().cloned());
            above = level.parent.as_deref();
        }
        files.extend(
            self.read_so_far()
                .values()
                .flat_map(|level| level.unreadable.iter().cloned()),
        );
        files
    }

    /// The directories read so far. A panic while one was being added leaves the map whole, so a
    /// poisoned lock is still safe to use.
    fn read_so_far(&self) -> MutexGuard<'_, HashMap<PathBuf, Arc<Level>>> {
        self.inside.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How `matcher`, whose file lies at `origin` and is one git reads or not, judges `path`.
fn rule<'a>(
    matcher: &'a Gitignore,
    path: &Path,
    is_dir: bool,
    origin: Origin,
    git_reads: bool,
) -> Match<Rule<'a>> {
    matcher.matched(path, is_dir).map(|glob| Rule {
        glob,
        origin,
        git_reads,
    })
}

/// An ignore file that exists but couldn't be read in full, so some or all of its rules don't
/// apply.
#[derive(Debug, Clone)]
struct UnreadableFile {
    /// The file.
    path: AbsolutePath,
    /// Where it lies, which decides how a warning names it.
    origin: Origin,
    /// How reading it failed.
    failure: ReadFailure,
}

/// How reading an ignore file that exists failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReadFailure {
    /// Permission to read it was denied, so none of its rules apply.
    Denied,
    /// Reading stopped partway, as at a line that isn't UTF-8 or with a directory in its place, so
    /// the rules after that point don't apply.
    Partial,
    /// It is larger than `max-ignore-size`, so it wasn't read and none of its rules apply.
    TooLarge {
        /// Its size in bytes.
        size: u64,
        /// The ceiling in bytes.
        limit: u64,
    },
}

impl Level {
    /// The rules of `dir`, the root or a directory beneath it. A `.ignore` or `.gitignore` there
    /// that is a symlink is not read, wherever it points (04 S15): only an `lstat` looks at it.
    /// `is_nested_top` marks a directory beneath the root that is the top of its own git work
    /// tree ([`holds_git`]).
    ///
    /// Effect: reads file metadata in `dir`, its ignore files and its git exclude file.
    fn inside(
        dir: AbsolutePath,
        parent: Option<Arc<Self>>,
        is_nested_top: bool,
        limit: SizeLimit,
    ) -> Self {
        let own = own_ignore_files(dir.as_path(), limit);
        let exclude = git_exclude(dir.as_path(), limit);
        let exclude_origin = if exclude.quotable {
            Origin::Inside
        } else {
            Origin::Exclude
        };
        let unreadable = unreadable_in(&dir, &own, &exclude, Origin::Inside, exclude_origin);
        Self {
            dir,
            ignore: own.ignore,
            gitignore: own.gitignore,
            exclude: exclude.rules,
            place: Place::Inside {
                quote_exclude: exclude.quotable,
            },
            symlinked: own.symlinked,
            unreadable,
            is_nested_top,
            parent,
        }
    }

    /// The rules of `dir`, a directory above the root and at most the top of its git work tree.
    /// It lies inside that work tree, so a `.ignore` or `.gitignore` there that is a symlink is not
    /// read either, and unlike inside the root it is not reported: git warns of it already.
    ///
    /// Effect: reads file metadata in `dir`, its ignore files and its git exclude file.
    fn above(dir: AbsolutePath, parent: Option<Arc<Self>>, limit: SizeLimit) -> Self {
        let own = own_ignore_files(dir.as_path(), limit);
        let exclude = git_exclude(dir.as_path(), limit);
        let unreadable = unreadable_in(&dir, &own, &exclude, Origin::Above, Origin::Above);
        Self {
            dir,
            ignore: own.ignore,
            gitignore: own.gitignore,
            exclude: exclude.rules,
            place: Place::Above,
            symlinked: Vec::new(),
            unreadable,
            is_nested_top: false,
            parent,
        }
    }
}

/// The ignore files of `dir` that exist but couldn't be read: its own, at `files`, and its exclude
/// file, at `exclude_origin`.
fn unreadable_in(
    dir: &AbsolutePath,
    own: &OwnIgnoreFiles,
    exclude: &ExcludeFile,
    files: Origin,
    exclude_origin: Origin,
) -> Vec<UnreadableFile> {
    let own = own
        .unreadable
        .iter()
        .map(|&(file, failure)| UnreadableFile {
            path: dir.join_path(Path::new(file.path())),
            origin: files,
            failure,
        });
    let exclude = exclude
        .unreadable
        .iter()
        .filter_map(|(path, failure)| Some((AbsolutePath::new(path)?, *failure)))
        .map(|(path, failure)| UnreadableFile {
            path,
            origin: exclude_origin,
            failure,
        });
    own.chain(exclude).collect()
}

/// The rules of a directory's `.ignore` and `.gitignore`, and which of them weren't read.
struct OwnIgnoreFiles {
    /// The rules of its `.ignore`.
    ignore: Gitignore,
    /// The rules of its `.gitignore`.
    gitignore: Gitignore,
    /// Those that are symlinks, which were not read.
    symlinked: Vec<IgnoreFile>,
    /// Those that exist but couldn't be read in full, and how reading failed.
    unreadable: Vec<(IgnoreFile, ReadFailure)>,
}

/// The rules of a directory's git exclude file, whether they may be quoted, and the file when it
/// exists but couldn't be read.
struct ExcludeFile {
    /// Its rules, none when there is no exclude file.
    rules: Gitignore,
    /// Whether its rules may be quoted ([`git_exclude`]).
    quotable: bool,
    /// The file and how reading it failed, when it exists but couldn't be read in full.
    unreadable: Option<(PathBuf, ReadFailure)>,
}

impl ExcludeFile {
    /// No exclude file applies.
    fn none() -> Self {
        Self {
            rules: Gitignore::empty(),
            quotable: false,
            unreadable: None,
        }
    }
}

/// The rules of `dir`'s `.ignore` and `.gitignore`, which of the two are symlinks, and which
/// exist but couldn't be read. A symlink is not read, wherever it points (04 S15): only an `lstat`
/// looks at it.
///
/// Effect: reads file metadata in `dir` and its ignore files.
fn own_ignore_files(dir: &Path, limit: SizeLimit) -> OwnIgnoreFiles {
    let mut symlinked = Vec::new();
    let mut unreadable = Vec::new();
    let mut own = |file: IgnoreFile| {
        let path = dir.join(file.path());
        match fs::symlink_metadata(&path) {
            Err(_) => Gitignore::empty(),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                symlinked.push(file);
                Gitignore::empty()
            }
            Ok(_) => {
                let (rules, failure) = rules_in(dir, &path, limit);
                unreadable.extend(failure.map(|failure| (file, failure)));
                rules
            }
        }
    };
    let ignore = own(IgnoreFile::Ignore);
    let gitignore = own(IgnoreFile::Gitignore);
    OwnIgnoreFiles {
        ignore,
        gitignore,
        symlinked,
        unreadable,
    }
}

/// The rules of `file`, which apply beneath `dir`, and how reading it failed, if it exists but
/// couldn't be read in full. A missing file has no rules, one that can't be opened none, one whose
/// reading stops partway the rules before that point, and a line that isn't a valid pattern is
/// left out, as git and the walker treat them. A file larger than `limit` is not read at all: it
/// has no rules and fails as [`ReadFailure::TooLarge`], which fails the run rather than widening
/// the scan unnoticed.
///
/// Effect: reads `file`'s metadata and then `file`, following symlinks.
fn rules_in(dir: &Path, file: &Path, limit: SizeLimit) -> (Gitignore, Option<ReadFailure>) {
    if let Some((size, limit)) = limit.get().and_then(|limit| {
        let size = fs::metadata(file).ok()?.len();
        (size > limit).then_some((size, limit))
    }) {
        return (
            Gitignore::empty(),
            Some(ReadFailure::TooLarge { size, limit }),
        );
    }
    let mut builder = GitignoreBuilder::new(dir);
    let failure = builder.add(file).and_then(|error| read_failure(&error));
    let rules = builder.build().unwrap_or_else(|_| Gitignore::empty());
    (rules, failure)
}

/// How `error`, from reading an ignore file, says reading failed: denied permission, or any other
/// failure partway. `None` when the file is missing or only a line isn't a valid pattern.
fn read_failure(error: &ignore::Error) -> Option<ReadFailure> {
    match error {
        ignore::Error::Io(error) => match error.kind() {
            io::ErrorKind::NotFound => None,
            io::ErrorKind::PermissionDenied => Some(ReadFailure::Denied),
            _ => Some(ReadFailure::Partial),
        },
        ignore::Error::Partial(errors) => errors.iter().filter_map(read_failure).min(),
        ignore::Error::WithPath { err, .. }
        | ignore::Error::WithDepth { err, .. }
        | ignore::Error::WithLineNumber { err, .. } => read_failure(err),
        _ => None,
    }
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
fn git_exclude(dir: &Path, limit: SizeLimit) -> ExcludeFile {
    let dot_git = dir.join(".git");
    let Ok(metadata) = fs::metadata(&dot_git) else {
        return ExcludeFile::none();
    };
    let git_dir = if metadata.is_file() {
        let Some(common) = common_dir(dir, &dot_git) else {
            return ExcludeFile::none();
        };
        common
    } else if is_git_dir(&dot_git) {
        dot_git.clone()
    } else {
        return ExcludeFile::none();
    };
    let exclude = git_dir.join("info").join("exclude");
    let quotable = is_real_dir(&dot_git)
        && is_real_dir(&dot_git.join("info"))
        && fs::symlink_metadata(&exclude).is_ok_and(|metadata| metadata.is_file());
    let (rules, failure) = rules_in(dir, &exclude, limit);
    ExcludeFile {
        rules,
        quotable,
        unreadable: failure.map(|failure| (exclude, failure)),
    }
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

/// The warning for `file`, an ignore file that exists but couldn't be read in full, so some or
/// all of its rules don't apply and the files they would hide are checked. It names the file as
/// [`Origin::name`] does, never a line. Only a file inside the root whose read permission was
/// denied gets the `chmod` that fixes it.
fn unreadable_ignore_file(root: &AbsolutePath, file: &UnreadableFile) -> Finding {
    let named = file.origin.name(root, Some(file.path.as_path()));
    if let ReadFailure::TooLarge { size, limit } = file.failure {
        return too_large_ignore_file(&named, size, limit);
    }
    let message = match file.failure {
        ReadFailure::Denied => format!("{named} can't be read, so its rules don't apply"),
        ReadFailure::Partial | ReadFailure::TooLarge { .. } => {
            format!("{named} could not be fully read, so some of its rules may not apply")
        }
    };
    let finding = Finding::new("scan-ignore-file-unreadable", Severity::Warning, message);
    match (file.failure, file.origin) {
        (ReadFailure::Denied, Origin::Inside) => finding.with_help(format!(
            "The files its rules would hide were checked. Run `chmod u+r {}` so they're skipped, \
             or add them to `exclude` in `{CONFIG_FILE_NAME}`",
            shell_quote(&relative_to(root, &file.path)),
        )),
        (ReadFailure::Denied, Origin::Exclude | Origin::Above | Origin::Global) => finding
            .with_help(format!(
                "The files its rules would hide were checked. Make it readable, or add those \
                 files to `exclude` in `{CONFIG_FILE_NAME}`"
            )),
        (ReadFailure::Partial | ReadFailure::TooLarge { .. }, _) => finding.with_help(format!(
            "The files its later rules would hide may have been checked. Make it a regular UTF-8 \
             text file, or add those files to `exclude` in `{CONFIG_FILE_NAME}`"
        )),
    }
}

/// The error for `named`, an ignore file of `size` bytes over the `limit` (`max-ignore-size`). It
/// was not read, so the files its rules would hide would be checked: the run fails instead of
/// quietly widening the scan. The help names the key, variable and flag that raise the limit, with
/// the next power of two above the measured size.
fn too_large_ignore_file(named: &str, size: u64, limit: u64) -> Finding {
    let raised = size_text(size.saturating_add(1).next_power_of_two());
    Finding::new(
        "ignore-too-large",
        Severity::Error,
        format!(
            "{named} is {} (limit {}), so it was not read",
            size_text(size),
            size_text(limit)
        ),
    )
    .with_help(format!(
        "Real ignore files are a few KiB, and each pattern costs memory. If this file is real, \
         raise the limit with `[limits] max-ignore-size = \"{raised}\"` in {CONFIG_FILE_NAME}, \
         `SUPDOC_MAX_IGNORE_SIZE={raised}` or `--max-ignore-size {raised}`, or add its directory \
         to `exclude`. `0` means no limit"
    ))
}

/// `bytes` in the largest unit, `B`, `KiB` or `MiB`, that divides it; a size with a remainder is
/// shown in `KiB` or `MiB` to one decimal place, rounded down, as `292.9KiB`.
fn size_text(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    let (unit, scale) = match bytes {
        b if b >= MIB => ("MiB", MIB),
        b if b >= KIB => ("KiB", KIB),
        _ => return format!("{bytes}B"),
    };
    if bytes.is_multiple_of(scale) {
        format!("{}{unit}", bytes / scale)
    } else {
        let tenths = bytes.saturating_mul(10) / scale;
        format!("{}.{}{unit}", tenths / 10, tenths % 10)
    }
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

/// `entry` as a source, `None` when a rule skips it, or a `scan-invalid-source-path` warning when
/// its name can't be a source path, so the skipped file is never silent.
fn as_source(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    entry: &DirEntry,
) -> Option<Result<(AbsolutePath, &'static dyn Cartridge), Finding>> {
    // A symlink reports its own type, not its target's, so links are skipped here (04 S3).
    if !entry.file_type()?.is_file() {
        return None;
    }
    let path = AbsolutePath::new(entry.path())?;
    if !config.scans(&path) {
        return None;
    }
    let cartridge = languages.for_path(path.as_path())?;
    let relative_path = path.strip_prefix(config.root())?;
    let relative = match SourcePath::from_relative(relative_path) {
        Ok(relative) => relative,
        Err(error) => return Some(Err(invalid_source_path(relative_path, &error))),
    };
    scope.contains(&relative).then_some(Ok((path, cartridge)))
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

/// The warning for one explicitly named path, or `None` when it was walked or is missing. A path
/// in a version-control or nested-project directory, or of a type no cartridge reads, gets a
/// `scan-explicit-path-unscanned` warning. A path that is a symlink, or lies under a symlinked directory, is never walked, so it always
/// gets a warning.
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
    if let Some(skipped) = skipped_dir(root, listed) {
        return Some(unscanned(listed, &Unscanned::Dir(skipped)));
    }
    if kind.is_symlink() {
        return Some(symlinked(root, listed, &path, Symlink::Itself));
    }
    if !kind.is_file() && !kind.is_dir() {
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
        return Some(excluded(
            listed,
            Some(pattern),
            &defined_in(root, config.exclude_definition()),
        ));
    }
    if is_source_file && !config.scans(&path) {
        return Some(excluded(
            listed,
            None,
            &defined_in(root, config.include_definition()),
        ));
    }
    // The walk never enters a symlinked directory, so nothing under one is walked and no rule
    // names it.
    if has_symlinked_parent(root, listed) {
        return Some(symlinked(root, listed, &path, Symlink::Parent));
    }
    // The walk skipped it, but asking each rule again found none that does: say so, rather than
    // blame a rule that may not exist.
    if is_source_file && !walked {
        return Some(unscanned(
            listed,
            &Unscanned::Unreached {
                in_work_tree: rules.in_work_tree,
            },
        ));
    }
    (kind.is_file() && !is_source_file).then(|| unscanned(listed, &Unscanned::UnreadType))
}

/// Why the walk never reads an explicitly named path.
enum Unscanned {
    /// A directory on the way is skipped whatever the user names.
    Dir(SkippedDir),
    /// No cartridge reads files of this type.
    UnreadType,
    /// The walk didn't reach it, and no rule found explains why, as when it is named in a case
    /// other than its own on a case-insensitive file system. In a git work tree,
    /// `git check-ignore` may name a rule supdoc missed.
    Unreached {
        /// Whether the root lies in a git work tree.
        in_work_tree: bool,
    },
}

/// A directory the walk never enters, whatever the user names.
enum SkippedDir {
    /// A version-control directory, at this project-relative path.
    Vcs(String),
    /// A separate project with its own config, at this project-relative path.
    Project(String),
}

/// The finding for an explicit path the walk never reads for a reason that isn't a user setting.
fn unscanned(listed: &SourcePath, why: &Unscanned) -> Finding {
    let listed = listed.as_str();
    let (reason, help) = match why {
        Unscanned::Dir(SkippedDir::Vcs(dir)) => (
            format!("is inside `{dir}`, a version-control directory supdoc never scans"),
            "Pass a path outside it, or stop passing this one".to_owned(),
        ),
        Unscanned::Dir(SkippedDir::Project(dir)) => (
            format!("is inside `{dir}`, a separate project with its own {CONFIG_FILE_NAME}"),
            format!("Run `supdoc check` inside `{dir}`, or stop passing this path"),
        ),
        Unscanned::UnreadType => (
            "has a type no language supdoc reads".to_owned(),
            "Pass a `.md` or `.rs` file instead, or stop passing this one".to_owned(),
        ),
        Unscanned::Unreached { in_work_tree } => (
            "the walk didn't reach it, and supdoc found no rule that skips it".to_owned(),
            if *in_work_tree {
                format!(
                    "Check that `{listed}` matches its name on disk, case included. `git \
                     check-ignore -v -- {}` may show a rule that skips it",
                    shell_quote(listed),
                )
            } else {
                format!(
                    "Check that `{listed}` matches its name on disk, case included, and can be \
                     read, or stop passing it"
                )
            },
        ),
    };
    Finding::new(
        "scan-explicit-path-unscanned",
        Severity::Warning,
        format!("{listed} was named on the command line but {reason}"),
    )
    .with_help(help)
}

/// Whether a directory above `listed`, inside `root`, is a symlink.
fn has_symlinked_parent(root: &AbsolutePath, listed: &SourcePath) -> bool {
    let mut current = root.as_path().to_path_buf();
    let mut segments = listed.as_str().split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            break;
        }
        current.push(segment);
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.is_symlink()) {
            return true;
        }
    }
    false
}

/// Where the symlink sits that keeps the walk from an explicitly named path.
#[derive(Clone, Copy)]
enum Symlink {
    /// The named path is itself a symlink, which the walk doesn't follow.
    Itself,
    /// A directory above the named path is a symlink, which the walk doesn't enter.
    Parent,
}

/// The finding for an explicit path that is a symlink or is reached through a symlinked
/// directory, neither of which the walk follows. It suggests the real path only when that lies
/// inside the project.
fn symlinked(
    root: &AbsolutePath,
    listed: &SourcePath,
    path: &AbsolutePath,
    symlink: Symlink,
) -> Finding {
    let listed = listed.as_str();
    let message = match symlink {
        Symlink::Itself => format!(
            "{listed} was named on the command line but is a symlink, which supdoc doesn't follow",
        ),
        Symlink::Parent => format!(
            "{listed} was named on the command line but is reached through a symlinked directory, which supdoc doesn't enter",
        ),
    };
    let finding = Finding::new("scan-explicit-path-symlinked", Severity::Warning, message);
    let real = fs::canonicalize(path.as_path()).ok().and_then(|real| {
        let real_root = fs::canonicalize(root.as_path()).ok()?;
        let relative = real.strip_prefix(real_root).ok()?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        (!relative.is_empty()).then_some(relative)
    });
    match real {
        Some(real) => finding.with_help(format!("Pass the real path `{real}` instead")),
        None => finding.with_help("Pass the real path inside the project instead"),
    }
}

/// The config file that set a pattern key, relative to the project `root`, or the default config
/// name when no file sets it.
fn defined_in(root: &AbsolutePath, definition: Option<PatternDefinition<'_>>) -> String {
    definition.map_or_else(
        || CONFIG_FILE_NAME.to_owned(),
        |definition| relative_to(root, definition.file()),
    )
}

/// The finding for an explicit path that the config excludes, naming the `exclude` pattern when
/// one matched and otherwise blaming `include`, each in the config `file` that set it.
fn excluded(listed: &SourcePath, pattern: Option<&str>, file: &str) -> Finding {
    let listed = listed.as_str();
    match pattern {
        Some(pattern) => Finding::new(
            "scan-explicit-path-excluded",
            Severity::Warning,
            format!(
                "{listed} was named on the command line but is excluded by `{pattern}` in `exclude` in {file}",
            ),
        )
        .with_help(format!(
            "Remove `{pattern}` from `exclude` in {file} to check it, or stop passing it"
        )),
        None => Finding::new(
            "scan-explicit-path-excluded",
            Severity::Warning,
            format!(
                "{listed} was named on the command line but is not matched by `include` in {file}",
            ),
        )
        .with_help(format!(
            "Add `{listed}` to `include` in {file} to check it, or stop passing it"
        )),
    }
}

/// The finding for an explicit path that an ignore file skips, naming the rule and its file as
/// far as `by` may, and in a git work tree offering `git check-ignore` to show the rule.
fn ignored(listed: &SourcePath, by: &IgnoredBy) -> Finding {
    let listed = listed.as_str();
    let show_rule = if by.check_ignore {
        format!(
            ". `git check-ignore -v -- {}` shows the rule",
            shell_quote(listed)
        )
    } else {
        String::new()
    };
    let help = format!(
        "Remove that rule from {file} to check `{listed}`, or stop passing it{show_rule}",
        file = by.file,
    );
    Finding::new(
        "scan-explicit-path-ignored",
        Severity::Warning,
        format!(
            "{listed} was named on the command line but is ignored by {rule}",
            rule = by.rule,
        ),
    )
    .with_help(help)
}

/// The version-control directory or nested project that `listed` or one of its parent directories
/// is, which the walk skips whatever the user names (04 S2, S4). A symlink is never judged here:
/// the walk doesn't follow it, so it gets the symlink warning.
fn skipped_dir(root: &AbsolutePath, listed: &SourcePath) -> Option<SkippedDir> {
    let mut current = root.as_path().to_path_buf();
    let mut relative = String::new();
    for segment in listed.as_str().split('/') {
        current.push(segment);
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(segment);
        if is_vcs_dir(&current) {
            return Some(SkippedDir::Vcs(relative));
        }
        let is_real_dir = fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.is_dir());
        if is_real_dir && is_nested_project(&current) {
            return Some(SkippedDir::Project(relative));
        }
    }
    None
}

/// Describes the ignore rule that keeps the walk from reaching `path`, or `None` when none does.
///
/// It asks what the walk asked, with the same [`IgnoreRules`] and in the order the walk would:
/// each directory on the way down from the root, then `path` itself. The walk never enters a
/// symlinked directory, so nothing beneath one is judged. A rule is quoted with its file only when
/// that file lies inside the root and is no symlink; any other is blamed on "an ignore file", and
/// its file named by kind and, unless above the root, by path, so no line from outside the root
/// is ever printed (04 S15).
///
/// Effect: reads ignore files and file metadata beneath the root.
fn ignore_rule(
    rules: &IgnoreRules,
    root: &AbsolutePath,
    path: &AbsolutePath,
    is_dir: bool,
) -> Option<IgnoredBy> {
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
            return Some(rule.describe(root, rules.in_work_tree));
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
            walk(
                &config,
                Languages::builtin().expect("the built-in cartridges should load"),
                scope,
            )
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
        );
        let names = walked
            .sources
            .iter()
            .filter_map(|(path, _)| path.as_path().file_name())
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
        );
        let one = walk(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &paths(&["out.rs"]),
        );

        let walked: Vec<_> = all
            .sources
            .iter()
            .filter_map(|(path, _)| path.as_path().file_name())
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
                .map(|(path, _)| project.relative(path))
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
