//! A fixture copied into a temporary project, and the `supdoc` runs made inside it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// A fixture from `tests/fixtures/` copied into its own temporary directory, so runs can write to it.
pub(crate) struct Project {
    /// Keeps the directory alive until the project is dropped.
    _dir: TempDir,
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
        Self { _dir: dir, root }
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
        let output = Command::new(env!("CARGO_BIN_EXE_supdoc"))
            .args(args)
            .current_dir(self.path(dir))
            .env_clear()
            .output()
            .expect("the supdoc binary should start");
        Run {
            args: args.join(" "),
            stdout: String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
            code: output.status.code(),
        }
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
