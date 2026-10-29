//! `supdoc skill install`: writes the embedded `SKILL.md` where agent runtimes look for skills
//! (06 X16).
//!
//! It always writes two copies, one for the shared agents directory and one for Claude Code, and
//! nothing else. The project scope puts them under the project root, the config's directory or
//! the current directory when there is no config. `--global` puts them under `HOME` and Claude
//! Code's config directory. Each copy is a real file written atomically. A copy that already
//! holds the embedded text is left alone; a different one is replaced only under `--yes`.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::CONFIG_FILE_NAME;
use crate::finding::{Finding, Severity};
use crate::lock::io::write_atomically;
use crate::output::json::Envelope;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, Report, TextContext};
use crate::path::AbsolutePath;

use super::args::{SkillInstallArgs, WriteMode};
use super::project::absolute;
use super::{CliError, Context, Outcome};

/// The skill, embedded so the installed copy always matches this binary.
const SKILL: &str = include_str!("../../SKILL.md");

/// Where the skill goes beneath the shared agents base: the project root or `HOME`.
const AGENTS_SKILL: &str = ".agents/skills/supdoc/SKILL.md";

/// Where the skill goes beneath the project root for Claude Code.
const PROJECT_CLAUDE_SKILL: &str = ".claude/skills/supdoc/SKILL.md";

/// Claude Code's config directory beneath `HOME` when `CLAUDE_CONFIG_DIR` is not set.
const DEFAULT_CLAUDE_DIR: &str = ".claude";

/// The environment variable that moves Claude Code's config directory.
const CLAUDE_CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";

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
/// Returns `cli/skill-no-home` when `--global` is given and `HOME` is not set, `cli/skill-write`
/// when a copy cannot be written, and `cli/skill-output` when the report cannot be written.
pub fn run(
    args: &SkillInstallArgs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let applied = args.confirm.mode() == WriteMode::Apply;
    let base = if args.global {
        context.cwd.clone()
    } else {
        project_root(context)
    };
    let changes: Vec<SkillChange> = destinations(args.global, context)?
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
                .map_err(|error| write_failed(&change.path, &error))?;
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
/// Returns `cli/skill-no-home` when `global` is set and `HOME` is not, and
/// `cli/skill-relative-claude-dir` when `CLAUDE_CONFIG_DIR` is a relative path, which `main`
/// would otherwise drop without a word and install into `~/.claude` instead.
fn destinations(global: bool, context: &Context) -> Result<[AbsolutePath; 2], CliError> {
    if !global {
        let root = project_root(context);
        return Ok([
            beneath(&root, AGENTS_SKILL),
            beneath(&root, PROJECT_CLAUDE_SKILL),
        ]);
    }
    let home = context.home.as_ref().ok_or_else(no_home)?;
    reject_relative_claude_dir(std::env::var_os(CLAUDE_CONFIG_DIR_VAR).as_deref())?;
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
/// otherwise the nearest directory at or above the current one holding `supdoc.config.toml`,
/// otherwise the current directory.
fn project_root(context: &Context) -> AbsolutePath {
    if let Some(file) = &context.global.config {
        let file = absolute(&context.cwd, file);
        return file.parent().unwrap_or(file);
    }
    std::iter::successors(Some(context.cwd.clone()), AbsolutePath::parent)
        .find(|dir| dir.as_path().join(CONFIG_FILE_NAME).is_file())
        .unwrap_or_else(|| context.cwd.clone())
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
    /// A `cli/skill-exists` warning for each different file a preview would replace.
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
/// through.
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
        "cli/skill-exists",
        Severity::Warning,
        format!("{shown} already exists with different contents"),
    )
    .with_help("Re-run with `--yes` to replace it")
}

/// The error for `--global` without a home directory.
fn no_home() -> CliError {
    CliError::new(
        "cli/skill-no-home",
        "`--global` installs into your home directory, but HOME is not set",
        "Set HOME to your home directory, or drop `--global` to install into the project",
        CliError::USAGE_EXIT_CODE,
    )
}

/// Refuses a relative `CLAUDE_CONFIG_DIR`. `main` keeps only absolute values, so a relative one
/// would silently send the Claude Code copy to the default directory.
///
/// # Errors
///
/// Returns `cli/skill-relative-claude-dir` when `value` is a non-empty relative path.
fn reject_relative_claude_dir(value: Option<&OsStr>) -> Result<(), CliError> {
    match value {
        Some(dir) if !dir.is_empty() && !Path::new(dir).is_absolute() => Err(CliError::new(
            "cli/skill-relative-claude-dir",
            format!(
                "{CLAUDE_CONFIG_DIR_VAR} is `{}`, which is not an absolute path",
                dir.to_string_lossy()
            ),
            format!(
                "Set {CLAUDE_CONFIG_DIR_VAR} to an absolute path, or unset it to use ~/.claude"
            ),
            CliError::USAGE_EXIT_CODE,
        )),
        _ => Ok(()),
    }
}

/// The error for a copy of the skill that could not be written.
fn write_failed(path: &AbsolutePath, error: &io::Error) -> CliError {
    CliError::new(
        "cli/skill-write",
        format!("cannot write {}: {error}", path.display()),
        "Check that you can create and write files in that directory",
        CliError::IO_EXIT_CODE,
    )
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/skill-output",
        format!("cannot write the skill install report: {error}"),
        "Check that standard output and standard error are open and writable",
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
    fn a_relative_claude_config_dir_is_a_usage_error() {
        let error = reject_relative_claude_dir(Some(OsStr::new("rel/claude")))
            .expect_err("a relative CLAUDE_CONFIG_DIR should be refused");

        assert_eq!(error.code(), "cli/skill-relative-claude-dir");
        assert_eq!(error.exit_code(), CliError::USAGE_EXIT_CODE);
        assert!(
            error.to_string().contains("rel/claude"),
            "the message should name the value: {error}"
        );
    }

    #[test]
    fn unset_empty_and_absolute_claude_config_dirs_are_accepted() {
        for value in [None, Some(OsStr::new("")), Some(OsStr::new("/abs/claude"))] {
            assert!(
                reject_relative_claude_dir(value).is_ok(),
                "{value:?} should be accepted"
            );
        }
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

        assert_eq!(error.code(), "cli/skill-no-home");
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
        assert_eq!(codes, ["cli/skill-exists"]);
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
        assert_eq!(error.code(), "cli/skill-write");
        assert_eq!(run.exit_code(), CliError::IO_EXIT_CODE);
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

    #[test]
    fn the_repository_root_skill_is_identical_to_the_embedded_one() {
        let root_copy = include_str!("../../../../SKILL.md");

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
