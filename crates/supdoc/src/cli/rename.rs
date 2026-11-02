//! `supdoc namespace rename`: renames a namespace in the config, in `supdoc.lock` and in every
//! link that uses it (06, 06a §5.8).
//!
//! The run is `config → lockfile → plan → preview or (verify → write) → report`. [`Plan::build`]
//! reads every file and decides every edit, and writes nothing. [`Plan::apply`] first checks that
//! each file it will touch still holds what was read, refusing the whole rename if one doesn't
//! (NR8), then writes sources, `supdoc.lock` and the config in that order, each atomically,
//! stopping at the first failure (NR9). The config is last, so it names OLD until every link has
//! moved, and running the command again finishes an interrupted rename.
//!
//! Links are found the way the scan finds them, never by searching text (NR6): only the `OLD`
//! prefix inside each link's written destination changes, and a reference definition that
//! several links share is edited once.

use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::{Config, ConfigError};
use crate::exit::ExitStatus;
use crate::finding::{Finding, IndexedText, Location};
use crate::link::destination::parse_destination;
use crate::link::{LinkKey, SourcePath, Target};
use crate::lock::error::LockError;
use crate::lock::io::write_atomically;
use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
use crate::namespace::NamespaceName;
use crate::output::json::{Envelope, LocationJson};
use crate::output::paths::relative_or_absolute;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, Report, TextContext, preview_note};
use crate::path::{AbsolutePath, ProjectFile, resolve_project_file};
use crate::read::cartridge::{Languages, SourceFile};
use crate::read::error::{ReadError, ReadRole};
use crate::read::links::links;
use crate::scan::model::Scope;
use crate::scan::walk::{UnreadCause, UnreadDir, walk};

use super::args::{RenameArgs, WriteMode};
use super::project::{command_line, config_failure, invocation, load_config, lock_path, read_lock};
use super::{CliError, Context, Outcome, cause};

/// The command as users type it, for help text and the JSON `command`.
const COMMAND: &str = "namespace rename";

/// Runs `supdoc namespace rename OLD NEW`: previews every edit, or with `--yes` makes them.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), the lockfile's own error (78 when
/// invalid, 74 when unreadable), `cli-rename-unknown-namespace` when OLD isn't defined,
/// `cli-rename-target-exists` when NEW already is, `cli-rename-inherited` when OLD comes only from
/// a config this one extends, `cli-rename-unreadable-source` when a source or a directory can't
/// be read, `cli-rename-config-outside-root` when the config is a symlink out of the root, and
/// `cli-rename-file-changed` when a file changed between planning and writing (all 1),
/// `cli-rename-write` when a file can't be written, and `cli-rename-output` when the report can't
/// be written (both 74).
pub fn run(args: &RenameArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let plan = Plan::build(args, context, output)?;
    let applied = match args.confirm.mode() {
        WriteMode::Preview => false,
        WriteMode::Apply => {
            plan.apply()?;
            true
        }
    };
    output
        .report(&plan.report(applied), true)
        .map_err(|error| {
            CliError::output_failed("cli-rename-output", "the rename report", &error)
        })?;
    let apply_note = preview_note(
        args.confirm.dry_run,
        &command_line(
            context,
            "namespace rename",
            &[args.old.to_string(), args.new.to_string()],
            &["--yes"],
        ),
    );
    for note in plan.notes(applied, &apply_note) {
        output.note(&note).map_err(|error| {
            CliError::output_failed("cli-rename-output", "the rename report", &error)
        })?;
    }
    Ok(Outcome::Clean)
}

/// The command that repeats this run, `--yes` included when it applies, for helps that say to run
/// it again.
fn command_text(args: &RenameArgs, context: &Context) -> String {
    let flags: &[&str] = match args.confirm.mode() {
        WriteMode::Apply => &["--yes"],
        WriteMode::Preview => &[],
    };
    rename_command(args, context, flags)
}

/// `supdoc namespace rename OLD NEW` with `flags`, as the shell takes it from here.
fn rename_command(args: &RenameArgs, context: &Context, flags: &[&str]) -> String {
    command_line(
        context,
        COMMAND,
        &[args.old.as_str(), args.new.as_str()],
        flags,
    )
}

/// The JSON Schema of `supdoc namespace rename`'s `--json` output: the whole envelope, with `data`
/// typed as [`RenameReport`].
#[must_use]
pub fn schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, RenameReport>)
}

/// Every edit a rename makes, decided from the files as they were read.
#[derive(Debug)]
struct Plan {
    /// The namespace being renamed.
    old: NamespaceName,
    /// Its new name.
    new: NamespaceName,
    /// The command that applies this rename, for helps after a write was refused or failed.
    apply_command: String,
    /// The project root, where `supdoc.lock` sits.
    root: AbsolutePath,
    /// The key to rename in the nearest config.
    config: ConfigEdit,
    /// A config further up the `extends` chain that also defines OLD, left as it is (NR5).
    parent: Option<AbsolutePath>,
    /// Each source with a link through OLD, in path order.
    sources: Vec<SourceEdit>,
    /// The lockfile as read and the entries that move.
    lock: LockEdit,
    /// Problems met while walking the project.
    findings: Vec<Finding>,
}

impl Plan {
    /// Loads the config and the lockfile, checks the two names, and finds every config key, link
    /// and lock entry to change. Reads files and writes none.
    ///
    /// # Errors
    ///
    /// Returns the config's or the lockfile's own error, `cli-rename-unknown-namespace`,
    /// `cli-rename-target-exists`, `cli-rename-inherited`, `cli-rename-unreadable-source` when a
    /// source or a directory can't be read, `cli-rename-config-outside-root` when the config is a
    /// symlink out of the root, or `cli-rename-file-changed` when the config no longer defines OLD
    /// once it is read again.
    fn build(args: &RenameArgs, context: &Context, output: &mut Output) -> Result<Self, CliError> {
        let (old, new) = (&args.old, &args.new);
        let rerun = command_text(args, context);
        let config = load_config(context, output)?;
        let lock = read_lock(&config, &context.cwd)?;
        let nearest = defining_file(&config, old, new, context)?;
        let config_edit = ConfigEdit::read(nearest, config.root(), old, &rerun, context, output)?;
        let parent = parent_definition(&config, old, context, output)?;

        let languages = Languages::builtin().map_err(CliError::from)?;
        let walked = walk(&config, languages, &Scope::All);
        if let Some(dir) = walked.unread_dirs.first() {
            return Err(unread_dir(dir, config.root(), &rerun));
        }
        let findings = walked.findings;
        let mut sources = Vec::new();
        for (path, _cartridge) in &walked.sources {
            // The walk only returns files beneath the root whose paths are valid source paths.
            let Some(source) = path
                .strip_prefix(config.root())
                .and_then(|relative| SourcePath::from_relative(relative).ok())
            else {
                return Err(invalid_source(path, &rerun));
            };
            match SourceFile::read(path.clone(), languages) {
                Ok(file) => {
                    sources.extend(SourceEdit::find(&config, languages, source, &file, old));
                }
                Err(error) => return Err(unread_source(&source, &rerun, &error)),
            }
        }

        Ok(Self {
            lock: LockEdit::new(lock, old, new),
            old: old.clone(),
            new: new.clone(),
            apply_command: rename_command(args, context, &["--yes"]),
            root: config.root().clone(),
            config: config_edit,
            parent,
            sources,
            findings,
        })
    }

