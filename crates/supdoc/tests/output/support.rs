//! A temporary project, a way to run `supdoc` in it, and a stable transcript of what it printed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The code the fixture docs point at, as recorded in `supdoc.lock`.
const LIB: &str = "\
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn mul(a: i32, b: i32) -> i32 {
    a * b
}

pub fn sub(a: i32, b: i32) -> i32 {
    a - b
}
";

/// A project in a fresh temporary directory.
pub(crate) struct Project {
    /// The project root; removed when the project is dropped.
    dir: tempfile::TempDir,
}

impl Project {
    /// A directory with nothing in it.
    pub(crate) fn empty() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a fresh temp directory should be created"),
        }
    }

    /// A project whose every link is recorded and still accurate, so `check` passes.
    ///
    /// It has one namespace, `core` for `src/`, and two docs: `docs/guide.md` links to `add`
    /// and, through the namespace, `mul`; `docs/old.md` links to `sub`.
    pub(crate) fn clean() -> Self {
        let project = Self::empty();
        project.write(
            "supdoc.config.toml",
            "# Project settings\n[namespaces]\ncore = \"src\"\n",
        );
        project.write(
            "docs/guide.md",
            "# Guide\n\nThe [add](../src/lib.rs#add) function sums.\nThe [mul](core:lib.rs#mul) function multiplies.\n",
        );
        project.write("docs/old.md", "# Old\n\nSee [sub](../src/lib.rs#sub).\n");
        project.write("src/lib.rs", LIB);
        let recorded = project.run(&["update", "--yes"]);
        assert_eq!(
            recorded.code,
            Some(0),
            "`update --yes` should record the fixture links: {}",
            recorded.stderr
        );
        project
    }

    /// [`Project::clean`] after the code and docs moved on, with one link in each state:
    /// `add` changed (failed), `mul` unchanged (passed), `docs/api.md` links to a symbol that
    /// doesn't exist (broken), `docs/setup.md` adds a link (new), and `docs/old.md` is gone
    /// (obsolete).
    pub(crate) fn dirty() -> Self {
        let project = Self::failing();
        fs::remove_file(project.path().join("docs/old.md"))
            .expect("the fixture's docs/old.md should be removable");
        project
    }

    /// [`Project::dirty`] with `docs/old.md` still in place, so no link is obsolete: one
    /// failed, one broken, one new and the rest passed.
    pub(crate) fn failing() -> Self {
        let project = Self::clean();
        project.write("src/lib.rs", &LIB.replacen("a + b", "b + a", 1));
        project.write("docs/api.md", "# API\n\nSee [ad](../src/lib.rs#ad).\n");
        project.write(
            "docs/setup.md",
            "# Setup\n\nThe [mul](../src/lib.rs#mul) function multiplies.\n",
        );
        project
    }

    /// [`Project::clean`] after `docs/old.md` was renamed to `docs/legacy.md`, so its link
    /// carries its history to the new file.
    pub(crate) fn moved() -> Self {
        let project = Self::clean();
        fs::rename(
            project.path().join("docs/old.md"),
            project.path().join("docs/legacy.md"),
        )
        .expect("the fixture's docs/old.md should be renamable");
        project
    }

    /// The project root.
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Writes `text` to `relative`, creating its directories.
    pub(crate) fn write(&self, relative: &str, text: &str) {
        let path = self.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture directory should be created");
        }
        fs::write(&path, text).expect("the fixture file should be written");
    }

    /// Every file under the root and its bytes, to compare before and after a run.
    pub(crate) fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut found = BTreeMap::new();
        let mut pending = vec![self.path().to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).expect("a fixture directory should be readable") {
                let path = entry.expect("a directory entry should be readable").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let bytes = fs::read(&path).expect("a fixture file should be readable");
                    let relative = path
                        .strip_prefix(self.path())
                        .expect("every file walked is under the root")
                        .to_path_buf();
                    let _previous = found.insert(relative, bytes);
                }
            }
        }
        found
    }

    /// Runs `supdoc args` in the project root with an empty environment.
    pub(crate) fn run(&self, args: &[&str]) -> Run {
        self.run_with(args, &[])
    }

    /// Runs `supdoc args` in the project root with only `env` set.
    pub(crate) fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let output = self
            .command(args, env)
            .stdin(Stdio::null())
            .output()
            .expect("the supdoc binary should start");
        Run {
            args: args.join(" "),
            stdout: String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
            code: output.status.code(),
            root: self.path().to_path_buf(),
        }
    }

    /// The `supdoc args` command, run in the project root with only `env` set.
    pub(crate) fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_supdoc"));
        let _command = command
            .args(args)
            .current_dir(self.path())
            .env_clear()
            .envs(env.iter().copied());
        command
    }
}

/// What one run printed and how it ended.
pub(crate) struct Run {
    /// The arguments, joined with spaces, for failure messages and transcripts.
    pub(crate) args: String,
    /// Everything written to stdout.
    pub(crate) stdout: String,
    /// Everything written to stderr.
    pub(crate) stderr: String,
    /// The exit code, or `None` when a signal ended the process.
    pub(crate) code: Option<i32>,
    /// The project root the run happened in, replaced by `[ROOT]` in transcripts.
    root: PathBuf,
}

impl Run {
    /// The command, exit code, stdout and stderr as one stable text, with the temporary
    /// project root spelled `[ROOT]` so the transcript is the same on every machine.
    pub(crate) fn transcript(&self) -> String {
        let code = self
            .code
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        let text = format!(
            "$ supdoc {}\nexit: {code}\n--- stdout\n{}--- stderr\n{}",
            self.args, self.stdout, self.stderr
        );
        redact_root(&text, &self.root)
    }
}

/// `text` with every spelling of `root` replaced by `[ROOT]`: the canonical path first (on
/// macOS the temp directory is reached through the `/private` symlink), then the path as given.
fn redact_root(text: &str, root: &Path) -> String {
    let canonical = fs::canonicalize(root).expect("the project root should be canonicalizable");
    let mut redacted = text.to_owned();
    for spelling in [canonical.as_path(), root] {
        let spelling = spelling
            .to_str()
            .expect("the temp directory path should be UTF-8");
        redacted = redacted.replace(spelling, "[ROOT]");
    }
    redacted
}

/// `text` without ANSI escape sequences (`ESC [ … letter`).
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            for inner in characters.by_ref() {
                if inner.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(character);
        }
    }
    plain
}
