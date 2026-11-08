//! A fixture copied into a temporary project, and the `supdoc` runs made inside it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// What one `supdoc` run printed and how it ended.
pub(crate) struct Run {
    /// The arguments after `supdoc`, joined for failure messages.
    pub(crate) args: String,
    /// Everything written to stdout.
    pub(crate) stdout: String,
    /// Everything written to stderr.
    pub(crate) stderr: String,
    /// The exit code, or `None` when a signal ended the process.
    pub(crate) code: Option<i32>,
}

impl Run {
    /// The run of `supdoc args` that produced `output`.
    fn from_output(args: &[&str], output: Output) -> Self {
        Self {
            args: args.join(" "),
            stdout: String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
            code: output.status.code(),
        }
    }

    /// Fails unless the run exited with `code`, showing both streams.
    pub(crate) fn assert_exit(&self, code: i32) -> &Self {
        assert_eq!(
            self.code,
            Some(code),
            "`supdoc {}` should exit {code}\nstdout:\n{}\nstderr:\n{}",
            self.args,
            self.stdout,
            self.stderr
        );
        self
    }

    /// Stdout parsed as the one JSON document a `--json` run prints.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "`supdoc {}` should print one JSON document on stdout ({error}):\n{}",
                self.args, self.stdout
            )
        })
    }
}

/// The value at the JSON `pointer` (`/data/summary/failed`), or a failure naming the pointer.
pub(crate) fn at<'a>(value: &'a serde_json::Value, pointer: &str) -> &'a serde_json::Value {
    value
        .pointer(pointer)
        .unwrap_or_else(|| panic!("the JSON output should have `{pointer}`: {value}"))
}

/// The array at the JSON `pointer`, or a failure naming the pointer.
pub(crate) fn array_at<'a>(
    value: &'a serde_json::Value,
    pointer: &str,
) -> &'a Vec<serde_json::Value> {
    at(value, pointer)
        .as_array()
        .unwrap_or_else(|| panic!("`{pointer}` should be an array: {value}"))
}

/// The absolute `PATH` directory holding the real `binary`, for the test named `test`, with a
/// stderr note saying whether `test` runs it or skips.
///
/// `None` when `binary` is not on `PATH`, so a local run without the tool skips the test. With
/// `SUPDOC_REQUIRE_REAL_TOOLS` set (CI sets it), a missing tool fails the test instead.
pub(crate) fn real_tool_dir(binary: &str, test: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .find(|dir| dir.is_absolute() && dir.join(binary).is_file());
    let required =
        std::env::var_os("SUPDOC_REQUIRE_REAL_TOOLS").is_some_and(|value| !value.is_empty());
    assert!(
        dir.is_some() || !required,
        "{test} needs the real `{binary}` on PATH because SUPDOC_REQUIRE_REAL_TOOLS is set"
    );
    let note = match &dir {
        Some(dir) => format!("REAL TOOL {test}: runs {binary} from {}\n", dir.display()),
        None => format!("SKIPPED {test}: {binary} is not on PATH\n"),
    };
    // Written to the process's stderr directly, which `cargo test` shows for a passing test.
    let _ignored = std::io::Write::write_all(&mut std::io::stderr(), note.as_bytes());
    dir
}

/// A fixture from `tests/fixtures/` copied into its own temporary directory, so runs can write to it.
pub(crate) struct Project {
    /// Keeps the directory alive until the project is dropped.
    _dir: TempDir,
    /// An empty directory outside the project that stands in for every git config location.
    no_git_config: TempDir,
    /// The copied fixture's root.
    root: PathBuf,
}

impl Project {
    /// Copies the fixture `name` into a fresh temporary directory.
    pub(crate) fn fixture(name: &str) -> Self {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let dir = TempDir::new().expect("a temporary directory should be creatable");
        let root = dir.path().to_path_buf();
        copy_tree(&source, &root);
        let no_git_config =
            TempDir::new().expect("an empty git config directory should be creatable");
        Self {
            _dir: dir,
            root,
            no_git_config,
        }
    }

    /// The absolute path of `relative` inside the project.
    pub(crate) fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Runs `supdoc` with `args` from the project root.
    pub(crate) fn supdoc(&self, args: &[&str]) -> Run {
        self.supdoc_in(".", args)
    }

    /// Runs `supdoc` with `args` from the project directory `dir`, with no inherited environment.
    pub(crate) fn supdoc_in(&self, dir: &str, args: &[&str]) -> Run {
        self.supdoc_in_with(dir, args, &[])
    }

    /// Runs `supdoc` with `args` from the project directory `dir`, with only the environment
    /// variables `vars`.
    pub(crate) fn supdoc_in_with(&self, dir: &str, args: &[&str], vars: &[(&str, &Path)]) -> Run {
        let output = self
            .command(dir, args, vars)
            .output()
            .expect("the supdoc binary should start");
        Run::from_output(args, output)
    }

    /// Runs `supdoc` with `args` from the project directory `dir` with no inherited
    /// environment, failing the test if it runs past `limit`.
    pub(crate) fn supdoc_bounded(&self, dir: &str, args: &[&str], limit: Duration) -> Run {
        let mut child = self
            .command(dir, args, &[])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the supdoc binary should start");
        let started = Instant::now();
        loop {
            if child
                .try_wait()
                .expect("the run should be waitable")
                .is_some()
            {
                break;
            }
            if started.elapsed() > limit {
                drop(child.kill());
                drop(child.wait());
                panic!("`supdoc {}` hung for over {limit:?}", args.join(" "));
            }
            thread::sleep(Duration::from_millis(50));
        }
        let output = child
            .wait_with_output()
            .expect("the finished run's output should be readable");
        Run::from_output(args, output)
    }