    /// Checks every file against what was read, then writes sources, the lockfile and the config,
    /// each atomically, stopping at the first failure.
    ///
    /// # Errors
    ///
    /// Returns `cli-rename-file-changed` (1) before writing anything when a file no longer holds
    /// what was read, and `cli-rename-write` (74) when a write fails, naming what was already
    /// written.
    fn apply(&self) -> Result<(), CliError> {
        self.verify()?;
        let mut written: Vec<String> = Vec::new();
        for source in &self.sources {
            let renamed =
                replace_ranges(&source.text, source.prefixes(&self.old), self.new.as_str())
                    .ok_or_else(|| self.file_changed(source.source.as_str()))?;
            replace_file(source.file.as_path(), renamed.as_bytes()).map_err(|error| {
                self.write_failed(source.source.as_str(), &cause(&error), &written)
            })?;
            written.push(source.source.to_string());
        }
        if !self.lock.moved.is_empty() {
            self.lock
                .next()
                .write(&lock_path(&self.root))
                .map_err(|error| {
                    let reason = match &error {
                        LockError::Write { error, .. } => cause(error),
                        other => other.message_from(&self.root),
                    };
                    self.write_failed(LOCK_FILE_NAME, &reason, &written)
                })?;
            written.push(LOCK_FILE_NAME.to_owned());
        }
        let renamed = replace_ranges(
            &self.config.text,
            std::iter::once(self.config.key.clone()),
            self.new.as_str(),
        )
        .ok_or_else(|| self.file_changed(&self.config.shown))?;
        replace_file(self.config.file.as_path(), renamed.as_bytes())
            .map_err(|error| self.write_failed(&self.config.shown, &cause(&error), &written))
    }

    /// Refuses the rename when any file it will write is no longer exactly as read (NR8).
    ///
    /// # Errors
    ///
    /// Returns `cli-rename-file-changed` naming the first such file.
    fn verify(&self) -> Result<(), CliError> {
        for source in &self.sources {
            if !holds(&source.file, &source.text) {
                return Err(self.file_changed(source.source.as_str()));
            }
        }
        if !self.lock.moved.is_empty() {
            let now = Lockfile::read(&lock_path(&self.root)).map(Option::unwrap_or_default);
            if now.ok().as_ref() != Some(&self.lock.read) {
                return Err(self.file_changed(LOCK_FILE_NAME));
            }
        }
        if !holds(&self.config.file, &self.config.text) {
            return Err(self.file_changed(&self.config.shown));
        }
        Ok(())
    }

    /// What the run changed, or would change: the config key, then each link, then each lock
    /// entry.
    fn report(&self, applied: bool) -> RenameReport {
        let config = RenameChange {
            kind: RenameKind::Config,
            file: self.config.shown.clone(),
            location: self.config.location.as_ref().map(LocationJson::from),
            before: ConfigEdit::line(&self.config.text, self.config.key.clone()),
            after: self.config.renamed_line(self.new.as_str()),
            path: self.config.file.clone(),
            line_only: true,
        };
        let links = self.sources.iter().flat_map(|source| {
            source.links.iter().map(|link| {
                let before = source
                    .text
                    .get(link.destination.clone())
                    .unwrap_or_default()
                    .to_owned();
                let after = format!(
                    "{}{}",
                    self.new,
                    before.get(self.old.as_str().len()..).unwrap_or_default()
                );
                RenameChange {
                    kind: RenameKind::Link,
                    file: source.source.as_str().to_owned(),
                    location: link.location.as_ref().map(LocationJson::from),
                    before,
                    after,
                    path: source.file.clone(),
                    line_only: false,
                }
            })
        });
        let lock = self.lock.moved.iter().map(|(before, after)| RenameChange {
            kind: RenameKind::Lock,
            file: LOCK_FILE_NAME.to_owned(),
            location: None,
            before: before.to_string(),
            after: after.to_string(),
            path: lock_path(&self.root),
            line_only: false,
        });
        RenameReport {
            applied,
            old: self.old.to_string(),
            new: self.new.to_string(),
            changes: std::iter::once(config).chain(links).chain(lock).collect(),
            findings: self.findings.clone(),
        }
    }

    /// The notes that close the run: the parent config left as it is, then the counts and what
    /// to do next, or that the rename is done.
    fn notes(&self, applied: bool, apply_note: &str) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(parent) = &self.parent {
            notes.push(format!(
                "{} also defines `{}` and was left as it is, so `{}` still names its directory \
                 there.",
                relative_or_absolute(&self.root, parent),
                self.old,
                self.old
            ));
        }
        if applied {
            notes.push(format!("Renamed {} to {}.", self.old, self.new));
        } else {
            let links: usize = self.sources.iter().map(|source| source.links.len()).sum();
            notes.push(format!(
                "1 config entry, {} in {}, {}. {apply_note}",
                count(links, "link", "links"),
                count(self.sources.len(), "file", "files"),
                count(self.lock.moved.len(), "lock entry", "lock entries"),
            ));
        }
        notes
    }

    /// The error for a file that no longer holds what was read.
    fn file_changed(&self, file: &str) -> CliError {
        file_changed(file, &self.apply_command)
    }

    /// The error for a file that couldn't be written, after the files in `written`.
    fn write_failed(&self, file: &str, error: &str, written: &[String]) -> CliError {
        let again = &self.apply_command;
        let help = if written.is_empty() {
            format!("Nothing was written. Make {file} writable, then run `{again}` again")
        } else {
            format!(
                "Already written: {}. The config still defines `{}`, so once {file} is writable, \
                 `{again}` finishes the rename",
                written.join(", "),
                self.old
            )
        };
        CliError::new(
            "cli-rename-write",
            format!("cannot write {file}: {error}"),
            help,
            ExitStatus::Io,
        )
    }
}

/// The nearest config file, which must be the one defining OLD, after checking both names.
///
/// # Errors
///
/// Returns `cli-rename-unknown-namespace` when OLD isn't defined (NR2), `cli-rename-target-exists`
/// when NEW is, or equals OLD (NR3), and `cli-rename-inherited` when only a config further up
/// the `extends` chain defines OLD (NR4).
fn defining_file<'c>(
    config: &'c Config,
    old: &NamespaceName,
    new: &NamespaceName,
    context: &Context,
) -> Result<&'c AbsolutePath, CliError> {
    let Some(defined_in) = config.namespace_defined_in(old) else {
        return Err(unknown_namespace(config, old, new));
    };
    if old == new || config.namespace(new).is_some() {
        return Err(target_exists(config, old, new, context));
    }
    match config.files().first() {
        Some(nearest) if nearest == defined_in => Ok(nearest),
        _ => Err(inherited(old, defined_in)),
    }
}

/// The first config further up the `extends` chain that also defines `name`.
///
/// # Errors
///
/// Returns the config's own read error when one of those files can't be read.
fn parent_definition(
    config: &Config,
    name: &NamespaceName,
    context: &Context,
    output: &mut Output,
) -> Result<Option<AbsolutePath>, CliError> {
    for file in config.files().iter().skip(1) {
        let text = read_config(file, context, output)?;
        if namespace_key(&text, name).is_some() {
            return Ok(Some(file.clone()));
        }
    }
    Ok(None)
}

