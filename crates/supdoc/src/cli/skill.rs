//! `supdoc skill install`: writes the embedded `SKILL.md` where agent runtimes look for skills
//! (06 X16).
//!
//! It always writes two copies, one for the shared agents directory and one for Claude Code, and
//! nothing else. The project scope puts them under the project root, the config's directory or
//! the current directory when there is no config. `--global` puts them under `HOME` and Claude
//! Code's config directory. Each copy is a real file written atomically. A copy that already
//! holds the embedded text is left alone; a different one is replaced only under `--yes`. A
//! project install writes nothing outside the project: it refuses a symlinked directory on the
//! way to either copy that leads out of the root (04 S15), and follows one that stays inside.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::confine::real_location;
use crate::config::{ConfigError, discover_config};
use crate::finding::{Finding, Severity};
use crate::lock::error::shell_quote;
use crate::lock::io::write_atomically;
use crate::output::json::Envelope;
use crate::output::paths::relative_to;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, Report, TextContext};
use crate::path::AbsolutePath;

use super::args::{SkillInstallArgs, WriteMode};
use super::project::{absolute, config_failure};
use super::{CliError, Context, Outcome};

/// The skill, embedded so the installed copy always matches this binary.
const SKILL: &str = include_str!("../../SKILL.md");

/// Where the skill goes beneath the shared agents base: the project root or `HOME`.
const AGENTS_SKILL: &str = ".agents/skills/supdoc/SKILL.md";

/// Where the skill goes beneath the project root for Claude Code.
const PROJECT_CLAUDE_SKILL: &str = ".claude/skills/supdoc/SKILL.md";

/// Claude Code's config directory beneath `HOME` when `CLAUDE_CONFIG_DIR` is not set.
const DEFAULT_CLAUDE_DIR: &str = ".claude";

/// Where the skill goes beneath Claude Code's config directory.
const CLAUDE_DIR_SKILL: &str = "skills/supdoc/SKILL.md";

/// The closing note of a preview that would change something (06a K7).
const PREVIEW_NOTE: &str = "Nothing written. Run the same command with --yes to apply.";

/// The closing note when every copy already holds the embedded skill.
const UP_TO_DATE_NOTE: &str = "The supdoc skill is already installed and up to date.";

/// The closing note after writing at least one copy.
const INSTALLED_NOTE: &str = "Installed the supdoc skill.";

