//! `supdoc skill install`: writes the embedded `SKILL.md` where agent runtimes look for skills
//!
//! It always writes two copies, one for the shared agents directory and one for Claude Code, and
//! nothing else. The project scope puts them under the project root, the config's directory or
//! the current directory when there is no config. `--global` puts them under `HOME` and Claude
//! Code's config directory. Each copy is a real file written atomically. A copy that already
//! holds the embedded text is left alone; a different one is replaced only under `--yes`. A
//! project install writes nothing outside the project: it refuses a symlinked directory on the
//! way to either copy that leads out of the root, and follows one that stays inside.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::confine::{self, Boundary, real_location, write_atomically};
use crate::config::{ConfigError, discover_config};
use crate::exit::ExitStatus;
use crate::finding::{Finding, Severity};
use crate::lock::error::shell_quote;
use crate::output::json::Envelope;
use crate::output::paths::{relative_or_absolute, relative_to};
use crate::output::text::{render_finding, visible};
use crate::output::{Output, RecordArray, Report, TextContext, preview_note};
use crate::path::AbsolutePath;

use super::args::{SkillInstallArgs, WriteMode};
use super::project::{absolute, command_line, config_failure};
use super::{CliError, Context, Outcome, cause};

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

/// The closing note when every copy already holds the embedded skill.
const UP_TO_DATE_NOTE: &str = "The supdoc skill is already installed and up to date.";

/// The closing note after writing at least one copy.
const INSTALLED_NOTE: &str = "Installed the supdoc skill.";

/// The environment variable that moves Claude Code's config directory.
const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

/// What `CLAUDE_CONFIG_DIR` said, keeping a relative value instead of dropping it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudeConfigDir {
    /// Unset or empty: Claude Code's directory is `~/.claude`.
    Unset,
    /// An absolute directory.
    Absolute(AbsolutePath),
    /// A relative value, which `--global` refuses.
    Relative(OsString),
}

/// The directories `skill install --global` writes under, read once by `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillDirs {
    /// The user's home directory (`HOME`), when set to an absolute path.
    pub(crate) home: Option<AbsolutePath>,
    /// Claude Code's config directory (`CLAUDE_CONFIG_DIR`).
    pub(crate) claude_config_dir: ClaudeConfigDir,
}