/// The text of the config file `file`.
///
/// # Errors
///
/// Returns the config's own read error (74), naming the file relative to the current directory, as
/// every config error does.
fn read_config(
    file: &AbsolutePath,
    context: &Context,
    output: &mut Output,
) -> Result<String, CliError> {
    fs::read_to_string(file.as_path()).map_err(|error| {
        let error = ConfigError::Read {
            path: file.clone(),
            error,
        };
        config_failure(&error, context, output)
    })
}

/// Where the key `name` is written under `[namespaces]` in the config `text`, whether as a
/// table, an inline table or dotted keys. `None` when it isn't there.
fn namespace_key(text: &str, name: &NamespaceName) -> Option<Range<usize>> {
    let document = toml_edit::Document::parse(text).ok()?;
    let namespaces = document.as_table().get("namespaces")?.as_table_like()?;
    let (key, _value) = namespaces.get_key_value(name.as_str())?;
    key.span()
}

/// The namespace key to rename in the nearest config.
#[derive(Debug)]
struct ConfigEdit {
    /// The file read, checked and written: the config, or the real file inside the root that a
    /// symlinked config leads to, so the link stays.
    file: AbsolutePath,
    /// The file as JSON shows it: relative to the project root.
    shown: String,
    /// Its text as read.
    text: String,
    /// Where the key is written, quotes included when it is quoted.
    key: Range<usize>,
    /// The key's place, for the report.
    location: Option<Location>,
}

impl ConfigEdit {
    /// Reads `file` and finds the key OLD in it with `toml_edit`, so comments and order stay as
    /// they are (NR10).
    ///
    /// # Errors
    ///
    /// Returns `cli-rename-config-outside-root` when `file` is a symlink whose real file is outside
    /// `root`, the config's own read error, or `cli-rename-file-changed` when the file no longer
    /// defines OLD.
    fn read(
        file: &AbsolutePath,
        root: &AbsolutePath,
        old: &NamespaceName,
        rerun: &str,
        context: &Context,
        output: &mut Output,
    ) -> Result<Self, CliError> {
        let shown = relative_or_absolute(root, file);
        let real = match resolve_project_file(file) {
            Ok(ProjectFile::Inside(real)) => real,
            Ok(ProjectFile::Outside(real)) => {
                return Err(config_outside_root(&shown, real.as_ref(), rerun));
            }
            // Gone since it was loaded, or its metadata can't be read: the read says why.
            Ok(ProjectFile::Absent) | Err(_) => file.clone(),
        };
        let text = read_config(&real, context, output)?;
        let relative = file.strip_prefix(root).map(Path::to_path_buf);
        let Some(key) = namespace_key(&text, old) else {
            return Err(file_changed(&shown, rerun));
        };
        let location = relative
            .as_deref()
            .and_then(|path| SourcePath::from_relative(path).ok())
            .and_then(|source| IndexedText::new(&text).locate(source, key.clone()));
        Ok(Self {
            file: real,
            shown,
            text,
            key,
            location,
        })
    }

    /// The line of `text` holding `range`, trimmed.
    fn line(text: &str, range: Range<usize>) -> String {
        let start = text
            .get(..range.start)
            .and_then(|before| before.rfind('\n'))
            .map_or(0, |newline| newline + 1);
        let end = text
            .get(range.end..)
            .and_then(|after| after.find('\n'))
            .map_or(text.len(), |newline| range.end + newline);
        text.get(start..end).unwrap_or_default().trim().to_owned()
    }

    /// The key's line with the key renamed to `new`.
    fn renamed_line(&self, new: &str) -> String {
        let Some(renamed) = replace_ranges(&self.text, std::iter::once(self.key.clone()), new)
        else {
            return Self::line(&self.text, self.key.clone());
        };
        let start = self.key.start;
        Self::line(&renamed, start..start + new.len())
    }
}

/// One source and the links in it whose destination starts with `OLD:`.
#[derive(Debug)]
struct SourceEdit {
    /// The file.
    file: AbsolutePath,
    /// The file relative to the project root.
    source: SourcePath,
    /// Its text as read.
    text: String,
    /// Each destination to rename, in file order, each once.
    links: Vec<LinkEdit>,
}

/// One written destination to rename.
#[derive(Debug)]
struct LinkEdit {
    /// The destination's bytes in the file, without any `<…>`.
    destination: Range<usize>,
    /// The destination's place, for the report.
    location: Option<Location>,
}

impl SourceEdit {
    /// The links in `file` that supdoc reads and whose destination is written with the prefix
    /// `old:`, or `None` when there are none (NR6). Several uses of one reference definition
    /// share its range and are renamed once.
    fn find(
        config: &Config,
        languages: &Languages,
        source: SourcePath,
        file: &SourceFile,
        old: &NamespaceName,
    ) -> Option<Self> {
        let text = file.text();
        let prefix = format!("{old}:");
        let mut ranges: Vec<Range<usize>> = links(file)
            .into_iter()
            .filter(|raw| parse_destination(raw, &source, config.namespaces(), languages).is_some())
            .map(|raw| raw.destination_range)
            .filter(|range| {
                text.get(range.clone())
                    .is_some_and(|written| written.starts_with(&prefix))
            })
            .collect();
        ranges.sort_by_key(|range| (range.start, range.end));
        ranges.dedup();
        if ranges.is_empty() {
            return None;
        }
        let indexed = IndexedText::new(text);
        let links = ranges
            .into_iter()
            .map(|destination| LinkEdit {
                location: indexed.locate(source.clone(), destination.clone()),
                destination,
            })
            .collect();
        Some(Self {
            file: file.path().clone(),
            source,
            text: text.to_owned(),
            links,
        })
    }

    /// The bytes of the `old` prefix in each destination, in file order.
    fn prefixes(&self, old: &NamespaceName) -> impl Iterator<Item = Range<usize>> {
        let length = old.as_str().len();
        self.links
            .iter()
            .map(move |link| link.destination.start..link.destination.start + length)
    }
}

/// The lockfile as read, and every entry whose target is in OLD with the key it moves to (NR7).
#[derive(Debug)]
struct LockEdit {
    /// The lockfile as read; empty when there is none.
    read: Lockfile,
    /// Each entry that moves, old key first, in lock order.
    moved: Vec<(LinkKey, LinkKey)>,
}

impl LockEdit {
    /// Finds the entries of `read` that point into `old`.
    fn new(read: Lockfile, old: &NamespaceName, new: &NamespaceName) -> Self {
        let moved = read
            .iter()
            .filter_map(|(key, _hash)| match &key.target {
                Target::Namespaced { namespace, path } if namespace == old => {
                    let target = Target::Namespaced {
                        namespace: new.clone(),
                        path: path.clone(),
                    };
                    Some((
                        key.clone(),
                        LinkKey {
                            target,
                            ..key.clone()
                        },
                    ))
                }
                _ => None,
            })
            .collect();
        Self { read, moved }
    }

    /// The lockfile with every moved entry under its new key, its hash kept.
    fn next(&self) -> Lockfile {
        let mut next = self.read.clone();
        for (old, new) in &self.moved {
            if let Some(hash) = next.remove(old) {
                let _replaced = next.insert(new.clone(), hash);
            }
        }
        next
    }
}