    /// The `supdoc args` command, run from the project directory `dir` with only `vars` set.
    ///
    /// Git's environment is isolated first: `HOME` and `XDG_CONFIG_HOME` are an empty directory
    /// and `GIT_CONFIG_SYSTEM` a missing file in it, with `GIT_CONFIG_GLOBAL` cleared with the rest,
    /// so no run reads the real user's or the machine's git config. `vars` may set any of them.
    fn command(&self, dir: &str, args: &[&str], vars: &[(&str, &Path)]) -> Command {
        let empty = self.no_git_config.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_supdoc"));
        let _command = command
            .args(args)
            .current_dir(self.path(dir))
            .env_clear()
            .env("HOME", empty)
            .env("XDG_CONFIG_HOME", empty)
            .env("GIT_CONFIG_SYSTEM", empty.join("gitconfig"))
            .envs(vars.iter().copied());
        command
    }

    /// Writes `text` to `relative`, creating its directories.
    pub(crate) fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the fixture directory should be creatable");
        }
        std::fs::write(path, text).expect("the fixture file should be writable");
    }

    /// Records every link in `supdoc.lock` from the project root, as a user's first `update --yes`.
    pub(crate) fn record(&self) {
        let _ = self.supdoc(&["update", "--yes"]).assert_exit(0);
    }

    /// The text of `relative`.
    pub(crate) fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative))
            .unwrap_or_else(|error| panic!("{relative} should be readable: {error}"))
    }

    /// Replaces `from` with `to` in `relative`, failing if `from` isn't there to replace.
    pub(crate) fn replace(&self, relative: &str, from: &str, to: &str) {
        let text = self.read(relative);
        assert!(
            text.contains(from),
            "{relative} should contain {from:?} before the edit:\n{text}"
        );
        std::fs::write(self.path(relative), text.replacen(from, to, 1))
            .unwrap_or_else(|error| panic!("{relative} should be writable: {error}"));
    }

    /// Every link recorded in the lockfile at `lock`, as `target` or `target#symbol`.
    pub(crate) fn locked_links(&self, lock: &str) -> BTreeSet<String> {
        let text = self.read(lock);
        let table: toml::Table = text
            .parse()
            .unwrap_or_else(|error| panic!("{lock} should be TOML ({error}):\n{text}"));
        let Some(links) = table.get("link").and_then(toml::Value::as_array) else {
            return BTreeSet::new();
        };
        links
            .iter()
            .map(|link| {
                let field = |name: &str| link.get(name).and_then(toml::Value::as_str);
                let target = field("target")
                    .unwrap_or_else(|| panic!("every [[link]] in {lock} should have a target"));
                field("symbol")
                    .map_or_else(|| target.to_owned(), |symbol| format!("{target}#{symbol}"))
            })
            .collect()
    }
}

/// Copies the directory `from` into the existing directory `to`, recursively.
fn copy_tree(from: &Path, to: &Path) {
    let entries = std::fs::read_dir(from)
        .unwrap_or_else(|error| panic!("fixture {} should be readable: {error}", from.display()));
    for entry in entries {
        let entry = entry.expect("a fixture directory entry should be readable");
        let destination = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .expect("a fixture entry's type should be readable");
        if kind.is_dir() {
            std::fs::create_dir_all(&destination)
                .expect("the copy's directory should be creatable");
            copy_tree(&entry.path(), &destination);
        } else {
            let _ = std::fs::copy(entry.path(), &destination).expect("a fixture file should copy");
        }
    }
}

/// Builds the set `locked_links` returns from string literals.
pub(crate) fn links(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Creates a symlink at `link` pointing to `target`, a file link or a directory link as `target` is.
///
/// A relative `target` is resolved against `link`'s parent, as the operating system resolves it.
/// Unix has one kind of symlink. Windows has two, so the kind follows what `target` is: a
/// directory gets a directory link, anything else a file link.
pub(crate) fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
    let (target, link) = (target.as_ref(), link.as_ref());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        // Windows rejects a forward slash in a link target (os error 123).
        let target = std::path::PathBuf::from(target.to_string_lossy().replace('/', "\\"));
        let resolved = link
            .parent()
            .map_or_else(|| target.clone(), |parent| parent.join(&target));
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(&target, link)
        } else {
            std::os::windows::fs::symlink_file(&target, link)
        }
    }
}

#[cfg(test)]
mod symlink_tests {
    use super::symlink;
    use std::fs;

    #[test]
    fn symlink_reads_through_file_and_directory_links() {
        let dir = tempfile::tempdir().expect("the temporary directory should be creatable");
        fs::create_dir(dir.path().join("real")).expect("the directory should be creatable");
        fs::write(dir.path().join("real/note.txt"), "hello").expect("the file should be writable");
        symlink("real/note.txt", dir.path().join("file-link"))
            .expect("the file link should be creatable");
        symlink("real", dir.path().join("dir-link"))
            .expect("the directory link should be creatable");

        assert_eq!(
            fs::read_to_string(dir.path().join("file-link")).expect("the file link should read"),
            "hello"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("dir-link/note.txt"))
                .expect("the directory link should read"),
            "hello"
        );
    }
}