/// Runs `supdoc skill install`: previews, or under `--yes` writes, both copies of `SKILL.md`.
///
/// The config is never loaded; only its location decides the project root.
///
/// # Errors
///
/// Returns the config error (78) when `--config` names a missing path, a dangling symlink or a
/// path through a file, the config read error (74) when it names a directory or its metadata
/// cannot be read, and the config error (78) when the config entry found while looking for the
/// project root is a symlink that does not lead to a `supdoc.config.toml`, all before anything is
/// written or previewed, `cli-skill-no-home` when `--global` is given and `HOME` is not set,
/// `cli-skill-outside-root` when a project copy would be written through a symlink out of the
/// project, before anything is written or previewed, `cli-skill-write` when a copy cannot be
/// written, and `cli-skill-output` when the report cannot be written.
pub fn run(
    args: &SkillInstallArgs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let applied = args.confirm.mode() == WriteMode::Apply;
    let base = if args.global {
        context.cwd.clone()
    } else {
        project_root(context, output)?
    };
    let destinations = destinations(args.global, &base, context)?;
    if !args.global {
        for path in &destinations {
            contain(&base, path, &context.cwd)?;
        }
    }
    let changes: Vec<SkillChange> = destinations
        .into_iter()
        .map(|path| SkillChange {
            action: inspect(&path),
            shown: label(&path, &base),
            path,
        })
        .collect();

    if applied {
        for change in changes.iter().filter(|change| change.action.writes()) {
            install_file(change.path.as_path(), SKILL.as_bytes())
                .map_err(|error| write_failed(change, args.global, &context.cwd, &error))?;
        }
    }

    if !args.global {
        output.set_root(base);
    }
    let report = SkillInstallReport::new(applied, changes);
    output
        .report(&report, true)
        .map_err(|error| output_failed(&error))?;
    output
        .note(report.closing_note())
        .map_err(|error| output_failed(&error))?;
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc skill install`'s `--json` output: the whole envelope, with `data`
/// typed as [`SkillInstallReport`].
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, SkillInstallReport>))
}

/// The two files the skill goes to: the shared agents copy, then Claude Code's.
///
/// # Errors
///
/// Returns `cli-skill-no-home` when `global` is set and `HOME` is not.
fn destinations(
    global: bool,
    root: &AbsolutePath,
    context: &Context,
) -> Result<[AbsolutePath; 2], CliError> {
    if !global {
        return Ok([
            beneath(root, AGENTS_SKILL),
            beneath(root, PROJECT_CLAUDE_SKILL),
        ]);
    }
    let home = context.home.as_ref().ok_or_else(no_home)?;
    let claude_dir = context
        .claude_config_dir
        .clone()
        .unwrap_or_else(|| beneath(home, DEFAULT_CLAUDE_DIR));
    Ok([
        beneath(home, AGENTS_SKILL),
        beneath(&claude_dir, CLAUDE_DIR_SKILL),
    ])
}

/// The project root without loading the config: the directory of `--config` when given,
/// otherwise the directory of the config that discovery finds searching upward from the current
/// directory, the same rule every other command follows, otherwise the current directory.
///
/// # Errors
///
/// Returns the config error (78) when `--config` names a path that does not exist, a symlink that
/// leads nowhere, or a path through a file, and the config read error (74) when it names a
/// directory or its metadata cannot be read. Discovery returns the config error (78) when it
/// stops at a symlinked `supdoc.config.toml` that does not lead to a file of that name.
fn project_root(context: &Context, output: &mut Output) -> Result<AbsolutePath, CliError> {
    if let Some(file) = &context.global.config {
        let file = absolute(&context.cwd, file);
        check_config_exists(&file)
            .map_err(|error| config_failure(&error, context, output, "skill install"))?;
        return Ok(file.parent().unwrap_or(file));
    }
    let found = discover_config(&context.cwd)
        .map_err(|error| config_failure(&error, context, output, "skill install"))?;
    Ok(found
        .and_then(|file| file.parent())
        .unwrap_or_else(|| context.cwd.clone()))
}

/// Refuses a `--config` that is not an existing file, from its metadata alone: the file is never
/// opened, read or named-checked, since only its location decides the root.
///
/// Effect: reads the file's metadata, following symlinks.
///
/// # Errors
///
/// [`ConfigError::NotFound`] when the path is missing, a dangling symlink, or beneath a file,
/// and [`ConfigError::Read`] for a directory or any other metadata failure.
fn check_config_exists(file: &AbsolutePath) -> Result<(), ConfigError> {
    let read_failure = |error| ConfigError::Read {
        path: file.clone(),
        error,
    };
    match fs::metadata(file.as_path()) {
        Ok(metadata) if metadata.is_dir() => Err(read_failure(io::Error::new(
            io::ErrorKind::IsADirectory,
            "is a directory",
        ))),
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Err(ConfigError::NotFound { path: file.clone() })
        }
        Err(error) => Err(read_failure(error)),
    }
}

/// `relative` joined onto `base`.
fn beneath(base: &AbsolutePath, relative: &str) -> AbsolutePath {
    AbsolutePath::new(&base.as_path().join(relative))
        .unwrap_or_else(|| unreachable!("a path joined onto an absolute directory is absolute"))
}

/// `path` as a person reads it: relative to `base` when it is beneath it, absolute otherwise.
fn label(path: &AbsolutePath, base: &AbsolutePath) -> String {
    path.strip_prefix(base).map_or_else(
        || path.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

/// Refuses `target` when a directory between `root` and it leads outside `root` once symlinks
/// are followed, so a project install never writes outside the project (04 S15). A symlink that
/// leads elsewhere inside `root` is followed. A dangling one counts where it points, and one whose
/// real location can't be worked out counts as outside, as [`Config::contains`] decides for reads.
///
/// [`Config::contains`]: crate::config::Config::contains
///
/// Effect: reads file metadata and symlinks along the directories beneath `root` on the way to
/// `target`.
///
/// # Errors
///
/// Returns `cli-skill-outside-root` naming the first directory that leads out, in the help
/// relative to `cwd` so its commands run as written.
fn contain(root: &AbsolutePath, target: &AbsolutePath, cwd: &AbsolutePath) -> Result<(), CliError> {
    // Every project destination is beneath the root, so there is always a directory chain.
    let Some(directories) = target.strip_prefix(root).and_then(Path::parent) else {
        return Ok(());
    };
    let real_root = real_location(root.as_path()).unwrap_or_else(|| root.as_path().to_path_buf());
    let mut dir = root.as_path().to_path_buf();
    for component in directories.components() {
        dir.push(component);
        match real_location(&dir) {
            Some(real) if real.starts_with(&real_root) => {}
            real => return Err(outside_root(root, target, &dir, real.as_deref(), cwd)),
        }
    }
    Ok(())
}

/// What installing does to `path`. Only a regular file holding exactly the embedded skill is
/// `unchanged`, so a symlink or a directory is never treated as installed.
fn inspect(path: &AbsolutePath) -> SkillAction {
    match fs::symlink_metadata(path.as_path()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => SkillAction::Create,
        Ok(metadata)
            if metadata.is_file()
                && fs::read(path.as_path()).is_ok_and(|bytes| bytes == SKILL.as_bytes()) =>
        {
            SkillAction::Unchanged
        }
        Ok(_) | Err(_) => SkillAction::Replace,
    }
}

/// `supdoc skill install`: what happens, or would happen, to each copy of the skill.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SkillInstallReport {
    /// `false` for a preview: nothing was written.
    applied: bool,
    /// One per destination: the shared agents copy, then Claude Code's.
    changes: Vec<SkillChange>,
    /// A `cli-skill-exists` warning for each different file a preview would replace.
    #[serde(skip)]
    findings: Vec<Finding>,
}

impl SkillInstallReport {
    /// The report for `changes`, warning about each file a preview would replace.
    fn new(applied: bool, changes: Vec<SkillChange>) -> Self {
        let findings = if applied {
            Vec::new()
        } else {
            changes
                .iter()
                .filter(|change| change.action == SkillAction::Replace)
                .map(|change| exists(&change.shown))
                .collect()
        };
        Self {
            applied,
            changes,
            findings,
        }
    }

    /// The note that ends the run.
    fn closing_note(&self) -> &'static str {
        if !self.changes.iter().any(|change| change.action.writes()) {
            UP_TO_DATE_NOTE
        } else if self.applied {
            INSTALLED_NOTE
        } else {
            PREVIEW_NOTE
        }
    }
}

impl Report for SkillInstallReport {
    fn command(&self) -> &'static str {
        "skill install"
    }

    fn findings(&self) -> &[Finding] {
        &self.findings
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        for change in &self.changes {
            let action = change.action.as_str();
            writeln!(out, "{action:<9}  {}", visible(&change.shown))?;
        }
        for finding in &self.findings {
            render_finding(context, finding, out)?;
        }
        Ok(())
    }
}

/// One copy of the skill and what installing does to it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SkillChange {
    /// What happens to the file.
    action: SkillAction,
    /// The file, absolute.
    path: AbsolutePath,
    /// The file as text output shows it.
    #[serde(skip)]
    shown: String,
}

/// What installing does to one copy of the skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SkillAction {
    /// Nothing is there yet; the file is created.
    Create,
    /// Something different is there; it is replaced under `--yes`.
    Replace,
    /// The file already holds the embedded skill and is not rewritten.
    Unchanged,
}

impl SkillAction {
    /// The word a text line starts with.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Replace => "replace",
            Self::Unchanged => "unchanged",
        }
    }

    /// Whether applying this action writes the file.
    const fn writes(self) -> bool {
        matches!(self, Self::Create | Self::Replace)
    }
}

/// Writes `bytes` to `target`, creating its directories, through the shared temp-and-rename
/// writer. An existing regular file keeps its permissions; a symlink is replaced, never written
/// through. Directories are created or followed as they are: a project install has already
/// refused any that lead out of the project ([`contain`]).
///
/// # Errors
///
/// Fails if a directory cannot be created or the file cannot be written; `target` is then
/// unchanged.
fn install_file(target: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let permissions = fs::symlink_metadata(target)
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.permissions());
    write_atomically(target, bytes, permissions.as_ref())
}

/// The warning for a different `SKILL.md` a preview would replace (06 error table, Q25).
fn exists(shown: &str) -> Finding {
    Finding::new(
        "cli-skill-exists",
        Severity::Warning,
        format!("{shown} already exists with different contents"),
    )
    .with_help("Re-run with `--yes` to replace it")
}

/// The refusal for a project copy that `link` would send out of `root`: to `real`, or somewhere
/// that can't be worked out when `real` is `None`.
///
/// The message names the copy and the link as the report lines do, relative to `root`. The help
/// names the link relative to `cwd`, so its `rm` and `mkdir` run as written.
fn outside_root(
    root: &AbsolutePath,
    target: &AbsolutePath,
    link: &Path,
    real: Option<&Path>,
    cwd: &AbsolutePath,
) -> CliError {
    let shown_link = link.strip_prefix(root.as_path()).unwrap_or(link).display();
    let typed_link = AbsolutePath::new(link).map_or_else(
        || shell_quote(&link.display().to_string()),
        |link| shell_quote(&relative_to(cwd, &link)),
    );
    let shown = label(target, root);
    let message = match real {
        Some(real) => {
            let landing = target
                .as_path()
                .strip_prefix(link)
                .map_or_else(|_| real.to_path_buf(), |rest| real.join(rest));
            format!(
                "{shown} would be written to {}, outside the project root: {shown_link} is a \
                 symlink to {}",
                landing.display(),
                real.display()
            )
        }
        None => format!(
            "{shown} would be written through {shown_link}, a symlink whose destination can't be \
             worked out, so it may be outside the project root"
        ),
    };
    CliError::new(
        "cli-skill-outside-root",
        message,
        format!(
            "Nothing was written. Point {typed_link} at a directory inside the project, or \
             install into your home directory with `supdoc skill install --global --yes`. To \
             use a real directory instead, run `rm {typed_link}` (this removes only the link, \
             not the directory it points to), then `mkdir {typed_link}`"
        ),
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for `--global` without a home directory.
fn no_home() -> CliError {
    CliError::new(
        "cli-skill-no-home",
        "`--global` installs into your home directory, but HOME is not set",
        "Set HOME to your home directory, or drop `--global` to install into the project",
        CliError::USAGE_EXIT_CODE,
    )
}

/// The error for a copy of the skill that could not be written.
///
/// The message names the file as the report lines do. The help names its directory so a person
/// can use it from `cwd`: relative to it for a project install, absolute for `--global`, whose
/// copies are outside the project.
fn write_failed(
    change: &SkillChange,
    global: bool,
    cwd: &AbsolutePath,
    error: &io::Error,
) -> CliError {
    let directory = change.path.parent().unwrap_or_else(|| change.path.clone());
    let shown_directory = if global {
        directory.display().to_string()
    } else {
        relative_to(cwd, &directory)
    };
    CliError::new(
        "cli-skill-write",
        format!("cannot write {}: {error}", change.shown),
        format!("Check that you can create and write files in `{shown_directory}`"),
        CliError::IO_EXIT_CODE,
    )
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli-skill-output",
        format!("cannot write the skill install report: {error}"),
        "Check that standard output and standard error are open and writable, or redirect both \
         to a file with `> supdoc.out 2>&1`",
        CliError::IO_EXIT_CODE,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, UNIX_EPOCH};

    use clap::Parser as _;

    use crate::cli::args::{Cli, Command, SkillCommand};
    use crate::config::CONFIG_FILE_NAME;
    use crate::output::{OutputSettings, PathStyle, Verbosity};

    use super::*;

    /// A writer that keeps what it is given, shared with the test that reads it back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("the capture lock should be free");
            String::from_utf8(bytes.clone()).expect("captured output should be UTF-8")
        }
    }

    impl Write for Capture {
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

    /// What one run produced.
    struct Run {
        result: Result<Outcome, CliError>,
        stdout: String,
        stderr: String,
    }

    impl Run {
        /// The exit code the process would end with.
        fn exit_code(&self) -> u8 {
            match &self.result {
                Ok(outcome) => outcome.exit_code(),
                Err(error) => error.exit_code(),
            }
        }

        /// stdout parsed as the one JSON document `--json` prints.
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.stdout)
                .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", self.stdout))
        }
    }

    /// A fresh project directory and a fresh home directory, both empty.
    struct World {
        project: tempfile::TempDir,
        home: tempfile::TempDir,
    }

    impl World {
        fn new() -> Self {
            Self {
                project: tempfile::tempdir().expect("a project temp directory should be created"),
                home: tempfile::tempdir().expect("a home temp directory should be created"),
            }
        }

        fn project_file(&self, relative: &str) -> PathBuf {
            self.project.path().join(relative)
        }

        fn home_file(&self, relative: &str) -> PathBuf {
            self.home.path().join(relative)
        }

        /// Runs `supdoc skill install <flags>` in `cwd`, with this world's home and the given
        /// Claude Code config directory.
        fn install_in(&self, cwd: &Path, claude_dir: Option<&Path>, flags: &[&str]) -> Run {
            let mut words = vec!["supdoc", "skill", "install"];
            words.extend_from_slice(flags);
            let cli = Cli::try_parse_from(&words)
                .unwrap_or_else(|error| panic!("`{}` should parse: {error}", words.join(" ")));
            let Command::Skill(SkillCommand::Install(args)) = &cli.command else {
                panic!("`{}` should be skill install", words.join(" "));
            };
            let cwd = AbsolutePath::new(cwd).expect("the working directory should be absolute");
            let settings = OutputSettings {
                format: cli.global.format(),
                verbosity: Verbosity::from_flags(cli.global.quiet, cli.global.verbose),
                color: false,
                unicode: false,
                width: 80,
                paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
            };
            let (stdout, stderr) = (Capture::default(), Capture::default());
            let mut output =
                Output::new(settings, Box::new(stdout.clone()), Box::new(stderr.clone()));
            let context = Context {
                global: cli.global.clone(),
                cwd,
                home: AbsolutePath::new(self.home.path()),
                claude_config_dir: claude_dir.and_then(AbsolutePath::new),
            };
            let result = run(args, &context, &mut output);
            Run {
                result,
                stdout: stdout.text(),
                stderr: stderr.text(),
            }
        }

        /// Runs `supdoc skill install <flags>` from the project directory.
        fn install(&self, flags: &[&str]) -> Run {
            self.install_in(self.project.path(), None, flags)
        }
    }

    /// The contents of `path`, or `None` when nothing is there.
    fn contents(path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    /// Writes `text` to `path`, creating its directories.
    fn plant(path: &Path, text: &str) {
        let parent = path.parent().expect("a planted file has a parent");
        fs::create_dir_all(parent).expect("the planted file's directory should be created");
        fs::write(path, text).expect("the planted file should be written");
    }

    /// The `action` and `path` of each change in a `--json` document.
    fn changes(json: &serde_json::Value) -> Vec<(String, String)> {
        at(json, "/data/changes")
            .as_array()
            .expect("data.changes should be an array")
            .iter()
            .map(|change| {
                (
                    at(change, "/action")
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    at(change, "/path").as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }

    /// The value at `pointer` in `json`, which the test expects to be there.
    fn at<'a>(json: &'a serde_json::Value, pointer: &str) -> &'a serde_json::Value {
        json.pointer(pointer)
            .unwrap_or_else(|| panic!("{pointer} should be in {json}"))
    }

    /// The action of the first change in a `--json` document.
    fn first_action(json: &serde_json::Value) -> String {
        changes(json)
            .into_iter()
            .next()
            .map(|(action, _)| action)
            .expect("there should be at least one change")
    }

    fn display(path: &Path) -> String {
        path.display().to_string()
    }

    #[test]
    fn skill_install_preview_writes_nothing() {
        let world = World::new();

        let run = world.install(&[]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            run.stdout,
            "create     .agents/skills/supdoc/SKILL.md\ncreate     .claude/skills/supdoc/SKILL.md\n",
        );
        assert!(run.stderr.contains(PREVIEW_NOTE), "stderr: {}", run.stderr);
        assert!(
            !world.project_file(".agents").exists() && !world.project_file(".claude").exists(),
            "a preview should create no directory"
        );
    }

    #[test]
    fn skill_install_yes_writes_the_embedded_skill_to_both_project_directories() {
        let world = World::new();

        let run = world.install(&["--yes", "--json"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        for relative in [AGENTS_SKILL, PROJECT_CLAUDE_SKILL] {
            assert_eq!(
                contents(&world.project_file(relative)).as_deref(),
                Some(SKILL),
                "{relative} should hold the embedded skill"
            );
        }
        let json = run.json();
        assert_eq!(at(&json, "/data/applied"), true);
        assert_eq!(
            changes(&json),
            [
                (
                    "create".to_owned(),
                    display(&world.project_file(AGENTS_SKILL))
                ),
                (
                    "create".to_owned(),
                    display(&world.project_file(PROJECT_CLAUDE_SKILL))
                ),
            ],
            "one change per path, each absolute"
        );
    }

    #[test]
    fn skill_install_global_writes_under_home_and_the_default_claude_directory() {
        let world = World::new();

        let run = world.install(&["--global", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        for relative in [AGENTS_SKILL, ".claude/skills/supdoc/SKILL.md"] {
            assert_eq!(
                contents(&world.home_file(relative)).as_deref(),
                Some(SKILL),
                "~/{relative} should hold the embedded skill"
            );
        }
        assert!(
            !world.project_file(".agents").exists(),
            "--global should leave the project alone"
        );
    }

    #[test]
    fn skill_install_global_honors_claude_config_dir() {
        let world = World::new();
        let claude_dir = tempfile::tempdir().expect("a Claude config temp directory");

        let run = world.install_in(
            world.project.path(),
            Some(claude_dir.path()),
            &["--global", "--yes"],
        );

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            contents(&claude_dir.path().join(CLAUDE_DIR_SKILL)).as_deref(),
            Some(SKILL),
            "the Claude copy should go under CLAUDE_CONFIG_DIR"
        );
        assert_eq!(
            contents(&world.home_file(AGENTS_SKILL)).as_deref(),
            Some(SKILL),
            "the agents copy should still go under HOME"
        );
        assert!(
            !world.home_file(".claude").exists(),
            "~/.claude should be untouched when CLAUDE_CONFIG_DIR is set"
        );
    }

    #[test]
    fn skill_install_global_without_home_is_a_usage_error() {
        let world = World::new();
        let cli = Cli::try_parse_from(["supdoc", "skill", "install", "--global", "--yes"])
            .expect("the command line should parse");
        let Command::Skill(SkillCommand::Install(args)) = &cli.command else {
            panic!("should be skill install");
        };
        let cwd = AbsolutePath::new(world.project.path()).expect("absolute");
        let context = Context {
            global: cli.global.clone(),
            cwd: cwd.clone(),
            home: None,
            claude_config_dir: None,
        };
        let settings = OutputSettings {
            format: cli.global.format(),
            verbosity: Verbosity::Normal,
            color: false,
            unicode: false,
            width: 80,
            paths: PathStyle::new(cwd.clone(), cwd, None),
        };
        let mut output = Output::new(
            settings,
            Box::new(Capture::default()),
            Box::new(Capture::default()),
        );

        let error = run(args, &context, &mut output).expect_err("--global needs HOME");

        assert_eq!(error.code(), "cli-skill-no-home");
        assert_eq!(error.exit_code(), CliError::USAGE_EXIT_CODE);
    }

    #[test]
    fn skill_install_project_root_is_the_config_directory() {
        let world = World::new();
        plant(&world.project_file(CONFIG_FILE_NAME), "");
        let nested = world.project_file("docs/guide");
        fs::create_dir_all(&nested).expect("the nested directory should be created");

        let run = world.install_in(&nested, None, &["--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            contents(&world.project_file(AGENTS_SKILL)).as_deref(),
            Some(SKILL),
            "the skill should go beside supdoc.config.toml, not under the current directory"
        );
        assert!(!nested.join(".agents").exists());
    }

    #[test]
    fn skill_install_refuses_to_overwrite_without_yes() {
        let world = World::new();
        let agents = world.project_file(AGENTS_SKILL);
        plant(&agents, "my own skill\n");

        let run = world.install(&["--json"]);

        assert_eq!(
            run.exit_code(),
            0,
            "a preview exits 0; stderr: {}",
            run.stderr
        );
        assert_eq!(contents(&agents).as_deref(), Some("my own skill\n"));
        let json = run.json();
        assert_eq!(at(&json, "/data/applied"), false);
        assert_eq!(first_action(&json), "replace");
        let codes: Vec<&str> = at(&json, "/findings")
            .as_array()
            .expect("findings should be an array")
            .iter()
            .filter_map(|finding| finding.get("kind").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(codes, ["cli-skill-exists"]);
    }

    #[test]
    fn skill_install_exists_warning_names_the_project_relative_path_from_a_subdirectory() {
        let world = World::new();
        plant(&world.project_file(CONFIG_FILE_NAME), "");
        plant(&world.project_file(AGENTS_SKILL), "my own skill\n");
        let nested = world.project_file("docs/guide");
        fs::create_dir_all(&nested).expect("the nested directory should be created");

        let run = world.install_in(&nested, None, &["--json"]);

        let json = run.json();
        let message = at(&json, "/findings/0/message")
            .as_str()
            .expect("the finding message should be text");
        assert_eq!(
            message,
            format!("{AGENTS_SKILL} already exists with different contents")
        );
    }

    #[test]
    fn skill_install_yes_replaces_a_different_skill() {
        let world = World::new();
        let agents = world.project_file(AGENTS_SKILL);
        plant(&agents, "my own skill\n");
        let sibling = world.project_file(".agents/skills/supdoc/notes.md");
        plant(&sibling, "keep me\n");

        let run = world.install(&["--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(contents(&agents).as_deref(), Some(SKILL));
        assert_eq!(
            contents(&sibling).as_deref(),
            Some("keep me\n"),
            "only SKILL.md is touched"
        );
        assert!(run.stdout.starts_with("replace "), "stdout: {}", run.stdout);
    }

    #[test]
    fn skill_install_identical_file_is_unchanged_and_not_rewritten() {
        let world = World::new();
        let agents = world.project_file(AGENTS_SKILL);
        plant(&agents, SKILL);
        let old = UNIX_EPOCH + Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(&agents)
            .and_then(|file| file.set_modified(old))
            .expect("the planted skill's mtime should be set");

        let run = world.install(&["--yes", "--json"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(first_action(&run.json()), "unchanged");
        let modified = fs::metadata(&agents)
            .and_then(|metadata| metadata.modified())
            .expect("the skill's mtime should be readable");
        assert_eq!(modified, old, "an identical skill should not be rewritten");
        assert_eq!(
            contents(&world.project_file(PROJECT_CLAUDE_SKILL)).as_deref(),
            Some(SKILL),
            "the missing copy is still written"
        );
    }

    #[test]
    fn skill_install_when_everything_is_current_says_so() {
        let world = World::new();
        plant(&world.project_file(AGENTS_SKILL), SKILL);
        plant(&world.project_file(PROJECT_CLAUDE_SKILL), SKILL);

        let run = world.install(&[]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert!(
            run.stderr.contains(UP_TO_DATE_NOTE),
            "stderr: {}",
            run.stderr
        );
        assert!(!run.stderr.contains(PREVIEW_NOTE), "stderr: {}", run.stderr);
    }

    #[test]
    fn skill_install_write_failure_exits_74() {
        let world = World::new();
        // A file where a directory must go makes the write fail.
        plant(&world.project_file(".agents/skills"), "not a directory\n");

        let run = world.install(&["--yes"]);

        let error = run.result.as_ref().expect_err("the write should fail");
        assert_eq!(error.code(), "cli-skill-write");
        assert_eq!(run.exit_code(), CliError::IO_EXIT_CODE);
    }

    #[test]
    fn skill_install_write_failure_names_the_file_from_the_root_and_the_directory_from_the_cwd() {
        let world = World::new();
        plant(&world.project_file(CONFIG_FILE_NAME), "");
        fs::create_dir(world.project_file("docs")).expect("the docs directory should be created");
        // A file where a directory must go makes the write fail.
        plant(&world.project_file(".agents/skills"), "not a directory\n");

        let run = world.install_in(&world.project_file("docs"), None, &["--yes"]);

        let error = run.result.as_ref().expect_err("the write should fail");
        assert!(
            error
                .message()
                .starts_with("cannot write .agents/skills/supdoc/SKILL.md: "),
            "the message should name the copy relative to the root: {}",
            error.message()
        );
        assert_eq!(
            error.help(),
            "Check that you can create and write files in `../.agents/skills/supdoc`",
            "the help should name the directory so it can be used from the current directory"
        );
    }

    /// Makes `link` a symlink to `destination`.
    #[cfg(unix)]
    fn symlink(destination: &Path, link: &Path) {
        let parent = link.parent().expect("a symlink has a parent");
        fs::create_dir_all(parent).expect("the symlink's directory should be created");
        std::os::unix::fs::symlink(destination, link).expect("the symlink should be created");
    }

    /// Every file and directory under `dir`, relative to it.
    #[cfg(unix)]
    fn tree(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in fs::read_dir(&next).expect("the directory should be readable") {
                let path = entry.expect("the entry should be readable").path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                found.push(
                    path.strip_prefix(dir)
                        .expect("an entry is beneath its directory")
                        .to_path_buf(),
                );
            }
        }
        found
    }

    /// Fails unless `run` refused with `cli-skill-outside-root`, naming `link` and `destination`.
    #[cfg(unix)]
    fn assert_refused_outside(run: &Run, link: &str, destination: &Path) {
        let error = run
            .result
            .as_ref()
            .expect_err("an install through a symlink out of the project should be refused");
        assert_eq!(error.code(), "cli-skill-outside-root", "got {error:?}");
        assert_eq!(run.exit_code(), CliError::REFUSED_EXIT_CODE);
        let real = fs::canonicalize(destination).expect("the destination should resolve");
        assert!(
            error
                .message()
                .contains(&format!("{link} is a symlink to {}", real.display())),
            "the message should name the symlink and where it leads: {}",
            error.message()
        );
        let help = error.help();
        let global = help
            .find("--global")
            .expect("the help should offer `--global`");
        let remove = help
            .find(&format!("`rm {link}`"))
            .expect("the help should offer removing the link as its own step");
        assert!(
            global < remove && help.contains("removes only the link") && !help.contains("&&"),
            "the help should lead with the non-destructive routes, say `rm` removes only the \
             link, and give separate steps: {help}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_refuses_an_agents_directory_symlinked_out_of_the_project() {
        let world = World::new();
        plant(&world.project_file(CONFIG_FILE_NAME), "");
        let elsewhere = tempfile::tempdir().expect("an outside temp directory should be created");
        symlink(elsewhere.path(), &world.project_file(".agents"));
        let landing = fs::canonicalize(elsewhere.path())
            .expect("the destination should resolve")
            .join("skills/supdoc/SKILL.md");

        for flags in [&[][..], &["--yes"]] {
            let run = world.install(flags);

            assert_refused_outside(&run, ".agents", elsewhere.path());
            let message = run.result.as_ref().err().map(CliError::message);
            assert!(
                message.is_some_and(|text| text.contains(&landing.display().to_string())),
                "the message should name where the file would land: {message:?}"
            );
        }
        assert_eq!(
            tree(elsewhere.path()),
            Vec::<PathBuf>::new(),
            "nothing should be written through the symlink"
        );
        assert!(
            !world.project_file(".claude").exists(),
            "a refusal should write neither copy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_outside_root_help_names_the_link_from_a_subdirectory() {
        let world = World::new();
        plant(&world.project_file(CONFIG_FILE_NAME), "");
        let elsewhere = tempfile::tempdir().expect("an outside temp directory should be created");
        symlink(elsewhere.path(), &world.project_file(".agents"));
        let docs = world.project_file("docs");
        fs::create_dir_all(&docs).expect("the docs directory should be created");

        let run = world.install_in(&docs, None, &[]);

        let error = run
            .result
            .as_ref()
            .expect_err("an install through a symlink out of the project should be refused");
        assert_eq!(error.code(), "cli-skill-outside-root", "got {error:?}");
        assert!(
            error.message().contains(".agents is a symlink to "),
            "the message should name the link as the report lines do: {}",
            error.message()
        );
        assert!(
            error.help().contains("`rm ../.agents`") && error.help().contains("`mkdir ../.agents`"),
            "the help's commands should run from docs/: {}",
            error.help()
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_refuses_a_nested_claude_directory_symlinked_out_of_the_project() {
        let world = World::new();
        let elsewhere = tempfile::tempdir().expect("an outside temp directory should be created");
        symlink(elsewhere.path(), &world.project_file(".claude/skills"));

        let run = world.install(&["--yes"]);

        assert_refused_outside(&run, ".claude/skills", elsewhere.path());
        assert_eq!(
            tree(elsewhere.path()),
            Vec::<PathBuf>::new(),
            "nothing should be written through the symlink"
        );
        assert!(
            !world.project_file(".agents").exists(),
            "a refusal should write neither copy, even the one before it"
        );
    }

    /// A project with a real config at its root and a nested directory that `setup` fills in.
    #[cfg(unix)]
    fn nested_config_link(world: &World, setup: impl FnOnce(&Path)) -> PathBuf {
        plant(&world.project_file(CONFIG_FILE_NAME), "");
        let inner = world.project_file("inner");
        fs::create_dir_all(&inner).expect("the nested directory should be created");
        setup(&inner);
        inner
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_refuses_a_config_symlink_that_is_not_a_config_and_creates_nothing() {
        type Setup = fn(&Path);
        let cases: [(&str, Setup); 4] = [
            ("dangling", |inner| {
                symlink(Path::new("missing.toml"), &inner.join(CONFIG_FILE_NAME));
            }),
            ("looping", |inner| {
                symlink(Path::new(CONFIG_FILE_NAME), &inner.join(CONFIG_FILE_NAME));
            }),
            ("directory", |inner| {
                fs::create_dir_all(inner.join("target"))
                    .expect("the target directory should be created");
                symlink(Path::new("target"), &inner.join(CONFIG_FILE_NAME));
            }),
            ("wrong name", |inner| {
                plant(&inner.join("base.toml"), "");
                symlink(Path::new("base.toml"), &inner.join(CONFIG_FILE_NAME));
            }),
        ];
        for (name, setup) in cases {
            for flags in [&["--yes"][..], &[][..]] {
                let world = World::new();
                let inner = nested_config_link(&world, setup);
                let before = tree(world.project.path());

                let run = world.install_in(&inner, None, flags);

                let error = run
                    .result
                    .as_ref()
                    .expect_err("a config symlink that is not a config should be refused");
                assert_eq!(
                    (error.code(), error.exit_code()),
                    ("config-symlink-not-config", CliError::CONFIG_EXIT_CODE),
                    "{name} target with {flags:?}"
                );
                assert_eq!(
                    tree(world.project.path()),
                    before,
                    "{name} target with {flags:?} should create nothing"
                );
                assert_eq!(
                    run.stdout, "",
                    "{name} target with {flags:?} previews nothing"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_roots_at_a_config_symlink_that_leads_to_a_real_config() {
        let world = World::new();
        let inner = nested_config_link(&world, |inner| {
            let real = inner.join("elsewhere/supdoc.config.toml");
            plant(&real, "");
            symlink(&real, &inner.join(CONFIG_FILE_NAME));
        });

        let run = world.install_in(&inner, None, &["--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            contents(&inner.join(AGENTS_SKILL)).as_deref(),
            Some(SKILL),
            "the skill should go beside the symlinked config"
        );
        assert!(!world.project_file(AGENTS_SKILL).exists());
    }

    #[test]
    fn skill_install_without_a_config_roots_at_the_current_directory() {
        let world = World::new();
        let nested = world.project_file("docs");
        fs::create_dir_all(&nested).expect("the nested directory should be created");

        let run = world.install_in(&nested, None, &["--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(contents(&nested.join(AGENTS_SKILL)).as_deref(), Some(SKILL));
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_refuses_a_dangling_symlink_that_points_out_of_the_project() {
        let world = World::new();
        let elsewhere = tempfile::tempdir().expect("an outside temp directory should be created");
        symlink(
            &elsewhere.path().join("not-yet"),
            &world.project_file(".agents"),
        );

        let run = world.install(&[]);

        let error = run
            .result
            .as_ref()
            .expect_err("a dangling symlink out of the project should be refused");
        assert_eq!(error.code(), "cli-skill-outside-root", "got {error:?}");
        assert!(
            error.message().contains(".agents is a symlink to "),
            "the message should name the symlink: {}",
            error.message()
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_follows_a_symlink_that_stays_inside_the_project() {
        let world = World::new();
        fs::create_dir_all(world.project_file("shared/agents"))
            .expect("the shared directory should be created");
        symlink(Path::new("shared/agents"), &world.project_file(".agents"));

        let run = world.install(&["--yes"]);

        assert_eq!(run.exit_code(), 0, "result: {:?}", run.result);
        assert_eq!(
            contents(&world.project_file("shared/agents/skills/supdoc/SKILL.md")).as_deref(),
            Some(SKILL),
            "the skill should be written through an in-project symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_global_still_writes_through_a_symlinked_home_directory() {
        let world = World::new();
        let shared = tempfile::tempdir().expect("a shared temp directory should be created");
        symlink(shared.path(), &world.home_file(".agents"));

        let run = world.install(&["--global", "--yes"]);

        assert_eq!(run.exit_code(), 0, "result: {:?}", run.result);
        assert_eq!(
            contents(&shared.path().join("skills/supdoc/SKILL.md")).as_deref(),
            Some(SKILL),
            "--global writes wherever the home directory's links lead, as before"
        );
    }

    #[test]
    fn skill_install_json_validates_against_its_schema() {
        let world = World::new();
        plant(&world.project_file(AGENTS_SKILL), "different\n");
        let json = world.install(&["--json"]).json();
        let schema = serde_json::to_value(schema().expect("skill install is built"))
            .expect("the schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");

        let errors: Vec<String> = validator
            .iter_errors(&json)
            .map(|error| error.to_string())
            .collect();

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
    }

    /// Reports that a repository-only test was skipped, since a packaged crate lacks the repository.
    #[expect(
        clippy::print_stderr,
        reason = "a skipped test must say why; the test harness shows captured stderr on request"
    )]
    fn skip(missing: &std::path::Path) {
        eprintln!(
            "skipped: {} is absent outside the repository",
            missing.display()
        );
    }

    #[test]
    fn the_repository_root_skill_is_identical_to_the_embedded_one() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SKILL.md");
        let root_copy = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                skip(&path);
                return;
            }
            Err(error) => panic!("{} should be readable: {error}", path.display()),
        };

        assert_eq!(
            root_copy, SKILL,
            "SKILL.md at the repository root should match crates/supdoc/SKILL.md"
        );
    }

    #[test]
    fn embedded_skill_has_name_and_description_frontmatter() {
        let frontmatter = SKILL
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(head, _)| head)
            .expect("SKILL.md should start with a frontmatter block");

        assert!(frontmatter.lines().any(|line| line == "name: supdoc"));
        assert!(
            frontmatter
                .lines()
                .any(|line| line.starts_with("description: ")),
            "frontmatter: {frontmatter}"
        );
    }
}