/// `text` with every range in `ranges`, which are in order and don't overlap, replaced by
/// `replacement`, or `None` if a range doesn't lie inside `text` on character boundaries.
fn replace_ranges(
    text: &str,
    ranges: impl IntoIterator<Item = Range<usize>>,
    replacement: &str,
) -> Option<String> {
    let mut renamed = String::with_capacity(text.len());
    let mut at = 0;
    for range in ranges {
        renamed.push_str(text.get(at..range.start)?);
        renamed.push_str(replacement);
        at = range.end;
    }
    renamed.push_str(text.get(at..)?);
    Some(renamed)
}

/// Whether `file` is still a regular file holding exactly `text`.
fn holds(file: &AbsolutePath, text: &str) -> bool {
    fs::symlink_metadata(file.as_path()).is_ok_and(|metadata| metadata.is_file())
        && fs::read(file.as_path()).is_ok_and(|bytes| bytes == text.as_bytes())
}

/// Writes `bytes` over `target` atomically, keeping `target`'s permissions.
///
/// # Errors
///
/// Fails if `target` can't be inspected, or the replacement can't be written; `target` is then
/// unchanged.
fn replace_file(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let permissions = fs::metadata(target)?.permissions();
    write_atomically(target, bytes, Some(&permissions))
}

/// The `cli-rename-file-changed` error (1) for `file`, shown relative to the project root, which
/// no longer holds what the rename read; `apply_command` reruns the rename. A rename meets it only
/// when a file changes between planning and writing, so a test builds it here.
#[must_use]
pub fn file_changed(file: &str, apply_command: &str) -> CliError {
    CliError::new(
        "cli-rename-file-changed",
        format!(
            "{file} changed or can't be read since the rename was planned; nothing was written"
        ),
        format!("Run `{apply_command}` again to plan from the files as they are now"),
        ExitStatus::Failed,
    )
}

/// The `cli-rename-config-outside-root` error (1) for the config `shown`, a symlink whose real
/// file `real` lies outside the project root, where a rename never writes; `rerun` repeats it.
fn config_outside_root(shown: &str, real: Option<&AbsolutePath>, rerun: &str) -> CliError {
    let Some(real) = real else {
        return CliError::new(
            "cli-rename-config-outside-root",
            format!("{shown} is a symlink that doesn't resolve to a file; nothing was written"),
            format!(
                "Point the symlink at a file inside the project root, then run `{rerun}` again"
            ),
            ExitStatus::Failed,
        );
    };
    CliError::new(
        "cli-rename-config-outside-root",
        format!(
            "{shown} is a symlink to {}, outside the project root; nothing was written",
            real.display()
        ),
        format!(
            "supdoc writes only inside the project root. Rename the namespace in {} yourself, or \
             replace the symlink with a copy of that file and run `{rerun}` again",
            real.display()
        ),
        ExitStatus::Failed,
    )
}

/// The error for an OLD that no config defines, naming the likely intended one and every
/// configured namespace (NR2, Q18). When NEW is already defined it says the rename looks done.
fn unknown_namespace(config: &Config, old: &NamespaceName, new: &NamespaceName) -> CliError {
    let names: Vec<&str> = config
        .namespaces()
        .keys()
        .map(NamespaceName::as_str)
        .collect();
    let help = crate::namespace::unknown_namespace_help(old.as_str(), &names);
    let message = if config.namespace(new).is_some() {
        format!("no namespace is named `{old}`, but `{new}` is defined: the rename looks done")
    } else {
        format!("no namespace is named `{old}`")
    };
    CliError::new(
        "cli-rename-unknown-namespace",
        message,
        help,
        ExitStatus::Failed,
    )
}

/// The error for a NEW that is already a namespace, or is OLD itself (NR3).
fn target_exists(
    config: &Config,
    old: &NamespaceName,
    new: &NamespaceName,
    context: &Context,
) -> CliError {
    let message = match config.namespace_defined_in(new) {
        _ if old == new => format!("`{new}` is already the namespace's name"),
        Some(file) => format!(
            "a namespace named `{new}` already exists, defined in {}",
            relative_or_absolute(config.root(), file)
        ),
        None => format!("a namespace named `{new}` already exists"),
    };
    CliError::new(
        "cli-rename-target-exists",
        message,
        format!(
            "Run `{}` to see the names in use, and choose a NEW name none of them has. Nothing \
             was changed",
            invocation(context, "namespace list")
        ),
        ExitStatus::Failed,
    )
}

/// The error for an OLD that only a config further up the `extends` chain defines (NR4, Q19).
fn inherited(old: &NamespaceName, defined_in: &AbsolutePath) -> CliError {
    CliError::new(
        "cli-rename-inherited",
        format!(
            "`{old}` is defined in {}, a config this project extends",
            defined_in.display()
        ),
        format!(
            "Rename it from the project that owns `{}`; renaming it here would change projects \
             that weren't scanned. Nothing was changed",
            defined_in.display()
        ),
        ExitStatus::Failed,
    )
}

/// The error for a walked source that couldn't be read. Its links can't be checked for OLD, so
/// the whole rename is refused rather than moving lock entries past a file left unrenamed.
fn unread_source(source: &SourcePath, rerun: &str, error: &ReadError) -> CliError {
    CliError::new(
        "cli-rename-unreadable-source",
        format!(
            "{}; its links can't be renamed, so nothing was written",
            error.message_naming(source)
        ),
        then_rerun(
            &error.help(ReadRole::RenamedSource, &source.to_string()),
            rerun,
        ),
        ExitStatus::Failed,
    )
}

/// The error for a walked file whose name isn't a valid source path. The walk reports such a file
/// as a finding instead of returning it, so this guards the invariant rather than skipping.
fn invalid_source(path: &AbsolutePath, rerun: &str) -> CliError {
    CliError::new(
        "cli-rename-invalid-source-path",
        format!(
            "{} has a name that isn't a valid source path; its links can't be renamed, so \
             nothing was written",
            path.display()
        ),
        then_rerun("Rename the file", rerun),
        ExitStatus::Failed,
    )
}

/// The error for a directory the walk couldn't read. The sources beneath it are as unknown as an
/// unread source's links, so the rename is refused for the same reason (05 C5). As for an unread
/// source, the help never offers `exclude`.
fn unread_dir(dir: &UnreadDir, root: &AbsolutePath, rerun: &str) -> CliError {
    let shown = dir
        .path
        .strip_prefix(root)
        .and_then(|relative| SourcePath::from_relative(relative).ok())
        .map_or_else(|| dir.path.display().to_string(), |dir| dir.to_string());
    CliError::new(
        "cli-rename-unreadable-source",
        format!(
            "cannot read the directory {shown}: {}; the links beneath it can't be renamed, so \
             nothing was written",
            dir.cause
        ),
        then_rerun(&unread_dir_help(dir.cause), rerun),
        ExitStatus::Failed,
    )
}

/// What to do about a directory the rename couldn't read, before running it again.
fn unread_dir_help(cause: UnreadCause) -> String {
    match cause {
        UnreadCause::Io(_) => "Check that the current user can list the directory, for example \
                               with `chmod u+rx` on it"
            .to_owned(),
    }
}