impl SkillDirs {
    /// Reads `HOME` and `CLAUDE_CONFIG_DIR` through `var`.
    ///
    /// An unset or empty value is absent, and a relative `HOME` counts as unset.
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        let claude_config_dir = match set(CLAUDE_CONFIG_DIR) {
            None => ClaudeConfigDir::Unset,
            Some(value) => AbsolutePath::new(Path::new(&value))
                .map_or(ClaudeConfigDir::Relative(value), ClaudeConfigDir::Absolute),
        };
        Self {
            home: set("HOME").and_then(|value| AbsolutePath::new(Path::new(&value))),
            claude_config_dir,
        }
    }
}

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
/// written, `cli-skill-relative-claude-dir` when `--global` is given and `CLAUDE_CONFIG_DIR` is
/// relative, and `cli-output-failed` when the report cannot be written.
pub(crate) fn run(
    args: &SkillInstallArgs,
    dirs: &SkillDirs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let applied = args.confirm.mode() == WriteMode::Apply;
    let base = if args.global {
        context.cwd.clone()
    } else {
        project_root(context, output)?
    };
    let destinations = destinations(args.global, &base, dirs)?;
    let boundaries = destinations
        .each_ref()
        .map(|(owner, _path)| Boundary::resolve(owner));
    if !args.global {
        for ((_owner, path), boundary) in destinations.iter().zip(&boundaries) {
            contain(boundary, &base, path, context)?;
        }
    }
    let changes: Vec<SkillChange> = destinations
        .into_iter()
        .map(|(_owner, path)| SkillChange {
            action: inspect(&path),
            shown: relative_or_absolute(&base, &path),
            path,
        })
        .collect();

    if applied {
        for (change, boundary) in changes
            .iter()
            .zip(&boundaries)
            .filter(|(change, _boundary)| change.action.writes())
        {
            install_file(boundary, &change.path, SKILL.as_bytes())
                .map_err(|error| write_failed(change, args.global, &context.cwd, &error))?;
        }
    }

    if !args.global {
        output.set_root(base);
    }
    let apply_note = preview_note(
        args.confirm.dry_run,
        &command_line(
            context,
            "skill install",
            &[] as &[&str],
            if args.global {
                &["--global", "--yes"]
            } else {
                &["--yes"]
            },
        ),
    );
    let report = SkillInstallReport::new(applied, args.global, changes, context);
    output
        .report(&report, true)
        .map_err(|error| CliError::output_failed("the skill install report", &error))?;
    output
        .note(report.closing_note(&apply_note))
        .map_err(|error| CliError::output_failed("the skill install report", &error))?;
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc skill install`'s `--json` output: the whole envelope, with `data`
/// typed as [`SkillInstallReport`].
#[must_use]
pub(crate) fn schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, SkillInstallReport>)
}

/// The two files the skill goes to, each with the directory it is admitted against: the shared
/// agents copy, then Claude Code's. A project copy belongs beneath the project root. A `--global`
/// copy belongs in its own directory wherever the links under `HOME` or Claude Code's directory
/// lead, as the operator set them up.
///
/// # Errors
///
/// Returns `cli-skill-no-home` when `global` is set and `HOME` is not, and
/// `cli-skill-relative-claude-dir` when `global` is set and `CLAUDE_CONFIG_DIR` is relative.
fn destinations(
    global: bool,
    root: &AbsolutePath,
    dirs: &SkillDirs,
) -> Result<[(AbsolutePath, AbsolutePath); 2], CliError> {
    if !global {
        return Ok([
            (root.clone(), beneath(root, AGENTS_SKILL)),
            (root.clone(), beneath(root, PROJECT_CLAUDE_SKILL)),
        ]);
    }
    let home = dirs.home.as_ref().ok_or_else(no_home)?;
    let claude_dir = match &dirs.claude_config_dir {
        ClaudeConfigDir::Unset => beneath(home, DEFAULT_CLAUDE_DIR),
        ClaudeConfigDir::Absolute(dir) => dir.clone(),
        ClaudeConfigDir::Relative(value) => return Err(relative_claude_dir(value)),
    };
    let in_own_directory =
        |file: AbsolutePath| (file.parent().unwrap_or_else(|| file.clone()), file);
    Ok([
        in_own_directory(beneath(home, AGENTS_SKILL)),
        in_own_directory(beneath(&claude_dir, CLAUDE_DIR_SKILL)),
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
        check_config_exists(&file).map_err(|error| config_failure(&error, context, output))?;
        return Ok(file.parent().unwrap_or(file));
    }
    let found =
        discover_config(&context.cwd).map_err(|error| config_failure(&error, context, output))?;
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
    base.join_path(Path::new(relative))
}

/// Refuses `target` when a directory between `root` and it leads outside `boundary`, the
/// project root, once symlinks are followed, so a project install never writes outside the
/// project. A symlink that leads elsewhere inside the root is followed. A dangling one counts
/// where it points, and one whose real location can't be worked out counts as outside, as
/// [`Boundary::contains`] decides for reads.
///
/// Effect: reads file metadata and symlinks along the directories beneath `root` on the way to
/// `target`.
///
/// # Errors
///
/// Returns `cli-skill-outside-root` naming the first directory that leads out, in the help
/// relative to the current directory so its commands run as written.
fn contain(
    boundary: &Boundary,
    root: &AbsolutePath,
    target: &AbsolutePath,
    context: &Context,
) -> Result<(), CliError> {
    // Every project destination is beneath the root, so there is always a directory chain.
    let Some(directories) = target.strip_prefix(root).and_then(Path::parent) else {
        return Ok(());
    };
    let mut dir = root.as_path().to_path_buf();
    for component in directories.components() {
        dir.push(component);
        if !boundary.contains(&dir) {
            let real = real_location(&dir);
            return Err(outside_root(root, target, &dir, real.as_deref(), context));
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
    fn new(applied: bool, global: bool, changes: Vec<SkillChange>, context: &Context) -> Self {
        let findings = if applied {
            Vec::new()
        } else {
            changes
                .iter()
                .filter(|change| change.action == SkillAction::Replace)
                .map(|change| exists(&change.shown, global, context))
                .collect()
        };
        Self {
            applied,
            changes,
            findings,
        }
    }

    /// The note that ends the run.
    fn closing_note<'a>(&self, apply_note: &'a str) -> &'a str {
        if !self.changes.iter().any(|change| change.action.writes()) {
            UP_TO_DATE_NOTE
        } else if self.applied {
            INSTALLED_NOTE
        } else {
            apply_note
        }
    }
}

impl Report for SkillInstallReport {
    fn record_arrays(&self) -> &'static [RecordArray] {
        const { &[RecordArray::new("changes", "change")] }
    }

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
/// writer, each admitted against `boundary`, the directory the copy belongs beneath, immediately
/// before it is made. An existing regular file keeps its permissions; a symlink is replaced,
/// never written through. A project install has already refused any directory that leads out of
/// the project ([`contain`]).
///
/// # Errors
///
/// Fails if `boundary` refuses a directory or the file, a directory cannot be created, or the
/// file cannot be written; `target` is then unchanged.
fn install_file(boundary: &Boundary, target: &AbsolutePath, bytes: &[u8]) -> io::Result<()> {
    let escaped = |_escape| {
        io::Error::other("a symlinked directory on the way leads out of where the skill belongs")
    };
    if let Some(parent) = target.parent() {
        confine::create_dir_all(&boundary.admit(&parent).map_err(escaped)?, 0o777)?;
    }
    let admitted = boundary.admit(target).map_err(escaped)?;
    let permissions = fs::symlink_metadata(admitted.as_path())
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.permissions());
    write_atomically(&admitted, bytes, permissions.as_ref())
}

/// The warning for a different `SKILL.md` a preview would replace.
fn exists(shown: &str, global: bool, context: &Context) -> Finding {
    let flags: &[&str] = if global {
        &["--global", "--yes"]
    } else {
        &["--yes"]
    };
    let rerun = command_line::<&str>(context, "skill install", &[], flags);
    Finding::new(
        "cli-skill-exists",
        Severity::Warning,
        format!("{shown} already exists with different contents"),
    )
    .with_help(format!("Run `{rerun}` to replace it"))
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
    context: &Context,
) -> CliError {
    let cwd = &context.cwd;
    let shown_link = link.strip_prefix(root.as_path()).unwrap_or(link).display();
    let typed_link = AbsolutePath::new(link).map_or_else(
        || shell_quote(&link.display().to_string()),
        |link| shell_quote(&relative_to(cwd, &link)),
    );
    let shown = relative_or_absolute(root, target);
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
    let global_install =
        command_line::<&str>(context, "skill install", &[], &["--global", "--yes"]);
    CliError::new(
        "cli-skill-outside-root",
        message,
        format!(
            "Nothing was written. Point {typed_link} at a directory inside the project, or \
             install into your home directory with `{global_install}`. To \
             use a real directory instead, run `rm {typed_link}` (this removes only the link, \
             not the directory it points to), then `mkdir {typed_link}`"
        ),
        ExitStatus::Failed,
    )
}

/// The error for `--global` without a home directory.
fn no_home() -> CliError {
    CliError::new(
        "cli-skill-no-home",
        "`--global` installs into your home directory, but HOME is not set",
        "Set HOME, for example `export HOME=/home/you`, or drop `--global` to install into the project",
        ExitStatus::Usage,
    )
}

/// The error for a relative `CLAUDE_CONFIG_DIR`, which would otherwise send the Claude Code copy
/// somewhere the person did not mean.
fn relative_claude_dir(value: &OsString) -> CliError {
    CliError::new(
        "cli-skill-relative-claude-dir",
        format!(
            "{CLAUDE_CONFIG_DIR} is `{}`, which is not an absolute path",
            value.to_string_lossy()
        ),
        format!("Set `{CLAUDE_CONFIG_DIR}` to an absolute path, or unset it to use `~/.claude`"),
        ExitStatus::Usage,
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
        format!("cannot write {}: {}", change.shown, cause(error)),
        format!("Check that you can create and write files in `{shown_directory}`"),
        ExitStatus::Io,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use crate::cli::args::{Command, SkillCommand};
    use crate::cli::testing::{Run, Session, at, skip};
    use crate::config::CONFIG_FILE_NAME;

    use super::*;

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
            let mut words = vec!["skill", "install"];
            words.extend_from_slice(flags);
            let cwd = AbsolutePath::new(cwd).expect("the working directory should be absolute");
            let mut session = Session::new(cwd, &words);
            let Command::Skill(SkillCommand::Install(args)) = &session.cli.command else {
                panic!("`supdoc {}` should be skill install", words.join(" "));
            };
            let dirs = SkillDirs {
                home: AbsolutePath::new(self.home.path()),
                claude_config_dir: claude_dir
                    .and_then(AbsolutePath::new)
                    .map_or(ClaudeConfigDir::Unset, ClaudeConfigDir::Absolute),
            };
            let result = run(args, &dirs, &session.context, &mut session.output);
            session.finished(result)
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
        assert!(
            run.stderr
                .contains("Run `supdoc skill install --yes` to apply."),
            "stderr: {}",
            run.stderr
        );
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
        let cwd = AbsolutePath::new(world.project.path()).expect("absolute");
        let mut session = Session::new(cwd, &["skill", "install", "--global", "--yes"]);
        let Command::Skill(SkillCommand::Install(args)) = &session.cli.command else {
            panic!("should be skill install");
        };
        let dirs = SkillDirs::from_vars(|_| None);

        let error = run(args, &dirs, &session.context, &mut session.output)
            .expect_err("--global needs HOME");

        assert_eq!(error.code(), "cli-skill-no-home");
        assert_eq!(error.exit_code(), ExitStatus::Usage.code());
    }

    #[test]
    fn a_relative_claude_config_dir_is_kept_and_refused_for_global() {
        let dirs = SkillDirs::from_vars(|name| match name {
            "HOME" => Some(OsString::from("/home/me")),
            _ => Some(OsString::from("rel/claude")),
        });
        let root = AbsolutePath::new(Path::new("/work")).expect("absolute");

        assert_eq!(
            dirs.claude_config_dir,
            ClaudeConfigDir::Relative(OsString::from("rel/claude"))
        );
        let error = destinations(true, &root, &dirs).expect_err("relative should be refused");
        assert_eq!(error.code(), "cli-skill-relative-claude-dir");
        assert_eq!(error.exit_code(), ExitStatus::Usage.code());
        assert!(
            error.to_string().contains("rel/claude"),
            "the message should name the value: {error}"
        );
        assert!(
            destinations(false, &root, &dirs).is_ok(),
            "a project install should ignore CLAUDE_CONFIG_DIR"
        );
    }

    #[test]
    fn unset_and_empty_claude_config_dirs_are_unset() {
        for value in [None, Some("")] {
            let dirs = SkillDirs::from_vars(|name| {
                (name == CLAUDE_CONFIG_DIR)
                    .then_some(value)
                    .flatten()
                    .map(OsString::from)
            });
            assert_eq!(dirs.claude_config_dir, ClaudeConfigDir::Unset);
        }
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
        let findings = at(&json, "/findings");
        let codes: Vec<&str> = findings
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
        let message = at(&json, "/findings/0/message");
        let message = message
            .as_str()
            .expect("the finding message should be text");
        assert_eq!(
            message,
            format!("{AGENTS_SKILL} already exists with different contents")
        );
    }

    #[test]
    fn skill_install_exists_help_keeps_the_config_and_its_command_replaces_the_skill() {
        let world = World::new();
        plant(&world.project_file(CONFIG_FILE_NAME), "");
        plant(&world.project_file(AGENTS_SKILL), "my own skill\n");
        let nested = world.project_file("docs");
        fs::create_dir_all(&nested).expect("the nested directory should be created");

        let run = world.install_in(&nested, None, &["-c", "../supdoc.config.toml", "--json"]);

        let json = run.json();
        let help = at(&json, "/findings/0/help")
            .as_str()
            .expect("the finding help should be text")
            .to_owned();
        let printed = "supdoc -c ../supdoc.config.toml skill install --yes";
        assert!(
            help.contains(&format!("`{printed}`")),
            "the help should name the install with its config and --yes: {help}"
        );
        let again = world.install_in(&nested, None, &["-c", "../supdoc.config.toml", "--yes"]);
        assert_eq!(again.exit_code(), 0, "stderr: {}", again.stderr);
        assert_eq!(
            contents(&world.project_file(AGENTS_SKILL)).as_deref(),
            Some(SKILL),
            "the printed command should replace the skill"
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
        assert!(
            !run.stderr
                .contains("Run `supdoc skill install --yes` to apply."),
            "stderr: {}",
            run.stderr
        );
    }

    #[test]
    fn skill_install_write_failure_exits_74() {
        let world = World::new();
        // A file where a directory must go makes the write fail.
        plant(&world.project_file(".agents/skills"), "not a directory\n");

        let run = world.install(&["--yes"]);

        let error = run.result.as_ref().expect_err("the write should fail");
        assert_eq!(error.code(), "cli-skill-write");
        assert_eq!(run.exit_code(), ExitStatus::Io.code());
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
        assert_eq!(run.exit_code(), ExitStatus::Failed.code());
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
                    ("config-symlink-not-config", ExitStatus::Config.code()),
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
        let schema = serde_json::to_value(schema()).expect("the schema should serialize");
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
