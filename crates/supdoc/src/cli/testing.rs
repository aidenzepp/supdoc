//! Test doubles the `cli/` unit tests share: captured output, a closed pipe, one parsed run and a
//! temporary project.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Parser as _;

use crate::config::Config;
use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
use crate::output::{Output, OutputSettings, PathStyle, Verbosity};
use crate::path::AbsolutePath;
use crate::read::cartridge::Languages;
use crate::scan::model::{Scope, TargetState};
use crate::scan::walk::GlobalExcludes;

use super::args::Cli;
use super::{CliError, Context, Outcome};

/// A buffer `Output` writes into and the test reads back.
#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    /// Everything written so far.
    pub(crate) fn text(&self) -> String {
        let bytes = self.0.lock().expect("the capture lock should be free");
        String::from_utf8(bytes.clone()).expect("captured output should be UTF-8")
    }
}

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the capture lock should be free")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer whose reader has hung up.
pub(crate) struct ClosedPipe;

impl io::Write for ClosedPipe {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The value at `pointer` (RFC 6901) in `json`, or a failure naming what is missing.
pub(crate) fn at(json: &serde_json::Value, pointer: &str) -> serde_json::Value {
    json.pointer(pointer)
        .unwrap_or_else(|| panic!("`{pointer}` should be in the JSON output: {json}"))
        .clone()
}

/// Reports that a repository-only test was skipped, since a packaged crate lacks the repository.
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why; the test harness shows captured stderr on request"
)]
pub(crate) fn skip(missing: &Path) {
    eprintln!(
        "skipped: {} is absent outside the repository",
        missing.display()
    );
}

/// What one `supdoc` run returned and printed.
pub(crate) struct Run {
    /// The command's result.
    pub(crate) result: Result<Outcome, CliError>,
    /// What it wrote to stdout.
    pub(crate) stdout: String,
    /// What it wrote to stderr.
    pub(crate) stderr: String,
}

impl Run {
    /// The exit code the process would end with.
    pub(crate) fn exit_code(&self) -> u8 {
        match &self.result {
            Ok(outcome) => outcome.exit_code(),
            Err(error) => error.exit_code(),
        }
    }

    /// stdout parsed as the one JSON document `--json` prints.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", self.stdout))
    }
}

/// One parsed command line with the output and context it runs with, wired as `main` wires them.
pub(crate) struct Session {
    /// The parsed command line.
    pub(crate) cli: Cli,
    /// The context the command runs in.
    pub(crate) context: Context,
    /// The output the command prints through.
    pub(crate) output: Output,
    stdout: Capture,
    stderr: Capture,
}

impl Session {
    /// Parses `supdoc <args>` to run from `cwd`, with output paths provisionally rooted there as
    /// `main` roots them before any config loads.
    pub(crate) fn new(cwd: AbsolutePath, args: &[&str]) -> Self {
        let command_line: Vec<&str> = std::iter::once("supdoc")
            .chain(args.iter().copied())
            .collect();
        let cli = Cli::try_parse_from(&command_line)
            .unwrap_or_else(|error| panic!("`{}` should parse: {error}", command_line.join(" ")));
        let settings = OutputSettings {
            format: cli.global.format(),
            verbosity: Verbosity::from_flags(cli.global.quiet, cli.global.verbose),
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
        };
        let (stdout, stderr) = (Capture::default(), Capture::default());
        let output = Output::new(settings, Box::new(stdout.clone()), Box::new(stderr.clone()));
        let context = Context {
            global: cli.global.clone(),
            cwd,
            global_excludes: GlobalExcludes::none(),
        };
        Self {
            cli,
            context,
            output,
            stdout,
            stderr,
        }
    }

    /// What the run printed, with `result`.
    pub(crate) fn finished(self, result: Result<Outcome, CliError>) -> Run {
        Run {
            result,
            stdout: self.stdout.text(),
            stderr: self.stderr.text(),
        }
    }
}

/// A project in a fresh temporary directory.
pub(crate) struct Project {
    /// The directory that is the project root.
    pub(crate) dir: tempfile::TempDir,
}

impl Project {
    /// An empty project.
    pub(crate) fn empty() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a fresh temp directory should be created"),
        }
    }

    /// The project root.
    pub(crate) fn root(&self) -> AbsolutePath {
        AbsolutePath::new(self.dir.path()).expect("a temp directory should be absolute")
    }

    /// The directory `relative` under the root, as an absolute path.
    pub(crate) fn root_of(&self, relative: &str) -> AbsolutePath {
        AbsolutePath::new(&self.dir.path().join(relative))
            .expect("a directory under the root should be absolute")
    }

    /// Writes `text` at `relative` under the root, creating its directories.
    pub(crate) fn write(&self, relative: &str, text: &str) {
        let path = self.dir.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture directory should be created");
        }
        fs::write(&path, text).expect("the fixture file should be written");
    }

    /// Records every present link's current hash in `supdoc.lock`, as `update --yes` would.
    pub(crate) fn record_all(&self) {
        let config = Config::load(&self.root()).expect("the fixture config should load");
        let scan = crate::scan::assemble::run(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes::none(),
        )
        .expect("the fixture project should scan");
        let mut lock = Lockfile::default();
        for (key, link) in scan.links {
            if let TargetState::Present { hash, .. } = link.target {
                let _previous = lock.insert(key, hash);
            }
        }
        let path = AbsolutePath::new(&self.dir.path().join(LOCK_FILE_NAME))
            .expect("the lockfile path should be absolute");
        lock.write(&path, config.boundary())
            .expect("the fixture lockfile should be written");
    }

    /// Every file under the root and its bytes.
    pub(crate) fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn collect(dir: &Path, found: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).expect("a fixture directory should be readable") {
                let path = entry.expect("a directory entry should be readable").path();
                if path.is_dir() {
                    collect(&path, found);
                } else {
                    let bytes = fs::read(&path).expect("a fixture file should be readable");
                    let _previous = found.insert(path, bytes);
                }
            }
        }
        let mut found = BTreeMap::new();
        collect(self.dir.path(), &mut found);
        found
    }
}