/// `help`, followed by the sentence that says to run the rename again once it is followed.
fn then_rerun(help: &str, rerun: &str) -> String {
    let end = if help.ends_with(['.', '?', '!']) {
        ""
    } else {
        "."
    };
    format!("{help}{end} Then run `{rerun}` again")
}

/// `1 link`, `2 links`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What `supdoc namespace rename` changed, or would change.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RenameReport {
    /// `false` for a preview, which wrote nothing.
    applied: bool,
    /// The namespace's old name.
    old: String,
    /// Its new name.
    new: String,
    /// Every edit: the config key, then each link, then each lock entry.
    changes: Vec<RenameChange>,
    /// Problems met while reading sources.
    #[serde(skip)]
    findings: Vec<Finding>,
}

/// One edit a rename makes.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RenameChange {
    /// What is edited.
    kind: RenameKind,
    /// The file, relative to the project root.
    file: String,
    /// Where in the file: the key or the destination. Left out for lock entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<LocationJson>,
    /// The config line, destination or lock entry before.
    before: String,
    /// The same after.
    after: String,
    /// The file, for text output.
    #[serde(skip)]
    path: AbsolutePath,
    /// Whether text output shows only the line, not the column.
    #[serde(skip)]
    line_only: bool,
}

/// What kind of thing a [`RenameChange`] edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RenameKind {
    /// The namespace's key in the config.
    Config,
    /// A link's destination in a source.
    Link,
    /// An entry in `supdoc.lock`.
    Lock,
}

impl RenameKind {
    /// The word text output starts the line with.
    const fn word(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Link => "link",
            Self::Lock => "lock",
        }
    }
}

