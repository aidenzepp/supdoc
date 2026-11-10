//! `supdoc skill install`: writes the embedded `SKILL.md` where agent runtimes look for skills
//!
//! It always writes two copies, one for the shared agents directory and one for Claude Code, and
//! nothing else. The project scope puts them under the project root, the config's directory or
//! the current directory when there is no config. `--global` puts them under the home directory
//! (`HOME`, or `USERPROFILE` on Windows) and Claude Code's config directory. Each copy is a real
//! file written atomically. A copy that already holds the embedded text is left alone; a different
//! one is replaced only under `--yes`. A
//! project install writes nothing outside the project: it refuses a symlinked directory on the
//! way to either copy that leads out of the root, and follows one that stays inside. Either
//! scope refuses a `SKILL.md` that is itself a symlink leading out of where the copy belongs,
//! and writes neither copy unless both are allowed.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::confine::{self, Boundary, Escape, real_location, write_atomically};
use crate::config::{ConfigError, discover_config};
use crate::exit::ExitStatus;
use crate::finding::{Finding, Severity};
use crate::lock::error::shell_quote;
use crate::output::json::Envelope;
use crate::output::paths::{relative_or_absolute, relative_to};
use crate::output::text::{render_finding, visible};
use crate::output::{Output, RecordArray, Report, TextContext, preview_note};
use crate::path::{AbsolutePath, fixed};

use super::args::{OperatorPath, SkillInstallArgs, WriteMode};
use super::project::{absolute, command_line, config_failure};
use super::{CliError, Context, Outcome, cause};

/// The skill, embedded so the installed copy always matches this binary.
const SKILL: &str = include_str!("../../SKILL.md");

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
    /// The user's home directory, when its variable is set to an absolute path.
    pub(crate) home: Option<AbsolutePath>,
    /// The variable `home` was read from: `HOME`, or `USERPROFILE` on Windows.
    pub(crate) home_var: &'static str,
    /// Claude Code's config directory (`CLAUDE_CONFIG_DIR`).
    pub(crate) claude_config_dir: ClaudeConfigDir,
}

