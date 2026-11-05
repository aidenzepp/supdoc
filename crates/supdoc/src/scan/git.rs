//! Finding a directory's git exclude file, however `.git` is laid out.

use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use ignore::gitignore::Gitignore;

use crate::limits::SizeLimit;
use crate::path::is_git_dir;
use crate::read::source::open_without_blocking;
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
/// check out any `.git` path.
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
/// no `gitdir: ` line, or a `commondir` file is there but unreadable, empty or not a regular file,
/// so no exclude file applies.
///
/// Effect: reads `dot_git` and the `commondir` file.
pub(crate) fn common_dir(dir: &Path, dot_git: &Path) -> Option<PathBuf> {
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

/// The most bytes a `.git` file's or `commondir`'s first line may hold before its line ending:
/// `PATH_MAX` on Linux, so no real git link is longer.
const FIRST_LINE_MAX: u64 = 4096;

/// The first line of `file`, without its line ending, or `None` when it can't be read, isn't a
/// regular file once symlinks are followed, is empty, or runs past [`FIRST_LINE_MAX`] bytes with
/// no line ending. The open never waits on a FIFO, and the read never takes more than one byte
/// past the cap, so no file there can block or flood it.
///
/// Effect: opens `file` and, for a regular file, reads at most `FIRST_LINE_MAX + 1` bytes of it.
pub(crate) fn first_line(file: &Path) -> Option<String> {
    let file = open_without_blocking(file).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut head = Vec::new();
    let read = file.take(FIRST_LINE_MAX + 1).read_to_end(&mut head).ok()?;
    if read == 0 {
        return None;
    }
    if !head.contains(&b'\n') && u64::try_from(read).unwrap_or(u64::MAX) > FIRST_LINE_MAX {
        return None;
    }
    let line = head.split(|&byte| byte == b'\n').next()?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8(line.to_vec()).ok()
}

/// Whether `path` is a directory itself, not a symlink to one.
///
/// Effect: reads `path`'s metadata.
pub(super) fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    /// A temporary directory for one test.
    fn temp() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("a temporary directory should be creatable")
    }

    #[test]
    fn a_git_link_line_is_read_without_its_line_ending() {
        let dir = temp();
        let file = dir.path().join(".git");
        fs::write(&file, "gitdir: x\n").expect("the .git file should be writable");

        assert_eq!(first_line(&file).as_deref(), Some("gitdir: x"));
    }

    #[test]
    fn a_first_line_past_the_cap_is_not_a_git_link() {
        let dir = temp();
        let file = dir.path().join(".git");
        fs::write(&file, vec![b'a'; 8 * 1024]).expect("the .git file should be writable");

        assert_eq!(
            first_line(&file),
            None,
            "8 KiB with no line ending is longer than any git link"
        );
    }

    #[test]
    fn a_fifo_is_not_a_git_link_and_does_not_block() {
        let dir = temp();
        let fifo = dir.path().join(".git");
        let made = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo should be runnable");
        assert!(made.success(), "mkfifo failed for {}", fifo.display());
        let (sender, receiver) = mpsc::channel();
        let read = fifo.clone();
        let _detached = thread::spawn(move || drop(sender.send(first_line(&read))));

        let result = receiver.recv_timeout(Duration::from_secs(3));
        if result.is_err() {
            // Unblock the reader stuck in open, so the thread ends.
            drop(fs::OpenOptions::new().write(true).open(&fifo));
        }
        assert_eq!(
            result.expect("reading a FIFO's first line should return, not block"),
            None
        );
    }
}
