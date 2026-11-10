//! The ignore engine: which `.ignore`, `.gitignore` and exclude-file rules decide a path.

use crate::remedies::{self, Access};
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder, Glob};

use crate::config::{CONFIG_FILE_NAME, size_text};
use crate::finding::{Finding, Severity};
use crate::limits::SizeLimit;
use crate::lock::error::shell_quote;
use crate::output::paths::relative_to;
use crate::path::{AbsolutePath, FixedPath, beneath, fixed, holds_git, rebase};
use crate::scan::git::{ExcludeFile, git_exclude};
use crate::scan::walk::GlobalExcludes;

/// An ignore file a directory can hold that a checkout can plant. One that is a symlink is not
/// read, wherever it points, as git doesn't read one in its work tree: inside the project
/// root, and in the directories above it, which lie inside the work tree too. Git's exclude files,
/// reached through `.git`, are not among them: git refuses to check out any `.git` path, so they
/// are followed as git follows them, symlinks included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IgnoreFile {
    /// `.ignore`.
    Ignore,
    /// `.gitignore`.
    Gitignore,
}

impl IgnoreFile {
    /// Its path, relative to the directory it applies to.
    const fn path(self) -> FixedPath {
        match self {
            Self::Ignore => fixed::DOT_IGNORE,
            Self::Gitignore => fixed::DOT_GITIGNORE,
        }
    }
}

/// The ignore rules the walk applies and the explicit-path warnings quote from, so the rule a
/// warning names is the rule that decided.
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
/// ignores it all. The index isn't read either, so a rule ignores a file git tracks, which git
/// never does.
///
/// A `.ignore` or `.gitignore` that is a symlink is not read, whatever it points to, inside the
/// root or above it, as every directory above lies inside the git work tree. Only inside the root
/// is such a symlink reported, and no rule line from above the root is ever quoted.
pub(super) struct IgnoreRules {
    /// The project root, as the walk reaches it.
    root: AbsolutePath,
    /// The root with every symlink resolved, which the directories above it are found from and
    /// matched against. `None` when it can't be resolved, and then no directory above applies.
    canonical_root: Option<PathBuf>,
    /// The rules of the directory just above the root, linked to those above it up to the top of
    /// the git work tree. `None` when no directory above applies.
    above: Option<Arc<Level>>,
    /// The rules of the global excludes file, such as `~/.config/git/ignore`.
    global: Gitignore,
    /// The global excludes file, when it exists but couldn't be read in full.
    global_unreadable: Option<UnreadableFile>,
    /// Whether the root lies in a git work tree, where `git check-ignore` can name a rule.
    pub(super) in_work_tree: bool,
    /// The rules of the root and of each directory beneath it read so far, by path.
    inside: Mutex<HashMap<PathBuf, Arc<Level>>>,
    /// How large each ignore file may be. A larger one is not read and becomes an error.
    limit: SizeLimit,
}