impl SkillDirs {
    /// Reads the home directory and `CLAUDE_CONFIG_DIR` through `var` for the running platform.
    pub(crate) fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        Self::for_platform(var, cfg!(windows))
    }

    /// Reads `USERPROFILE` when `windows` is set and `HOME` otherwise, and `CLAUDE_CONFIG_DIR`,
    /// through `var`.
    ///
    /// An unset or empty value is absent, and a relative home counts as unset.
    pub(crate) fn for_platform(var: impl Fn(&str) -> Option<OsString>, windows: bool) -> Self {
        let home_var = if windows { "USERPROFILE" } else { "HOME" };
        let set = |name: &str| var(name).filter(|value| !value.is_empty());
        let claude_config_dir = match set(CLAUDE_CONFIG_DIR) {
            None => ClaudeConfigDir::Unset,
            Some(value) => AbsolutePath::new(Path::new(&value))
                .map_or(ClaudeConfigDir::Relative(value), ClaudeConfigDir::Absolute),
        };
        Self {
            home: set(home_var).and_then(|value| AbsolutePath::new(Path::new(&value))),
            home_var,
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
/// written or previewed, `cli-skill-no-home` when `--global` is given and the home variable is not set,
/// `cli-skill-outside-root` when a project copy would be written through a symlink out of the
/// project, or a copy's `SKILL.md` is itself a symlink out of where it belongs, before anything
/// is written or previewed, or when a path changed after that check, `cli-skill-write` when a copy cannot be
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
    let place = |owner: &AbsolutePath| Place {
        owner: owner.clone(),
        global: args.global,
    };
    if !args.global {
        for ((_owner, path), boundary) in destinations.iter().zip(&boundaries) {
            contain(boundary, &base, path, context)?;
        }
    }
    // Every copy is admitted before anything is previewed or written, so a preview refuses what
    // `--yes` would, and no copy is written unless both can be.
    for ((owner, path), boundary) in destinations.iter().zip(&boundaries) {
        admit_copy(boundary, path)
            .map_err(|escape| copy_outside(&place(owner), &base, path, &escape, context, &[]))?;
    }
    let owners = destinations.each_ref().map(|(owner, _path)| owner.clone());
    let changes: Vec<SkillChange> = destinations
        .into_iter()
        .map(|(_owner, path)| SkillChange {
            action: inspect(&path),
            shown: relative_or_absolute(&base, &path),
            path,
        })
        .collect();

    if applied {
        // A path can still change between the check and a write, so a refusal names the copies
        // already written.
        let mut written: Vec<String> = Vec::new();
        for ((change, boundary), owner) in changes
            .iter()
            .zip(&boundaries)
            .zip(&owners)
            .filter(|((change, _boundary), _owner)| change.action.writes())
        {
            install_file(boundary, &change.path, SKILL.as_bytes()).map_err(
                |error| match error {
                    NotInstalled::Outside(escape) => copy_outside(
                        &place(owner),
                        &base,
                        &change.path,
                        &escape,
                        context,
                        &written,
                    ),
                    NotInstalled::Io(error) => {
                        write_failed(change, args.global, &context.cwd, &error, &written)
                    }
                },
            )?;
            written.push(change.shown.clone());
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
/// Returns `cli-skill-no-home` when `global` is set and the home directory is not, and
/// `cli-skill-relative-claude-dir` when `global` is set and `CLAUDE_CONFIG_DIR` is relative.
fn destinations(
    global: bool,
    root: &AbsolutePath,
    dirs: &SkillDirs,
) -> Result<[(AbsolutePath, AbsolutePath); 2], CliError> {
    if !global {
        return Ok([
            (root.clone(), root.beneath(&fixed::AGENTS_SKILL)),
            (root.clone(), root.beneath(&fixed::PROJECT_CLAUDE_SKILL)),
        ]);
    }
    let home = dirs.home.as_ref().ok_or_else(|| no_home(dirs.home_var))?;
    let claude_dir = match &dirs.claude_config_dir {
        ClaudeConfigDir::Unset => home.beneath(&fixed::DEFAULT_CLAUDE_DIR),
        ClaudeConfigDir::Absolute(dir) => dir.clone(),
        ClaudeConfigDir::Relative(value) => return Err(relative_claude_dir(value)),
    };
    let in_own_directory =
        |file: AbsolutePath| (file.parent().unwrap_or_else(|| file.clone()), file);
    Ok([
        in_own_directory(home.beneath(&fixed::AGENTS_SKILL)),
        in_own_directory(claude_dir.beneath(&fixed::CLAUDE_DIR_SKILL)),
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
        let file = absolute(&context.cwd, OperatorPath::typed(file));
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

/// Checks that `boundary` admits `target`'s directory and `target` itself, writing nothing, so a
/// refusal comes before any preview or write.
///
/// Effect: reads file metadata and symlinks along `target`.
///
/// # Errors
///
/// The first [`Escape`] `boundary` reports, for the directory or the file.
fn admit_copy(boundary: &Boundary, target: &AbsolutePath) -> Result<(), Escape> {
    if let Some(parent) = target.parent() {
        let _dir = boundary.admit(&parent)?;
    }
    boundary.admit(target).map(drop)
}

/// Why [`install_file`] wrote nothing.
#[derive(Debug)]
enum NotInstalled {
    /// The boundary refused the copy's directory or the copy.
    Outside(Escape),
    /// A directory couldn't be created or the file couldn't be written.
    Io(io::Error),
}

impl From<io::Error> for NotInstalled {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Writes `bytes` to `target`, creating its directories, through the shared temp-and-rename
/// writer, each admitted against `boundary`, the directory the copy belongs beneath, immediately
/// before it is made. An existing regular file keeps its permissions; a symlink that stays inside
/// is replaced, never written through. Every copy was already admitted ([`admit_copy`]), so a
/// refusal here means a path changed since.
///
/// # Errors
///
/// [`NotInstalled::Outside`] if `boundary` refuses a directory or the file, and
/// [`NotInstalled::Io`] if a directory cannot be created or the file cannot be written; `target`
/// is then unchanged.
fn install_file(
    boundary: &Boundary,
    target: &AbsolutePath,
    bytes: &[u8],
) -> Result<(), NotInstalled> {
    if let Some(parent) = target.parent() {
        let dir = boundary.admit(&parent).map_err(NotInstalled::Outside)?;
        confine::create_dir_all(&dir, 0o777)?;
    }
    let admitted = boundary.admit(target).map_err(NotInstalled::Outside)?;
    let permissions = fs::symlink_metadata(admitted.as_path())
        .ok()
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.permissions());
    Ok(write_atomically(&admitted, bytes, permissions.as_ref())?)
}

/// The directory a copy must stay inside, for refusals.
struct Place {
    /// The project root, or the copy's own directory for `--global`.
    owner: AbsolutePath,
    /// Whether this is a `--global` install.
    global: bool,
}

impl Place {
    /// The directory as a refusal names it.
    fn shown(&self) -> String {
        if self.global {
            self.owner.display().to_string()
        } else {
            "the project root".to_owned()
        }
    }
}

/// The `cli-skill-outside-root` refusal for the copy at `target`, which `place`'s boundary
/// refused with `escape`. A `SKILL.md` that is itself a symlink leading out is named with where it
/// leads and help to remove the link. Anything else passed the checks before it and changed
/// since, so it says that. `written` names the copies already written, so the help says nothing
/// was written only when that is true.
fn copy_outside(
    place: &Place,
    base: &AbsolutePath,
    target: &AbsolutePath,
    escape: &Escape,
    context: &Context,
    written: &[String],
) -> CliError {
    let so_far = already_written(written);
    let shown = relative_or_absolute(base, target);
    let inside = place.shown();
    let flags: &[&str] = if place.global {
        &["--global", "--yes"]
    } else {
        &["--yes"]
    };
    let rerun = command_line::<&str>(context, "skill install", &[], flags);
    let leaf_link = fs::symlink_metadata(target.as_path())
        .is_ok_and(|metadata| metadata.file_type().is_symlink());
    let (message, help) = match escape {
        Escape::Real { real } if leaf_link => {
            let typed = if place.global {
                shell_quote(&target.display().to_string())
            } else {
                shell_quote(&relative_to(&context.cwd, target))
            };
            let leads = real.as_deref().map_or_else(
                || "somewhere that can't be worked out".to_owned(),
                |real| visible(&real.display().to_string()),
            );
            (
                format!("{shown} is a symlink to {leads}, outside {inside}"),
                format!(
                    "{so_far} supdoc installs the skill only as a real file inside \
                     {inside}. Run `rm {typed}` (this removes only the link, not the file it \
                     points to), then `{rerun}`"
                ),
            )
        }
        Escape::Real { .. } | Escape::Lexical => (
            format!(
                "{shown} now leads outside {inside}: a path on the way changed after supdoc \
                 checked it"
            ),
            format!("{so_far} Check what changed on the way to {shown}, then run `{rerun}` again"),
        ),
    };
    CliError::new("cli-skill-outside-root", message, help, ExitStatus::Failed)
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
                visible(&landing.display().to_string()),
                visible(&real.display().to_string())
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
fn no_home(home_var: &str) -> CliError {
    let set_it = if home_var == "USERPROFILE" {
        "set USERPROFILE=C:\\Users\\you"
    } else {
        "export HOME=/home/you"
    };
    CliError::new(
        "cli-skill-no-home",
        format!("`--global` installs into your home directory, but {home_var} is not set"),
        format!(
            "Set {home_var}, for example `{set_it}`, or drop `--global` to install into the project"
        ),
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
    written: &[String],
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
        if written.is_empty() {
            format!("Check that you can create and write files in `{shown_directory}`")
        } else {
            format!(
                "{} Check that you can create and write files in `{shown_directory}`",
                already_written(written)
            )
        },
        ExitStatus::Io,
    )
}

/// The opening of a refusal's help: that nothing was written, or which copies already were.
fn already_written(written: &[String]) -> String {
    if written.is_empty() {
        "Nothing was written.".to_owned()
    } else {
        format!("Already written: {}.", written.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use crate::cli::args::{Command, SkillCommand};
    use crate::cli::testing::{Run, Session, at, skip};
    use crate::config::CONFIG_FILE_NAME;

    use super::*;

    /// Where the skill goes beneath the shared agents base, as the tests expect to find it.
    const AGENTS_SKILL: &str = ".agents/skills/supdoc/SKILL.md";

    /// Where the skill goes beneath the project root for Claude Code.
    const PROJECT_CLAUDE_SKILL: &str = ".claude/skills/supdoc/SKILL.md";

    /// Where the skill goes beneath Claude Code's config directory.
    const CLAUDE_DIR_SKILL: &str = "skills/supdoc/SKILL.md";

    /// `base` with each `/`-separated segment of `relative` appended, so the result matches the
    /// paths the command builds on Windows as well, where a joined `a/b` keeps its `/`.
    fn segments_beneath(base: &Path, relative: &str) -> PathBuf {
        let mut joined = base.to_path_buf();
        joined.extend(relative.split('/'));
        joined
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
            segments_beneath(self.project.path(), relative)
        }

        fn home_file(&self, relative: &str) -> PathBuf {
            segments_beneath(self.home.path(), relative)
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
                home_var: "HOME",
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

    /// The text of an absolute fixture directory on this OS.
    fn text(raw: &str) -> OsString {
        crate::path::fixture(raw).as_path().as_os_str().to_owned()
    }

    #[test]
    fn a_relative_claude_config_dir_is_kept_and_refused_for_global() {
        let dirs = SkillDirs::for_platform(
            |name| match name {
                "HOME" => Some(text("/home/me")),
                _ => Some(OsString::from("rel/claude")),
            },
            false,
        );
        let root = crate::path::fixture("/work");

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
    fn the_home_directory_comes_from_userprofile_on_windows_and_home_elsewhere() {
        let vars = |name: &str| match name {
            "HOME" => Some(text("/unix/home")),
            "USERPROFILE" => Some(text("/windows/profile")),
            _ => None,
        };

        let windows = SkillDirs::for_platform(vars, true);
        let unix = SkillDirs::for_platform(vars, false);

        assert_eq!(windows.home, Some(crate::path::fixture("/windows/profile")));
        assert_eq!(windows.home_var, "USERPROFILE");
        assert_eq!(unix.home, Some(crate::path::fixture("/unix/home")));
        assert_eq!(unix.home_var, "HOME");
        let root = crate::path::fixture("/work");
        let [(_, agents), (_, claude)] =
            destinations(true, &root, &windows).expect("global destinations from USERPROFILE");
        assert_eq!(
            agents,
            crate::path::fixture("/windows/profile").beneath(&fixed::AGENTS_SKILL)
        );
        assert_eq!(
            claude,
            crate::path::fixture("/windows/profile")
                .beneath(&fixed::DEFAULT_CLAUDE_DIR)
                .beneath(&fixed::CLAUDE_DIR_SKILL)
        );
    }

    #[test]
    fn from_vars_reads_the_home_variable_this_platform_uses() {
        let dirs = SkillDirs::from_vars(|name| match name {
            "HOME" => Some(text("/unix/home")),
            "USERPROFILE" => Some(text("/windows/profile")),
            _ => None,
        });

        let (expected, variable) = if cfg!(windows) {
            ("/windows/profile", "USERPROFILE")
        } else {
            ("/unix/home", "HOME")
        };
        assert_eq!(dirs.home, Some(crate::path::fixture(expected)));
        assert_eq!(dirs.home_var, variable);
    }

    #[test]
    fn the_windows_no_home_hint_names_userprofile() {
        let root = crate::path::fixture("/work");
        let dirs = SkillDirs::for_platform(|_| None, true);

        let error = destinations(true, &root, &dirs).expect_err("--global needs a home");

        assert_eq!(error.code(), "cli-skill-no-home");
        assert!(
            error.help().contains("set USERPROFILE=C:\\Users\\you"),
            "the hint should show how to set USERPROFILE: {}",
            error.help()
        );
    }

    #[test]
    fn global_without_a_home_names_the_variable_the_platform_reads() {
        let root = crate::path::fixture("/work");
        for (windows, variable) in [(true, "USERPROFILE"), (false, "HOME")] {
            let dirs = SkillDirs::for_platform(|_| None, windows);

            let error = destinations(true, &root, &dirs).expect_err("--global needs a home");

            assert_eq!(error.code(), "cli-skill-no-home");
            assert!(
                error
                    .to_string()
                    .contains(&format!("{variable} is not set")),
                "the message should name {variable}: {error}"
            );
        }
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
    fn a_refusal_after_one_copy_was_written_names_it_and_escapes_the_link_target() {
        let world = World::new();
        let elsewhere = tempfile::tempdir().expect("an outside temp directory should be created");
        let victim = elsewhere.path().join("SKILL\u{1b}[31m.md");
        fs::write(&victim, "outside\n").expect("the outside file should be written");
        symlink(&victim, &world.project_file(PROJECT_CLAUDE_SKILL));
        let base =
            AbsolutePath::new(world.project.path()).expect("a temp directory should be absolute");
        let link = base.beneath(&fixed::PROJECT_CLAUDE_SKILL);
        let escape = Boundary::resolve(&base)
            .admit(&link)
            .expect_err("a SKILL.md symlinked out should be refused");
        let session = Session::new(base.clone(), &["skill", "install", "--yes"]);
        let place = Place {
            owner: base.clone(),
            global: false,
        };
        let written = [AGENTS_SKILL.to_owned()];
        let already = format!("Already written: {AGENTS_SKILL}.");

        let leaf = copy_outside(&place, &base, &link, &escape, &session.context, &written);
        let race = copy_outside(
            &place,
            &base,
            &base.beneath(&fixed::AGENTS_SKILL),
            &Escape::Real { real: None },
            &session.context,
            &written,
        );
        let failed = write_failed(
            &SkillChange {
                action: SkillAction::Create,
                path: link.clone(),
                shown: PROJECT_CLAUDE_SKILL.to_owned(),
            },
            false,
            &base,
            &io::Error::from(io::ErrorKind::PermissionDenied),
            &written,
        );

        for error in [&leaf, &race, &failed] {
            assert!(
                error.help().starts_with(&already) && !error.help().contains("Nothing was written"),
                "the help should name the copy already written: {}",
                error.help()
            );
        }
        assert_eq!(
            (leaf.code(), race.code()),
            ("cli-skill-outside-root", "cli-skill-outside-root")
        );
        assert!(
            leaf.message().contains("SKILL\\u{1b}[31m.md") && !leaf.message().contains('\u{1b}'),
            "the link target should be printed with its control characters escaped: {:?}",
            leaf.message()
        );
        assert!(
            race.message()
                .contains("now leads outside the project root"),
            "{}",
            race.message()
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_file_refuses_a_directory_symlinked_out_after_the_boundary_was_resolved() {
        let root = tempfile::tempdir().expect("a project directory should be created");
        let outside = tempfile::tempdir().expect("an outside directory should be created");
        let base = AbsolutePath::new(root.path()).expect("a temp directory should be absolute");
        let boundary = Boundary::resolve(&base);
        symlink(outside.path(), &root.path().join(".agents"));

        let installed = install_file(
            &boundary,
            &base.beneath(&fixed::AGENTS_SKILL),
            SKILL.as_bytes(),
        );

        assert!(
            matches!(installed, Err(NotInstalled::Outside(_))),
            "a copy a symlink leads out of the boundary should be refused: {installed:?}"
        );
        assert_eq!(
            tree(outside.path()),
            Vec::<PathBuf>::new(),
            "nothing should be created outside"
        );
    }

    /// Fails unless `run` refused the `SKILL.md` at `shown`, a symlink to `victim`, with
    /// `cli-skill-outside-root`, link-removal help naming `typed`, and no preview line.
    #[cfg(unix)]
    fn assert_refused_skill_link(run: &Run, shown: &str, typed: &str, victim: &Path) {
        let error = run
            .result
            .as_ref()
            .expect_err("a SKILL.md symlinked out should be refused");
        assert_eq!(
            (error.code(), run.exit_code()),
            ("cli-skill-outside-root", ExitStatus::Failed.code()),
            "got {error:?}"
        );
        let real = fs::canonicalize(victim).expect("the outside file should resolve");
        assert!(
            error.message().starts_with(&format!(
                "{shown} is a symlink to {}, outside ",
                real.display()
            )),
            "the message should name the link and where it leads: {}",
            error.message()
        );
        assert!(
            error.help().starts_with("Nothing was written.")
                && error.help().contains(&format!("`rm {typed}`")),
            "the help should say nothing was written and how to remove the link: {}",
            error.help()
        );
        assert!(
            !run.stdout.contains("replace"),
            "a refusal should never preview a replacement: {}",
            run.stdout
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_refuses_a_skill_file_symlinked_out_of_the_project_with_and_without_yes() {
        let world = World::new();
        let elsewhere = tempfile::tempdir().expect("an outside temp directory should be created");
        let victim = elsewhere.path().join("SKILL.md");
        fs::write(&victim, "outside\n").expect("the outside file should be written");
        symlink(&victim, &world.project_file(AGENTS_SKILL));

        for flags in [&[][..], &["--yes"]] {
            let run = world.install(flags);

            assert_refused_skill_link(&run, AGENTS_SKILL, AGENTS_SKILL, &victim);
        }
        assert_eq!(contents(&victim).as_deref(), Some("outside\n"));
        assert!(
            fs::symlink_metadata(world.project_file(AGENTS_SKILL))
                .expect("the link should still exist")
                .file_type()
                .is_symlink(),
            "the link should be left as it was"
        );
        assert!(
            !world.project_file(".claude").exists(),
            "a refusal should write neither copy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn skill_install_global_refuses_a_skill_file_symlinked_out_and_writes_neither_copy() {
        let world = World::new();
        let elsewhere = tempfile::tempdir().expect("an outside temp directory should be created");
        let victim = elsewhere.path().join("SKILL.md");
        fs::write(&victim, "outside\n").expect("the outside file should be written");
        let link = world.home_file(".claude/skills/supdoc/SKILL.md");
        symlink(&victim, &link);
        let shown = link.display().to_string();

        for flags in [&["--global"][..], &["--global", "--yes"]] {
            let run = world.install(flags);

            assert_refused_skill_link(&run, &shown, &shell_quote(&shown), &victim);
        }
        assert_eq!(contents(&victim).as_deref(), Some("outside\n"));
        assert!(
            !world.home_file(".agents").exists(),
            "the other copy should not be written when one is refused"
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