impl Report for RenameReport {
    fn command(&self) -> &'static str {
        COMMAND
    }

    fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// `config  supdoc.config.toml:4  auth = "services/auth"  ->  identity = "services/auth"`,
    /// one `link` line per destination, and one `lock  supdoc.lock  3 entries` line.
    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let arrow = if context.unicode { "→" } else { "->" };
        for change in self
            .changes
            .iter()
            .filter(|change| change.kind != RenameKind::Lock)
        {
            let mut place = context.paths.text_absolute(&change.path);
            if let Some(location) = &change.location {
                place = if change.line_only {
                    format!("{place}:{}", location.line)
                } else {
                    format!("{place}:{}:{}", location.line, location.column)
                };
            }
            writeln!(
                out,
                "{}  {}  {}  {arrow}  {}",
                change.kind.word(),
                visible(&place),
                visible(&change.before),
                visible(&change.after),
            )?;
        }
        let lock: Vec<&RenameChange> = self
            .changes
            .iter()
            .filter(|change| change.kind == RenameKind::Lock)
            .collect();
        if let Some(first) = lock.first() {
            writeln!(
                out,
                "lock  {}  {}",
                visible(&context.paths.text_absolute(&first.path)),
                count(lock.len(), "entry", "entries"),
            )?;
        }
        for finding in &self.findings {
            render_finding(context, finding, out)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::check::reconcile::reconcile_at_root;
    use crate::cli::args::{Command, NamespaceCommand};
    use crate::cli::testing::{Project, Run, Session, at};
    use crate::config::CONFIG_FILE_NAME;
    use crate::state::LinkState;

    /// A parent config whose namespace lies inside `app/`, so `app/` may inherit it.
    const INHERITED_CONFIG: &str = "[namespaces]\nauth = \"app/services/auth\"\n";

    /// A config naming two namespaces, with comments that a rename must keep.
    const CONFIG: &str = "\
# Shared code lives beside the docs.
[namespaces]
# The sign-in service.
auth = \"services/auth\"   # moved here in 2024
web = \"services/web\"
";

    /// A guide with one link into each namespace.
    const GUIDE: &str = "# Guide\n\nSee [login](auth:login.rs#login) and [app](web:app.rs#app).\n";

    #[test]
    fn config_read_error_names_the_config_from_the_current_directory() {
        let dir = tempfile::tempdir().expect("a fresh temp directory should be created");
        let root = AbsolutePath::new(dir.path()).expect("a temp directory should be absolute");
        let docs = AbsolutePath::new(&dir.path().join("docs"))
            .expect("a temp directory should be absolute");
        let file = AbsolutePath::new(&dir.path().join("base.toml"))
            .expect("a temp file path should be absolute");

        let mut session = Session::new(docs, &["namespace", "rename", "auth", "identity"]);
        let error = read_config(&file, &session.context, &mut session.output)
            .expect_err("a missing config should not be read");

        assert_eq!(error.code(), "config-read");
        assert!(
            error.message().starts_with("cannot read ../base.toml: "),
            "the message should name the config from the current directory: {}",
            error.message()
        );
        assert!(
            !error.message().contains(&root.display().to_string()),
            "the message should not name the config absolutely: {}",
            error.message()
        );
    }

    impl Run {
        fn rename_error(&self) -> &CliError {
            self.result
                .as_ref()
                .err()
                .unwrap_or_else(|| panic!("the rename should fail; stdout: {}", self.stdout))
        }
    }

    /// A project in a fresh temporary directory: the config, both namespaces' code and the guide.
    fn project() -> Project {
        let project = Project::empty();
        project.write(CONFIG_FILE_NAME, CONFIG);
        project.write(
            "services/auth/login.rs",
            "pub fn login() {}\n\npub fn logout() {}\n",
        );
        project.write("services/web/app.rs", "pub fn app() {}\n");
        project.write("docs/guide.md", GUIDE);
        project
    }

    impl Project {
        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.dir.path().join(relative))
                .unwrap_or_else(|error| panic!("{relative} should be readable: {error}"))
        }

        fn lockfile(&self) -> Lockfile {
            Lockfile::read(&lock_path(&self.root()))
                .expect("the lockfile should parse")
                .expect("the lockfile should exist")
        }

        /// The project's config, loaded as `supdoc` loads it from the root.
        fn config(&self) -> Config {
            Config::load(&self.root()).expect("the fixture config should load")
        }

        /// Each link's source, target, symbol and state, as `supdoc check` would judge them.
        fn states(&self) -> Vec<(String, String, LinkState)> {
            let scan = crate::scan::assemble::run(
                &self.config(),
                Languages::builtin().expect("the built-in cartridges should load"),
                &Scope::All,
            )
            .expect("the fixture project should scan");
            let lock = Lockfile::read(&lock_path(&self.root()))
                .expect("the lockfile should parse")
                .unwrap_or_default();
            reconcile_at_root(&scan, &lock, &Scope::All)
                .links
                .into_iter()
                .map(|link| {
                    let symbol = link.key.symbol.map(|symbol| symbol.to_string());
                    let target = format!("{}#{}", link.key.target, symbol.unwrap_or_default());
                    (link.key.source.to_string(), target, link.state)
                })
                .collect()
        }

        /// Runs `supdoc <args>` from `relative` under the root. `args` must name the rename.
        fn rename_from(&self, relative: &str, args: &[&str]) -> Run {
            let mut session = Session::new(self.root_of(relative), args);
            let Command::Namespace(NamespaceCommand::Rename(rename)) = &session.cli.command else {
                panic!("`{}` should be a namespace rename", args.join(" "));
            };
            let result = run(rename, &session.context, &mut session.output);
            session.finished(result)
        }

        /// Runs `supdoc <args>` from the project root.
        fn rename(&self, args: &[&str]) -> Run {
            self.rename_from("", args)
        }

        /// The plan `supdoc namespace rename OLD NEW` makes from the root, without running it.
        fn plan(&self, old: &str, new: &str) -> Plan {
            let args = ["namespace", "rename", old, new];
            let mut session = Session::new(self.root(), &args);
            let Command::Namespace(NamespaceCommand::Rename(rename)) = &session.cli.command else {
                panic!("`{}` should be a namespace rename", args.join(" "));
            };
            Plan::build(rename, &session.context, &mut session.output)
                .expect("the rename should plan")
        }
    }

    #[test]
    fn namespace_rename_yes_updates_config_lock_and_links() {
        let project = project();
        project.record_all();
        let entries = |lock: &Lockfile| -> BTreeMap<String, String> {
            lock.iter()
                .map(|(key, hash)| (key.to_string(), hash.to_string()))
                .collect()
        };
        let expected: BTreeMap<String, String> = entries(&project.lockfile())
            .into_iter()
            .map(|(key, hash)| (key.replace("→ auth:", "→ identity:"), hash))
            .collect();

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            project.read(CONFIG_FILE_NAME),
            CONFIG.replace("\nauth = ", "\nidentity = "),
            "only the key is renamed in the config"
        );
        assert_eq!(
            project.read("docs/guide.md"),
            GUIDE.replace("(auth:", "(identity:"),
            "the auth link moves and the web link stays"
        );
        assert_eq!(
            entries(&project.lockfile()),
            expected,
            "lock entries move to identity with their hashes"
        );
        assert!(
            run.stderr.contains("Renamed auth to identity."),
            "stderr: {}",
            run.stderr
        );
    }

    #[test]
    fn namespace_rename_preserves_config_comments() {
        let project = project();
        let config = "\
# Top comment.
include = [\"docs\"]   # only docs

[namespaces]
# Before web.
web = \"services/web\"
# Before auth.
\"auth\" = \"services/auth\" # trailing
# After auth.
";
        project.write(CONFIG_FILE_NAME, config);

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            project.read(CONFIG_FILE_NAME),
            config.replace("\"auth\" = ", "identity = "),
            "comments, order and every other byte stay as written"
        );
    }

    #[test]
    fn namespace_rename_rewrites_a_reference_definition_once_for_many_usages() {
        let project = project();
        let doc = "See [sign in][login] or [log in][login].\n\n[login]: auth:login.rs#login\n";
        project.write("docs/guide.md", doc);

        let run = project.rename(&["--json", "namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            project.read("docs/guide.md"),
            doc.replace(": auth:", ": identity:")
        );
        let links: Vec<serde_json::Value> = at(&run.json(), "/data/changes")
            .as_array()
            .expect("changes should be an array")
            .iter()
            .filter(|change| change.get("kind").is_some_and(|kind| kind == "link"))
            .cloned()
            .collect();
        assert_eq!(
            links.len(),
            1,
            "one definition is one change, however many links use it: {links:?}"
        );
    }

    #[test]
    fn namespace_rename_rewrites_a_link_in_a_joined_line_comment() {
        let project = project();
        let code = "/// Signs in through the [login\n/// handler](auth:login.rs#login).\npub fn sign_in() {}\n";
        project.write("src/lib.rs", code);

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            project.read("src/lib.rs"),
            code.replace("(auth:", "(identity:"),
            "the destination is edited in file coordinates, across the joined comment lines"
        );
    }

    #[test]
    fn namespace_rename_then_check_reports_the_same_states() {
        let project = project();
        project.write(
            "docs/more.md",
            "[logout](auth:login.rs#logout) and [all](auth:login.rs)\n",
        );
        project.record_all();
        // One recorded link now points at changed code, so it is failed, not passed.
        project.write(
            "services/auth/login.rs",
            "pub fn login() {}\n\npub fn logout() { todo!() }\n",
        );
        project.write("docs/new.md", "[unrecorded](auth:login.rs#login)\n");
        let before: Vec<(String, String, LinkState)> = project
            .states()
            .into_iter()
            .map(|(source, target, state)| (source, target.replace("auth:", "identity:"), state))
            .collect();

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert!(
            before
                .iter()
                .any(|(_, _, state)| *state == LinkState::Failed)
                && before.iter().any(|(_, _, state)| *state == LinkState::New),
            "the fixture should cover more than passed links: {before:?}"
        );
        assert_eq!(project.states(), before);
    }

    #[test]
    fn namespace_rename_preview_writes_nothing() {
        let project = project();
        project.record_all();
        let before = project.files();

        let text = project.rename(&["namespace", "rename", "auth", "identity"]);
        let dry = project.rename(&[
            "--json",
            "namespace",
            "rename",
            "auth",
            "identity",
            "--yes",
            "--dry-run",
        ]);

        assert_eq!(project.files(), before, "a preview changes no file");
        assert_eq!((text.exit_code(), dry.exit_code()), (0, 0));
        assert_eq!(at(&dry.json(), "/data/applied"), false);
        assert_eq!(
            text.stdout,
            "config  supdoc.config.toml:4  auth = \"services/auth\"   # moved here in 2024  ->  \
             identity = \"services/auth\"   # moved here in 2024\n\
             link  docs/guide.md:3:13  auth:login.rs#login  ->  identity:login.rs#login\n\
             lock  supdoc.lock  1 entry\n"
        );
        assert_eq!(
            text.stderr,
            "1 config entry, 1 link in 1 file, 1 lock entry. Nothing written. Run \
             `supdoc namespace rename auth identity --yes` to apply.\n"
        );
    }

    #[test]
    fn namespace_rename_of_a_missing_old_with_a_defined_new_says_it_looks_done() {
        let project = project();

        let run = project.rename(&["namespace", "rename", "gone", "web", "--yes"]);

        assert_eq!(run.exit_code(), 1);
        let error = run.rename_error();
        assert_eq!(error.code(), "cli-rename-unknown-namespace");
        assert!(
            error.message().contains("`web` is defined"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn namespace_rename_unknown_old_exits_1_listing_names() {
        let project = project();
        let before = project.files();

        let run = project.rename(&["namespace", "rename", "atuh", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 1);
        let error = run.rename_error();
        assert_eq!(error.code(), "cli-rename-unknown-namespace");
        assert!(error.message().contains("`atuh`"), "{}", error.message());
        assert_eq!(
            error.help(),
            "did you mean `auth`? Configured namespaces: `auth`, `web`"
        );
        assert_eq!(project.files(), before);
    }

    #[test]
    fn namespace_rename_existing_new_exits_1() {
        let project = project();
        let before = project.files();

        let taken = project.rename(&["namespace", "rename", "auth", "web", "--yes"]);
        let same = project.rename(&["namespace", "rename", "auth", "auth", "--yes"]);

        for (name, run) in [("taken", &taken), ("same", &same)] {
            assert_eq!(run.exit_code(), 1, "{name}");
            assert_eq!(
                run.rename_error().code(),
                "cli-rename-target-exists",
                "{name}"
            );
        }
        assert!(
            taken.rename_error().message().contains("`web`"),
            "{}",
            taken.rename_error().message()
        );
        assert_eq!(project.files(), before);
    }

    #[test]
    fn namespace_rename_existing_new_names_the_defining_config_from_the_root() {
        let project = project();

        let run = project.rename_from("docs", &["namespace", "rename", "auth", "web", "--yes"]);

        assert_eq!(
            run.rename_error().message(),
            "a namespace named `web` already exists, defined in supdoc.config.toml",
            "the project's own config is named relative to the root, not absolutely"
        );
    }

    #[test]
    fn namespace_rename_inherited_namespace_is_refused_naming_its_file() {
        let project = project();
        // A namespace is inherited only when it names a directory inside the child's root.
        project.write(CONFIG_FILE_NAME, INHERITED_CONFIG);
        project.write(
            "app/supdoc.config.toml",
            "extends = \"../supdoc.config.toml\"\n",
        );
        project.write("app/docs/a.md", "[login](auth:login.rs#login)\n");
        let before = project.files();

        let run = project.rename_from("app", &["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 1);
        let error = run.rename_error();
        assert_eq!(error.code(), "cli-rename-inherited");
        let parent = project.root().as_path().join(CONFIG_FILE_NAME);
        assert!(
            error.message().contains(&parent.display().to_string()),
            "the error names the file that defines it: {}",
            error.message()
        );
        assert_eq!(project.files(), before);
    }

    #[test]
    fn namespace_rename_of_a_name_also_in_a_parent_renames_only_the_local_one() {
        let project = project();
        project.write(CONFIG_FILE_NAME, INHERITED_CONFIG);
        let child =
            "extends = \"../supdoc.config.toml\"\n\n[namespaces]\nauth = \"services/auth\"\n";
        project.write("app/supdoc.config.toml", child);

        let run = project.rename_from("app", &["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            project.read("app/supdoc.config.toml"),
            child.replace("\nauth = ", "\nidentity = ")
        );
        assert_eq!(
            project.read(CONFIG_FILE_NAME),
            INHERITED_CONFIG,
            "the parent is left as it is"
        );
        assert!(
            run.stderr
                .contains("also defines `auth` and was left as it is"),
            "stderr: {}",
            run.stderr
        );
    }

    #[test]
    fn namespace_rename_leaves_code_spans_and_other_prefixes_alone() {
        let project = project();
        let doc = "\
Write `[x](auth:login.rs)` to link, or auth:login.rs in prose.

```md
[fenced](auth:login.rs#login)
```

[web](web:app.rs) [typo](authx:login.rs) [real](auth:login.rs#login)
";
        project.write("docs/guide.md", doc);

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            project.read("docs/guide.md"),
            doc.replace("[real](auth:", "[real](identity:"),
            "only the real link through auth changes"
        );
    }

    #[test]
    fn namespace_rename_refuses_when_a_source_changed_after_scan() {
        let project = project();
        project.record_all();
        let plan = project.plan("auth", "identity");
        let edited = format!("{GUIDE}One more line.\n");
        project.write("docs/guide.md", &edited);
        let before = project.files();

        let error = plan
            .apply()
            .expect_err("a source changed since it was read should refuse the rename");

        assert_eq!(
            (error.code(), error.exit_code()),
            ("cli-rename-file-changed", 1)
        );
        assert!(
            error.message().contains("docs/guide.md"),
            "{}",
            error.message()
        );
        assert_eq!(
            project.files(),
            before,
            "nothing is written, the edit is kept"
        );
    }

    #[test]
    fn namespace_rename_refuses_when_the_lockfile_changed_after_scan() {
        let project = project();
        project.record_all();
        let plan = project.plan("auth", "identity");
        let mut lock = project.lockfile();
        lock.retain(|_key, _hash| false);
        lock.write(&lock_path(&project.root()))
            .expect("the edited lockfile should be written");
        let before = project.files();

        let error = plan
            .apply()
            .expect_err("a lockfile changed since it was read should refuse the rename");

        assert_eq!(
            (error.code(), error.exit_code()),
            ("cli-rename-file-changed", 1)
        );
        assert!(
            error.message().contains("supdoc.lock"),
            "{}",
            error.message()
        );
        assert!(
            error
                .help()
                .contains("`supdoc namespace rename auth identity --yes`"),
            "the help should quote the command to run again: {}",
            error.help()
        );
        assert_eq!(
            project.files(),
            before,
            "nothing is written, the edit is kept"
        );
    }

    #[test]
    fn namespace_rename_refuses_when_the_config_changed_after_scan() {
        let project = project();
        project.record_all();
        let plan = project.plan("auth", "identity");
        let config_text = project.read(CONFIG_FILE_NAME);
        project.write(CONFIG_FILE_NAME, &format!("# edited\n{config_text}"));
        let before = project.files();

        let error = plan
            .apply()
            .expect_err("a config changed since it was read should refuse the rename");

        assert_eq!(
            (error.code(), error.exit_code()),
            ("cli-rename-file-changed", 1)
        );
        assert!(
            error.message().contains(CONFIG_FILE_NAME),
            "{}",
            error.message()
        );
        assert_eq!(
            project.files(),
            before,
            "nothing is written, the edit is kept"
        );
    }

    #[test]
    fn rename_help_command_keeps_config_and_yes_and_reruns_from_another_directory() {
        let project = project();
        project.record_all();
        std::fs::write(
            project.root().as_path().join("docs/broken.md"),
            b"See [login](auth:login.rs#login).\n\xff\xfe",
        )
        .expect("the broken doc should be writable");
        let typed = [
            "-c",
            "../supdoc.config.toml",
            "namespace",
            "rename",
            "auth",
            "identity",
            "--yes",
        ];

        let run = project.rename_from("docs", &typed);

        let error = run
            .result
            .expect_err("an unreadable source should refuse the rename");
        let help = error.help();
        let printed = "supdoc -c ../supdoc.config.toml namespace rename auth identity --yes";
        assert!(
            help.contains(&format!("`{printed}`")),
            "the help should name the run with its config and --yes: {help}"
        );
        let words: Vec<&str> = printed.split(' ').skip(1).collect();
        let again = project.rename_from("docs", &words);
        assert_eq!(
            again
                .result
                .expect_err("the printed command should rerun the refusal")
                .code(),
            "cli-rename-unreadable-source",
            "the printed command should load the same config and reach the same refusal"
        );
    }

    #[test]
    fn rename_target_exists_help_lists_namespaces_with_the_config() {
        let project = project();

        let run = project.rename_from(
            "docs",
            &[
                "-c",
                "../supdoc.config.toml",
                "namespace",
                "rename",
                "auth",
                "web",
            ],
        );

        let error = run
            .result
            .expect_err("renaming onto an existing namespace should refuse");
        let help = error.help();
        assert!(
            help.contains("`supdoc -c ../supdoc.config.toml namespace list`"),
            "the help should keep -c: {help}"
        );
    }

    #[test]
    fn namespace_rename_refuses_everything_when_a_source_is_unreadable() {
        let project = project();
        project.record_all();
        std::fs::write(
            project.root().as_path().join("docs/broken.md"),
            b"See [login](auth:login.rs#login).\n\xff\xfe",
        )
        .expect("the broken doc should be writable");
        let before = project.files();

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        let error = run
            .result
            .expect_err("an unreadable source should refuse the rename");
        assert_eq!(
            (error.code(), error.exit_code()),
            ("cli-rename-unreadable-source", 1)
        );
        assert!(
            error.message().starts_with("docs/broken.md is not UTF-8"),
            "the message should name the file relative to the root: {}",
            error.message()
        );
        assert!(
            !error
                .message()
                .contains(project.root().display().to_string().as_str()),
            "the message should not name the absolute temp root: {}",
            error.message()
        );
        assert_eq!(
            error.help(),
            "Save the file as UTF-8. Then run `supdoc namespace rename auth identity --yes` again",
            "the help should offer only fixing the file, then the rerun hint as its own sentence"
        );
        assert_eq!(
            project.files(),
            before,
            "no file, lock or config change is written"
        );
    }

    /// Takes every permission away from a directory until dropped, then gives it back, so a
    /// failing test still leaves its temporary project removable.
    #[cfg(unix)]
    struct Unreadable(PathBuf);

    #[cfg(unix)]
    impl Unreadable {
        fn new(dir: PathBuf) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o000))
                .expect("the fixture directory permissions should be settable");
            Self(dir)
        }
    }

    #[cfg(unix)]
    impl Drop for Unreadable {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _restored = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    /// Makes a directory read-only until dropped, then gives write access back, so a failing
    /// test still leaves its temporary project removable.
    #[cfg(unix)]
    struct ReadOnly(PathBuf);

    #[cfg(unix)]
    impl ReadOnly {
        fn new(dir: PathBuf) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o555))
                .expect("the fixture directory permissions should be settable");
            Self(dir)
        }
    }

    #[cfg(unix)]
    impl Drop for ReadOnly {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;
            let _restored = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    #[cfg(unix)]
    #[test]
    fn lock_write_failure_names_the_lockfile_once_and_relative() {
        let project = project();
        project.record_all();
        let plan = project.plan("auth", "identity");
        let _read_only = ReadOnly::new(project.dir.path().to_path_buf());

        let error = plan
            .apply()
            .expect_err("the lockfile can't be written into a read-only root");

        assert_eq!(
            (error.code(), error.message()),
            (
                "cli-rename-write",
                "cannot write supdoc.lock: permission denied"
            ),
            "the lockfile is named once, relative to the root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn namespace_rename_refuses_everything_when_a_directory_is_unreadable() {
        let project = project();
        project.write("docs/sub/c.md", "See [login](auth:login.rs#login).\n");
        project.record_all();
        let before = project.files();

        let unreadable = Unreadable::new(project.dir.path().join("docs/sub"));
        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);
        drop(unreadable);

        let error = run
            .result
            .expect_err("an unreadable directory should refuse the rename");
        assert_eq!(
            (error.code(), error.exit_code()),
            ("cli-rename-unreadable-source", 1)
        );
        assert!(
            error
                .message()
                .starts_with("cannot read the directory docs/sub:"),
            "the message should name the directory relative to the root: {}",
            error.message()
        );
        assert_eq!(
            error.help(),
            "Check that the current user can list the directory, for example with `chmod u+rx` \
             on it. Then run `supdoc namespace rename auth identity --yes` again",
            "the help should offer only fixing the directory, then the rerun hint as its own \
             sentence"
        );
        assert_eq!(
            project.files(),
            before,
            "no source, lock or config change is written"
        );
    }

    /// Symlinks `link`, relative to the project root, to a file of rules outside it.
    #[cfg(unix)]
    fn symlink_out(project: &Project, outside: &tempfile::TempDir, link: &str) {
        let rules = outside.path().join("rules");
        fs::write(&rules, "gui*.md\n").expect("the outside rules should be written");
        std::os::unix::fs::symlink(&rules, project.dir.path().join(link))
            .expect("the fixture symlink should be created");
    }

    #[cfg(unix)]
    #[test]
    fn namespace_rename_renames_links_a_symlinked_ignore_file_would_hide_and_warns() {
        let project = project();
        let outside = tempfile::tempdir().expect("an outside directory should be created");
        symlink_out(&project, &outside, "docs/.gitignore");

        let run = project.rename(&["--json", "namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            project.read("docs/guide.md"),
            GUIDE.replace("(auth:", "(identity:"),
            "the symlinked file's `gui*.md` doesn't hide the guide, as in git"
        );
        let shown = format!("{}{}", run.stdout, run.stderr);
        assert_eq!(
            shown.matches("\"scan-ignore-file-symlink\"").count(),
            1,
            "one warning names the symlink:\n{shown}"
        );
        assert!(!shown.contains("gui*.md"), "{shown}");
    }

    /// Excluding an unread source and renaming past it moves its lock entries to NEW while its
    /// links still say OLD, so no refusal may suggest `exclude`.
    #[test]
    fn no_unread_refusal_help_suggests_excluding_the_path() {
        const RERUN: &str = "supdoc namespace rename auth identity";
        let root = AbsolutePath::new(Path::new("/project")).expect("the root should be absolute");
        let path = AbsolutePath::new(Path::new("/project/docs/a.md"))
            .expect("the source path should be absolute");
        let source = SourcePath::from_relative(Path::new("docs/a.md"))
            .expect("docs/a.md is a valid source path");
        let read_errors = [
            ReadError::Missing { path: path.clone() },
            ReadError::Io {
                path: path.clone(),
                error: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            ReadError::NotUtf8 {
                path: path.clone(),
                offset: 0,
            },
            ReadError::TooLarge {
                path: path.clone(),
                size: 2,
                limit: 1,
            },
            ReadError::UnsupportedLanguage {
                path: path.clone(),
                extension: "xyz".to_owned(),
                supported: vec!["md"],
            },
            ReadError::Internal {
                path: path.clone(),
                reason: "tree-sitter returned no tree",
            },
        ];
        let causes = [UnreadCause::Io(io::ErrorKind::PermissionDenied)];
        let dirs = causes.map(|cause| UnreadDir {
            path: AbsolutePath::new(Path::new("/project/docs"))
                .expect("the directory should be absolute"),
            cause,
        });

        let refusals = read_errors
            .iter()
            .map(|error| (error.code(), unread_source(&source, RERUN, error)))
            .chain(
                dirs.iter()
                    .map(|dir| ("unread-dir", unread_dir(dir, &root, RERUN))),
            );

        for (cause, refusal) in refusals {
            assert!(
                !refusal.help().contains("exclude"),
                "the {cause} refusal help should not suggest `exclude`: {}",
                refusal.help()
            );
        }
    }

    #[test]
    fn namespace_rename_without_a_lockfile_creates_none() {
        let project = project();

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert!(
            !project.dir.path().join(LOCK_FILE_NAME).exists(),
            "a rename never creates supdoc.lock"
        );
    }

    #[test]
    fn namespace_rename_json_validates_against_its_schema() {
        let project = project();
        project.record_all();
        let json = project
            .rename(&["--json", "namespace", "rename", "auth", "identity"])
            .json();
        let schema = serde_json::to_value(schema()).expect("the schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");

        let errors: Vec<String> = validator
            .iter_errors(&json)
            .map(|error| error.to_string())
            .collect();

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
        assert_eq!(at(&json, "/command"), "namespace rename");
        assert_eq!(
            at(&json, "/data/changes").as_array().map(Vec::len),
            Some(3),
            "one config, one link and one lock change: {json}"
        );
    }
}