/// One directory's ignore rules, linked to those of the directory above it.
pub(super) struct Level {
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
pub(super) enum Place {
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
pub(super) struct Rule<'a> {
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
pub(super) enum Origin {
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
    /// contents.
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
pub(super) struct IgnoredBy {
    /// The rule for the message: its line and file when it may be quoted, and "an ignore file"
    /// otherwise, so no line from outside the root is ever printed.
    pub(super) rule: String,
    /// The file holding the rule for the help, by kind and path ([`Origin::name`]).
    pub(super) file: String,
    /// Whether `git check-ignore` can show the rule: the root lies in a git work tree and the
    /// rule comes from a file git reads, never a `.ignore`.
    pub(super) check_ignore: bool,
}

impl Rule<'_> {
    /// How a warning names this rule and its file, relative to `root`.
    pub(super) fn describe(self, root: &AbsolutePath, in_work_tree: bool) -> IgnoredBy {
        let from = self.glob.from();
        let rule = match (self.origin, from) {
            (Origin::Inside, Some(file)) => format!(
                "`{}` in {}",
                self.glob.original(),
                AbsolutePath::new(file).map_or_else(
                    || file.display().to_string(),
                    |file| relative_to(root, &file)
                )
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
    /// the top of its git work tree, and the global `excludes` file now; the root's own, and those
    /// beneath it, as the walk asks. Every one of them is read under `limit`.
    ///
    /// Effect: resolves `root`'s symlinks, reads file metadata in it and the directories above
    /// it, and reads ignore files and git files.
    pub(super) fn new(root: &AbsolutePath, excludes: &GlobalExcludes, limit: SizeLimit) -> Self {
        let canonical_root = fs::canonicalize(root.as_path()).ok();
        let top = canonical_root
            .as_deref()
            .and_then(|canonical| canonical.ancestors().position(holds_git));
        let above = canonical_root
            .as_deref()
            .zip(top)
            .and_then(|(canonical, top)| {
                // Every ancestor of the canonical root is absolute already.
                let parents: Vec<AbsolutePath> = canonical
                    .ancestors()
                    .take(top + 1)
                    .skip(1)
                    .filter_map(AbsolutePath::new)
                    .collect();
                parents.into_iter().rev().fold(None, |parent, dir| {
                    Some(Arc::new(Level::above(dir, parent, limit)))
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
        let (global, global_unreadable) = excludes.file.as_ref().map_or_else(
            || (Gitignore::empty(), None),
            |file| {
                let (rules, failure) = rules_in(anchor, file, limit);
                // Normalized only to name it in a warning; the read above used the path as is.
                let unreadable =
                    failure
                        .zip(AbsolutePath::new(file))
                        .map(|(failure, path)| UnreadableFile {
                            path,
                            origin: Origin::Global,
                            failure,
                        });
                (rules, unreadable)
            },
        );
        Self {
            root: root.clone(),
            canonical_root,
            above,
            global,
            global_unreadable,
            in_work_tree: top.is_some(),
            inside: Mutex::new(HashMap::new()),
            limit,
        }
    }

    /// Whether the walk skips `path`, an entry of the root or of a directory beneath it.
    ///
    /// Effect: reads the ignore files of `path`'s directory and those above it, once each.
    pub(super) fn ignores(&self, path: &Path, is_dir: bool) -> bool {
        let dir = path.parent().and_then(|dir| self.level(dir));
        self.matched(dir.as_deref(), path, is_dir).is_ignore()
    }

    /// The rules `dir` and the directories above it hold, read once and kept. A directory outside
    /// the root gets only those above the root.
    ///
    /// Effect: reads the ignore files and file metadata of `dir` and its parents within the root,
    /// the first time each is asked for.
    pub(super) fn level(&self, dir: &Path) -> Option<Arc<Level>> {
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
        // `dir` lies beneath the absolute root, so it is absolute already.
        let level = Arc::new(Level::inside(
            AbsolutePath::new(dir)?,
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
    pub(super) fn matched<'a>(
        &'a self,
        level: Option<&'a Level>,
        path: &Path,
        is_dir: bool,
    ) -> Match<Rule<'a>> {
        let resolved = self
            .canonical_root
            .as_deref()
            .and_then(|canonical| rebase(path, self.root.as_path(), canonical));
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
    pub(super) fn symlinked_files(&self) -> Vec<AbsolutePath> {
        self.read_so_far()
            .values()
            .flat_map(|level| {
                level
                    .symlinked
                    .iter()
                    .map(|file| level.dir.beneath(&file.path()))
            })
            .collect()
    }

    /// Every ignore file the walk met that exists but couldn't be read: the global excludes file
    /// and those above the root, all read up front, and those of the directories inside it read so
    /// far.
    pub(super) fn unreadable_files(&self) -> Vec<UnreadableFile> {
        let mut files: Vec<UnreadableFile> = self.global_unreadable.iter().cloned().collect();
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
pub(super) fn rule<'a>(
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
pub(super) struct UnreadableFile {
    /// The file.
    path: AbsolutePath,
    /// Where it lies, which decides how a warning names it.
    origin: Origin,
    /// How reading it failed.
    failure: ReadFailure,
}

/// How reading an ignore file that exists failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ReadFailure {
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
    /// that is a symlink is not read, wherever it points: only an `lstat` looks at it.
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
pub(super) fn unreadable_in(
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
            path: dir.beneath(&file.path()),
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
pub(super) struct OwnIgnoreFiles {
    /// The rules of its `.ignore`.
    ignore: Gitignore,
    /// The rules of its `.gitignore`.
    gitignore: Gitignore,
    /// Those that are symlinks, which were not read.
    symlinked: Vec<IgnoreFile>,
    /// Those that exist but couldn't be read in full, and how reading failed.
    unreadable: Vec<(IgnoreFile, ReadFailure)>,
}

/// The rules of `dir`'s `.ignore` and `.gitignore`, which of the two are symlinks, and which
/// exist but couldn't be read. A symlink is not read, wherever it points: only an `lstat`
/// looks at it.
///
/// Effect: reads file metadata in `dir` and its ignore files.
pub(super) fn own_ignore_files(dir: &Path, limit: SizeLimit) -> OwnIgnoreFiles {
    let mut symlinked = Vec::new();
    let mut unreadable = Vec::new();
    let mut own = |file: IgnoreFile| {
        let path = beneath(dir, &file.path());
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
/// left out, as git and the walker treat them. A leading UTF-8 byte order mark is dropped, as git
/// drops it.
///
/// A file larger than `limit` has no rules and fails as [`ReadFailure::TooLarge`], which fails the
/// run rather than widening the scan unnoticed. The size is checked on the open handle and the read
/// stops one byte past the limit on that same handle, so a file that grows after the check is
/// still refused.
///
/// Effect: reads `file`, following symlinks.
pub(super) fn rules_in(
    dir: &Path,
    file: &Path,
    limit: SizeLimit,
) -> (Gitignore, Option<ReadFailure>) {
    let (bytes, mut failure) = match read_capped(file, limit) {
        Ok(read) => read,
        Err(failure) => return (Gitignore::empty(), failure),
    };
    let mut builder = GitignoreBuilder::new(dir);
    for (index, line) in bytes.lines().enumerate() {
        let Ok(line) = line else {
            failure = Some(ReadFailure::Partial);
            break;
        };
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            &line
        };
        let _invalid_pattern = builder.add_line(Some(file.to_path_buf()), line);
    }
    let rules = builder.build().unwrap_or_else(|_| Gitignore::empty());
    (rules, failure)
}

/// The bytes of `file`, at most `limit` of them, and [`ReadFailure::Partial`] when reading stopped
/// partway, with the bytes read before that point.
///
/// # Errors
///
/// `None` when `file` is missing, [`ReadFailure::Denied`] when it can't be opened for lack of
/// permission, [`ReadFailure::TooLarge`] when it holds more than `limit` bytes, and
/// [`ReadFailure::Partial`] when it can't be opened for any other reason.
pub(super) fn read_capped(
    file: &Path,
    limit: SizeLimit,
) -> Result<(Vec<u8>, Option<ReadFailure>), Option<ReadFailure>> {
    let handle = fs::File::open(file).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => None,
        io::ErrorKind::PermissionDenied => Some(ReadFailure::Denied),
        _ => Some(ReadFailure::Partial),
    })?;
    let too_large = |size, limit| Some(ReadFailure::TooLarge { size, limit });
    let mut bytes = Vec::new();
    let read = match limit.get() {
        Some(limit) => {
            let size = handle.metadata().map_or(0, |metadata| metadata.len());
            if size > limit {
                return Err(too_large(size, limit));
            }
            let read = handle.take(limit.saturating_add(1)).read_to_end(&mut bytes);
            let held = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if held > limit {
                return Err(too_large(held, limit));
            }
            read
        }
        None => (&handle).read_to_end(&mut bytes),
    };
    if read.is_err() {
        // A read that stops partway may stop inside a line: drop that cut-off line, as the
        // ignore crate's line reader does, so it never becomes a shorter, wider rule.
        let whole = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |end| end + 1);
        bytes.truncate(whole);
    }
    Ok((bytes, read.err().map(|_| ReadFailure::Partial)))
}

/// The warning for `file`, an ignore file inside the root that is a symlink and so wasn't read.
/// The message names it relative to `root`, as the other walk findings name paths.
pub(super) fn symlinked_ignore_file(root: &AbsolutePath, file: &AbsolutePath) -> Finding {
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
pub(super) fn unreadable_ignore_file(root: &AbsolutePath, file: &UnreadableFile) -> Finding {
    let named = file.origin.name(root, Some(file.path.as_path()));
    if let ReadFailure::TooLarge { size, limit } = file.failure {
        return too_large_ignore_file(&named, size, limit, is_nested(root, file));
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
            "The files its rules would hide were checked. Run `{}` so they're skipped, \
             or add them to `exclude` in `{CONFIG_FILE_NAME}`",
            remedies::grant_on(
                Access::Read,
                &shell_quote(&relative_to(root, &file.path)),
                remedies::WINDOWS
            ),
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

/// The code for an ignore file over `max-ignore-size`, which the scan turns into a fatal error.
pub(crate) const CODE_IGNORE_TOO_LARGE: &str = "ignore-too-large";

/// The error for `named`, an ignore file of `size` bytes over the `limit` (`max-ignore-size`). It
/// was not read, so the files its rules would hide would be checked: the scan stops instead of
/// quietly widening. The help names the key, variable and flag that raise the limit, with the next
/// power of two above the measured size. Only a `nested` file, one beneath the root, can be left
/// out by excluding its directory; any other is the root's own, git's or above the root.
pub(super) fn too_large_ignore_file(named: &str, size: u64, limit: u64, nested: bool) -> Finding {
    let raised = size_text(size.saturating_add(1).next_power_of_two());
    let raise = format!(
        "raise the limit with `--max-ignore-size {raised}` or `SUPDOC_MAX_IGNORE_SIZE={raised}` \
         (`[limits] max-ignore-size = \"{raised}\"` in {CONFIG_FILE_NAME} raises it only in a \
         checkout you trust)"
    );
    let next = if nested {
        format!("{raise}, or add its directory to `exclude`")
    } else {
        format!("{raise}, or shorten the file")
    };
    Finding::new(
        CODE_IGNORE_TOO_LARGE,
        Severity::Error,
        format!(
            "{named} is {} (limit {}), so it was not read",
            size_text(size),
            size_text(limit)
        ),
    )
    .with_help(format!(
        "Real ignore files are a few KiB, and each pattern costs memory. If this file is real, \
         {next}. `0` means no limit"
    ))
}

/// Whether `file` is a `.ignore` or `.gitignore` in a directory beneath the root, which an
/// `exclude` of that directory leaves out. The root's own, a git exclude file and any file above
/// the root are not.
pub(super) fn is_nested(root: &AbsolutePath, file: &UnreadableFile) -> bool {
    let is_own_file = file
        .path
        .as_path()
        .file_name()
        .is_some_and(|name| name == ".ignore" || name == ".gitignore");
    file.origin == Origin::Inside
        && is_own_file
        && file
            .path
            .parent()
            .is_some_and(|dir| dir != *root && dir.as_path().starts_with(root.as_path()))
}
