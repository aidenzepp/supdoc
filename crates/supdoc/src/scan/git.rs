//! Finding a directory's git exclude file, however `.git` is laid out (04 S15).

use std::fs;
use std::io::{self, BufRead as _};
use std::path::{Path, PathBuf};

use ignore::gitignore::Gitignore;

use crate::limits::SizeLimit;
use crate::path::is_git_dir;
use crate::scan::ignore::{ReadFailure, rules_in};

/// The rules of a directory's git exclude file, whether they may be quoted, and the file when it
/// exists but couldn't be read.
pub(super) struct ExcludeFile {
    /// Its rules, none when there is no exclude file.
    pub(super) rules: Gitignore,
    /// Whether its rules may be quoted ([`git_exclude`]).
    pub(super) quotable: bool,
    /// The file and how reading it failed, when it exists but couldn't be read in full.
    pub(super) unreadable: Option<(PathBuf, ReadFailure)>,
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
pub(super) fn git_exclude(dir: &Path, limit: SizeLimit) -> ExcludeFile {
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
pub(super) fn common_dir(dir: &Path, dot_git: &Path) -> Option<PathBuf> {
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
pub(super) fn first_line(file: &Path) -> Option<String> {
    let file = fs::File::open(file).ok()?;
    io::BufReader::new(file).lines().next()?.ok()
}

/// Whether `path` is a directory itself, not a symlink to one.
///
/// Effect: reads `path`'s metadata.
pub(super) fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}
