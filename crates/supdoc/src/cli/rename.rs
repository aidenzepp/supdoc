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

use crate::config::{CONFIG_FILE_NAME, Config, ConfigError};
use crate::finding::{Finding, Location};
use crate::link::destination::parse_destination;
use crate::link::{LinkKey, SourcePath, Target};
use crate::lock::io::write_atomically;
use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
use crate::namespace::NamespaceName;
use crate::output::json::{Envelope, LocationJson};
use crate::output::text::visible;
use crate::output::{Output, OutputError, Report, TextContext};
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile};
use crate::read::error::ReadError;
use crate::read::links::links;
use crate::scan::model::Scope;
use crate::scan::targets::close_match;
use crate::scan::walk::walk;

use super::args::{RenameArgs, WriteMode};
use super::project::{load_config, lock_path, read_lock};
use super::{CliError, Context, Outcome};

/// The command as users type it, for help text and the JSON `command`.
const COMMAND: &str = "namespace rename";

/// The note that closes a preview, after the counts.
const PREVIEW_NOTE: &str = "Nothing written. Run the same command with --yes to apply.";

/// Runs `supdoc namespace rename OLD NEW`: previews every edit, or with `--yes` makes them.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), the lockfile's own error (78 when
/// invalid, 74 when unreadable), `cli/rename-unknown-namespace` when OLD isn't defined,
/// `cli/rename-target-exists` when NEW already is, `cli/rename-inherited` when OLD comes only from
/// a config this one extends, `cli/rename-unreadable-source` when a source can't be read,
/// `cli/rename-file-changed` when a file changed between planning and
/// writing (all 1), `cli/rename-write` when a file can't be written, and `cli/rename-output` when
/// the report can't be written (both 74).
pub fn run(args: &RenameArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let plan = Plan::build(&args.old, &args.new, context, output)?;
    let applied = match args.confirm.mode() {
        WriteMode::Preview => false,
        WriteMode::Apply => {
            plan.apply()?;
            true
        }
    };
    output
        .report(&plan.report(applied), true)
        .map_err(|error| output_failed(&error))?;
    for note in plan.notes(applied) {
        output.note(&note).map_err(|error| output_failed(&error))?;
    }
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc namespace rename`'s `--json` output: the whole envelope, with `data`
/// typed as [`RenameReport`].
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, RenameReport>))
}

/// Every edit a rename makes, decided from the files as they were read.
#[derive(Debug)]
struct Plan {
    /// The namespace being renamed.
    old: NamespaceName,
    /// Its new name.
    new: NamespaceName,
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
    /// Returns the config's or the lockfile's own error, `cli/rename-unknown-namespace`,
    /// `cli/rename-target-exists`, `cli/rename-inherited`, `cli/rename-unreadable-source` when a
    /// source can't be read, or `cli/rename-file-changed` when the config no longer defines OLD
    /// once it is read again.
    fn build(
        old: &NamespaceName,
        new: &NamespaceName,
        context: &Context,
        output: &mut Output,
    ) -> Result<Self, CliError> {
        let config = load_config(context, output, COMMAND)?;
        let lock = read_lock(&config)?;
        let nearest = defining_file(&config, old, new)?;
        let config_edit = ConfigEdit::read(nearest, config.root(), old, new)?;
        let parent = parent_definition(&config, old)?;

        let languages = Languages::builtin();
        let walked = walk(&config, &languages, &Scope::All);
        let findings = walked.findings;
        let mut sources = Vec::new();
        for (path, _cartridge) in &walked.sources {
            // The walk only returns files beneath the root whose paths are valid source paths.
            let Some(source) = path
                .strip_prefix(config.root())
                .and_then(|relative| SourcePath::from_relative(relative).ok())
            else {
                continue;
            };
            match SourceFile::read(path.clone(), &languages) {
                Ok(file) => {
                    sources.extend(SourceEdit::find(&config, &languages, source, &file, old));
                }
                Err(error) => return Err(unread_source(&source, old, new, &error)),
            }
        }

        Ok(Self {
            lock: LockEdit::new(lock, old, new),
            old: old.clone(),
            new: new.clone(),
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
    /// Returns `cli/rename-file-changed` (1) before writing anything when a file no longer holds
    /// what was read, and `cli/rename-write` (74) when a write fails, naming what was already
    /// written.
    fn apply(&self) -> Result<(), CliError> {
        self.verify()?;
        let mut written: Vec<String> = Vec::new();
        for source in &self.sources {
            let renamed =
                replace_ranges(&source.text, source.prefixes(&self.old), self.new.as_str());
            replace_file(source.file.as_path(), renamed.as_bytes()).map_err(|error| {
                self.write_failed(source.source.as_str(), &error.to_string(), &written)
            })?;
            written.push(source.source.to_string());
        }
        if !self.lock.moved.is_empty() {
            self.lock
                .next()
                .write(&lock_path(&self.root))
                .map_err(|error| self.write_failed(LOCK_FILE_NAME, &error.to_string(), &written))?;
            written.push(LOCK_FILE_NAME.to_owned());
        }
        let renamed = replace_ranges(
            &self.config.text,
            std::iter::once(self.config.key.clone()),
            self.new.as_str(),
        );
        replace_file(self.config.file.as_path(), renamed.as_bytes())
            .map_err(|error| self.write_failed(&self.config.shown, &error.to_string(), &written))
    }

    /// Refuses the rename when any file it will write is no longer exactly as read (NR8).
    ///
    /// # Errors
    ///
    /// Returns `cli/rename-file-changed` naming the first such file.
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
    fn notes(&self, applied: bool) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(parent) = &self.parent {
            notes.push(format!(
                "{} also defines `{}` and was left as it is, so `{}` still names its directory \
                 there.",
                parent.display(),
                self.old,
                self.old
            ));
        }
        if applied {
            notes.push(format!("Renamed {} to {}.", self.old, self.new));
        } else {
            let links: usize = self.sources.iter().map(|source| source.links.len()).sum();
            notes.push(format!(
                "1 config entry, {} in {}, {}. {PREVIEW_NOTE}",
                count(links, "link", "links"),
                count(self.sources.len(), "file", "files"),
                count(self.lock.moved.len(), "lock entry", "lock entries"),
            ));
        }
        notes
    }

    /// The error for a file that no longer holds what was read.
    fn file_changed(&self, file: &str) -> CliError {
        CliError::new(
            "cli/rename-file-changed",
            format!(
                "{file} changed or can't be read since the rename was planned; nothing was written"
            ),
            format!(
                "Run `supdoc namespace rename {} {} --yes` again to plan from the files as they are now",
                self.old, self.new
            ),
            CliError::REFUSED_EXIT_CODE,
        )
    }

    /// The error for a file that couldn't be written, after the files in `written`.
    fn write_failed(&self, file: &str, error: &str, written: &[String]) -> CliError {
        let again = format!("supdoc namespace rename {} {} --yes", self.old, self.new);
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
            "cli/rename-write",
            format!("cannot write {file}: {error}"),
            help,
            CliError::IO_EXIT_CODE,
        )
    }
}

/// The nearest config file, which must be the one defining OLD, after checking both names.
///
/// # Errors
///
/// Returns `cli/rename-unknown-namespace` when OLD isn't defined (NR2), `cli/rename-target-exists`
/// when NEW is, or equals OLD (NR3), and `cli/rename-inherited` when only a config further up
/// the `extends` chain defines OLD (NR4).
fn defining_file<'c>(
    config: &'c Config,
    old: &NamespaceName,
    new: &NamespaceName,
) -> Result<&'c AbsolutePath, CliError> {
    let Some(defined_in) = config.namespace_defined_in(old) else {
        return Err(unknown_namespace(config, old));
    };
    if old == new || config.namespace(new).is_some() {
        return Err(target_exists(config, old, new));
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
) -> Result<Option<AbsolutePath>, CliError> {
    for file in config.files().iter().skip(1) {
        let text = read_config(file)?;
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
/// Returns the config's own read error (74).
fn read_config(file: &AbsolutePath) -> Result<String, CliError> {
    fs::read_to_string(file.as_path()).map_err(|error| {
        let error = ConfigError::Read {
            path: file.clone(),
            error,
        };
        CliError::new(
            error.code(),
            error.to_string(),
            error.help().unwrap_or_else(|| {
                format!("Check that the file can be read, then run `supdoc {COMMAND}` again")
            }),
            error.exit_code(),
        )
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
    /// The config file.
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
    /// Returns the config's own read error, or `cli/rename-file-changed` when the file no longer
    /// defines OLD.
    fn read(
        file: &AbsolutePath,
        root: &AbsolutePath,
        old: &NamespaceName,
        new: &NamespaceName,
    ) -> Result<Self, CliError> {
        let text = read_config(file)?;
        let relative = file.strip_prefix(root).map(Path::to_path_buf);
        let shown = relative.as_deref().map_or_else(
            || file.display().to_string(),
            |path| path.display().to_string(),
        );
        let Some(key) = namespace_key(&text, old) else {
            return Err(CliError::new(
                "cli/rename-file-changed",
                format!("{shown} no longer defines `{old}`; nothing was written"),
                format!("Run `supdoc namespace rename {old} {new}` again"),
                CliError::REFUSED_EXIT_CODE,
            ));
        };
        let location = relative
            .as_deref()
            .and_then(|path| SourcePath::from_relative(path).ok())
            .and_then(|source| Location::in_text(source, &text, key.clone()));
        Ok(Self {
            file: file.clone(),
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
        let renamed = replace_ranges(&self.text, std::iter::once(self.key.clone()), new);
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
        let links = ranges
            .into_iter()
            .map(|destination| LinkEdit {
                location: Location::in_text(source.clone(), text, destination.clone()),
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
/// `replacement`.
fn replace_ranges(
    text: &str,
    ranges: impl IntoIterator<Item = Range<usize>>,
    replacement: &str,
) -> String {
    let mut renamed = String::with_capacity(text.len());
    let mut at = 0;
    for range in ranges {
        renamed.push_str(text.get(at..range.start).unwrap_or_default());
        renamed.push_str(replacement);
        at = range.end;
    }
    renamed.push_str(text.get(at..).unwrap_or_default());
    renamed
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

/// The error for an OLD that no config defines, naming the likely intended one and every
/// configured namespace (NR2, Q18).
fn unknown_namespace(config: &Config, old: &NamespaceName) -> CliError {
    let names: Vec<&str> = config
        .namespaces()
        .keys()
        .map(NamespaceName::as_str)
        .collect();
    let listed = names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let help = match close_match(old.as_str(), names.iter().copied()) {
        _ if names.is_empty() => format!(
            "No namespaces are configured. Define one under `[namespaces]` in {CONFIG_FILE_NAME} \
             first"
        ),
        Some(close) => format!("did you mean `{close}`? Configured namespaces: {listed}"),
        None => format!("Configured namespaces: {listed}"),
    };
    CliError::new(
        "cli/rename-unknown-namespace",
        format!("no namespace is named `{old}`"),
        help,
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for a NEW that is already a namespace, or is OLD itself (NR3).
fn target_exists(config: &Config, old: &NamespaceName, new: &NamespaceName) -> CliError {
    let message = match config.namespace_defined_in(new) {
        _ if old == new => format!("`{new}` is already the namespace's name"),
        Some(file) => format!(
            "a namespace named `{new}` already exists, defined in {}",
            file.display()
        ),
        None => format!("a namespace named `{new}` already exists"),
    };
    CliError::new(
        "cli/rename-target-exists",
        message,
        "Run `supdoc namespace list` to see the names in use, and choose a NEW name none of \
         them has. Nothing was changed",
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for an OLD that only a config further up the `extends` chain defines (NR4, Q19).
fn inherited(old: &NamespaceName, defined_in: &AbsolutePath) -> CliError {
    CliError::new(
        "cli/rename-inherited",
        format!(
            "`{old}` is defined in {}, a config this project extends",
            defined_in.display()
        ),
        format!(
            "Rename it from the project that owns `{}`; renaming it here would change projects \
             that weren't scanned. Nothing was changed",
            defined_in.display()
        ),
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for a walked source that couldn't be read. Its links can't be checked for OLD, so
/// the whole rename is refused rather than moving lock entries past a file left unrenamed.
fn unread_source(
    source: &SourcePath,
    old: &NamespaceName,
    new: &NamespaceName,
    error: &ReadError,
) -> CliError {
    CliError::new(
        "cli/rename-unreadable-source",
        format!(
            "{}; its links can't be renamed, so nothing was written",
            error.message_naming(source)
        ),
        format!(
            "{} Then run `supdoc namespace rename {old} {new}` again",
            error.help()
        ),
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/rename-output",
        format!("cannot write the rename report: {error}"),
        "Check that standard output and standard error are open and writable, or redirect both \
         to a file with `> supdoc.out 2>&1`",
        CliError::IO_EXIT_CODE,
    )
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
    const fn label(self) -> &'static str {
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
                change.kind.label(),
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use clap::Parser as _;

    use super::*;
    use crate::check::reconcile::reconcile;
    use crate::cli::args::{Cli, Command, NamespaceCommand};
    use crate::output::{OutputSettings, PathStyle, Verbosity};
    use crate::scan::model::TargetState;
    use crate::state::LinkState;

    /// A config naming two namespaces, with comments that a rename must keep.
    /// A parent config whose namespace lies inside `app/`, so `app/` may inherit it.
    const INHERITED_CONFIG: &str = "[namespaces]\nauth = \"app/services/auth\"\n";

    const CONFIG: &str = "\
# Shared code lives beside the docs.
[namespaces]
# The sign-in service.
auth = \"services/auth\"   # moved here in 2024
web = \"services/web\"
";

    /// A guide with one link into each namespace.
    const GUIDE: &str = "# Guide\n\nSee [login](auth:login.rs#login) and [app](web:app.rs#app).\n";

    /// A buffer `Output` writes into and the test reads back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
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

    /// The value at `pointer` (RFC 6901) in `json`, or a failure naming what is missing.
    fn at(json: &serde_json::Value, pointer: &str) -> serde_json::Value {
        json.pointer(pointer)
            .unwrap_or_else(|| panic!("`{pointer}` should be in the JSON output: {json}"))
            .clone()
    }

    /// What one `supdoc namespace rename` run returned and printed.
    struct Run {
        result: Result<Outcome, CliError>,
        stdout: String,
        stderr: String,
    }

    impl Run {
        fn exit_code(&self) -> u8 {
            match &self.result {
                Ok(outcome) => outcome.exit_code(),
                Err(error) => error.exit_code(),
            }
        }

        fn error(&self) -> &CliError {
            self.result
                .as_ref()
                .err()
                .unwrap_or_else(|| panic!("the rename should fail; stdout: {}", self.stdout))
        }

        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.stdout)
                .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", self.stdout))
        }
    }

    /// A project in a fresh temporary directory: the config, both namespaces' code and the guide.
    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        fn new() -> Self {
            let project = Self {
                dir: tempfile::tempdir().expect("a fresh temp directory should be created"),
            };
            project.write(CONFIG_FILE_NAME, CONFIG);
            project.write(
                "services/auth/login.rs",
                "pub fn login() {}\n\npub fn logout() {}\n",
            );
            project.write("services/web/app.rs", "pub fn app() {}\n");
            project.write("docs/guide.md", GUIDE);
            project
        }

        fn root(&self) -> AbsolutePath {
            AbsolutePath::new(self.dir.path()).expect("a temp directory should be absolute")
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be created");
            }
            fs::write(&path, text).expect("the fixture file should be written");
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.dir.path().join(relative))
                .unwrap_or_else(|error| panic!("{relative} should be readable: {error}"))
        }

        fn lock(&self) -> Lockfile {
            Lockfile::read(&lock_path(&self.root()))
                .expect("the lockfile should parse")
                .expect("the lockfile should exist")
        }

        /// Every file under the root and its bytes.
        fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
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

        /// The project's config, loaded as `supdoc` loads it from the root.
        fn config(&self) -> Config {
            Config::load(&self.root()).expect("the fixture config should load")
        }

        /// Records every present link's current hash in `supdoc.lock`, as `update --yes` would.
        fn record_all(&self) {
            let scan =
                crate::scan::assemble::run(&self.config(), &Languages::builtin(), &Scope::All)
                    .expect("the fixture project should scan");
            let mut lock = Lockfile::default();
            for (key, link) in scan.links {
                if let TargetState::Present { hash, .. } = link.target {
                    let _previous = lock.insert(key, hash);
                }
            }
            assert!(!lock.is_empty(), "the fixture should have a link to record");
            lock.write(&lock_path(&self.root()))
                .expect("the fixture lockfile should be written");
        }

        /// Each link's source, target, symbol and state, as `supdoc check` would judge them.
        fn states(&self) -> Vec<(String, String, LinkState)> {
            let scan =
                crate::scan::assemble::run(&self.config(), &Languages::builtin(), &Scope::All)
                    .expect("the fixture project should scan");
            let lock = Lockfile::read(&lock_path(&self.root()))
                .expect("the lockfile should parse")
                .unwrap_or_default();
            reconcile(&scan, &lock, &Scope::All)
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
            let cwd = AbsolutePath::new(&self.dir.path().join(relative))
                .expect("a directory under the root should be absolute");
            let (cli, mut output, stdout, stderr) = setup(&cwd, args);
            let Command::Namespace(NamespaceCommand::Rename(rename)) = &cli.command else {
                panic!("`{}` should be a namespace rename", args.join(" "));
            };
            let context = context(&cli, cwd);
            let result = run(rename, &context, &mut output);
            Run {
                result,
                stdout: stdout.text(),
                stderr: stderr.text(),
            }
        }

        /// Runs `supdoc <args>` from the project root.
        fn rename(&self, args: &[&str]) -> Run {
            self.rename_from("", args)
        }

        /// The plan `supdoc namespace rename OLD NEW` makes from the root, without running it.
        fn plan(&self, old: &str, new: &str) -> Plan {
            let args = ["namespace", "rename", old, new];
            let (cli, mut output, _stdout, _stderr) = setup(&self.root(), &args);
            let Command::Namespace(NamespaceCommand::Rename(rename)) = &cli.command else {
                panic!("`{}` should be a namespace rename", args.join(" "));
            };
            let context = context(&cli, self.root());
            Plan::build(&rename.old, &rename.new, &context, &mut output)
                .expect("the rename should plan")
        }
    }

    /// Parses `args` and builds an `Output` that writes into two captures, with paths rooted at
    /// `cwd` as `main` roots them before any config loads.
    fn setup(cwd: &AbsolutePath, args: &[&str]) -> (Cli, Output, Capture, Capture) {
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
        (cli, output, stdout, stderr)
    }

    fn context(cli: &Cli, cwd: AbsolutePath) -> Context {
        Context {
            global: cli.global.clone(),
            cwd,
            home: None,
            claude_config_dir: None,
        }
    }

    #[test]
    fn namespace_rename_yes_updates_config_lock_and_links() {
        let project = Project::new();
        project.record_all();
        let entries = |lock: &Lockfile| -> BTreeMap<String, String> {
            lock.iter()
                .map(|(key, hash)| (key.to_string(), hash.to_string()))
                .collect()
        };
        let expected: BTreeMap<String, String> = entries(&project.lock())
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
            entries(&project.lock()),
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
        let project = Project::new();
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
        let project = Project::new();
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
        let project = Project::new();
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
        let project = Project::new();
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
        let project = Project::new();
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
            "1 config entry, 1 link in 1 file, 1 lock entry. Nothing written. Run the same \
             command with --yes to apply.\n"
        );
    }

    #[test]
    fn namespace_rename_unknown_old_exits_1_listing_names() {
        let project = Project::new();
        let before = project.files();

        let run = project.rename(&["namespace", "rename", "atuh", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 1);
        let error = run.error();
        assert_eq!(error.code(), "cli/rename-unknown-namespace");
        assert!(error.message().contains("`atuh`"), "{}", error.message());
        assert_eq!(
            error.help(),
            "did you mean `auth`? Configured namespaces: `auth`, `web`"
        );
        assert_eq!(project.files(), before);
    }

    #[test]
    fn namespace_rename_existing_new_exits_1() {
        let project = Project::new();
        let before = project.files();

        let taken = project.rename(&["namespace", "rename", "auth", "web", "--yes"]);
        let same = project.rename(&["namespace", "rename", "auth", "auth", "--yes"]);

        for (name, run) in [("taken", &taken), ("same", &same)] {
            assert_eq!(run.exit_code(), 1, "{name}");
            assert_eq!(run.error().code(), "cli/rename-target-exists", "{name}");
        }
        assert!(
            taken.error().message().contains("`web`"),
            "{}",
            taken.error().message()
        );
        assert_eq!(project.files(), before);
    }

    #[test]
    fn namespace_rename_inherited_namespace_is_refused_naming_its_file() {
        let project = Project::new();
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
        let error = run.error();
        assert_eq!(error.code(), "cli/rename-inherited");
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
        let project = Project::new();
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
        let project = Project::new();
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
        let project = Project::new();
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
            ("cli/rename-file-changed", 1)
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
    fn namespace_rename_refuses_everything_when_a_source_is_unreadable() {
        let project = Project::new();
        project.record_all();
        project.write("docs/broken.md", "See [login](auth:login.rs#login).\n");
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
            ("cli/rename-unreadable-source", 1)
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
            project.files(),
            before,
            "no file, lock or config change is written"
        );
    }

    #[test]
    fn namespace_rename_without_a_lockfile_creates_none() {
        let project = Project::new();

        let run = project.rename(&["namespace", "rename", "auth", "identity", "--yes"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert!(
            !project.dir.path().join(LOCK_FILE_NAME).exists(),
            "a rename never creates supdoc.lock"
        );
    }

    #[test]
    fn namespace_rename_json_validates_against_its_schema() {
        let project = Project::new();
        project.record_all();
        let json = project
            .rename(&["--json", "namespace", "rename", "auth", "identity"])
            .json();
        let schema = serde_json::to_value(schema().expect("namespace rename is built"))
            .expect("the schema should serialize");
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
