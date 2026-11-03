//! The scan's entry point, [`run`]: walk the project, read every source once, extract its links,
//! then resolve each target once.
//!
//! Only a project root that can't be read or walked stops the run. Every other problem, with one
//! file or one link, becomes a finding and the scan goes on.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use crate::code_blocks::block::{CodeBlock, try_blocks_in};
use crate::config::Config;
use crate::exit::ExitStatus;
use crate::finding::{Finding, Fix, LineIndex, Location, Severity, Snippet};
use crate::hash::HashSettings;
use crate::link::destination::{
    Destination, LinkError, Untracked, parse_destination, repaired_destination,
};
use crate::link::{LinkKey, RawLink, SourcePath};
use crate::lock::error::shell_quote;
use crate::namespace::NamespaceName;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile};
use crate::read::error::{CartridgeLoadError, ReadError, ReadRole};
use crate::read::links::links;
use crate::scan::model::{Scan, ScannedLink, Scope, TargetState};
use crate::scan::targets::{self, CODE_UNKNOWN_NAMESPACE, Occurrence, Parsed, Written};
use crate::scan::walk::{
    CODE_IGNORE_TOO_LARGE, GlobalExcludes, UnreadDir, Walk, WalkedSource, walk,
};

/// Why a scan couldn't run at all. Every smaller problem is a finding instead.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ScanError {
    /// The project root itself can't be listed, so no result would mean anything.
    #[error("cannot read the project root {}: {error}", .root.display())]
    RootUnreadable {
        /// The project root.
        root: AbsolutePath,
        /// What the operating system reported.
        error: std::io::Error,
    },
    /// A built-in cartridge didn't load when a file of its language was read: a supdoc bug, so
    /// no result would mean anything.
    #[error("{0}")]
    CartridgeLoad(#[from] CartridgeLoadError),
    /// An ignore file is larger than `max-ignore-size`, so it wasn't read. Skipping it would widen
    /// the scan, so the scan doesn't run.
    #[error("{message}")]
    IgnoreTooLarge {
        /// What the walk reported, naming the file, its size and the limit.
        message: String,
        /// What the walk said to do next.
        help: String,
    },
}

impl ScanError {
    /// The refusal for the first `ignore-too-large` among a walk's `findings`, if there is one.
    /// Every command that walks the project refuses with it before it reads a source or writes.
    #[must_use]
    pub(crate) fn ignore_too_large(findings: &[Finding]) -> Option<Self> {
        let too_large = findings
            .iter()
            .find(|finding| finding.code == CODE_IGNORE_TOO_LARGE)?;
        Some(Self::IgnoreTooLarge {
            message: too_large.message.clone(),
            help: too_large.help.clone().unwrap_or_default(),
        })
    }

    /// The stable code for this error: `scan-root-unreadable`.
    #[must_use]
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::RootUnreadable { .. } => "scan-root-unreadable",
            Self::CartridgeLoad(_) => CartridgeLoadError::code(),
            Self::IgnoreTooLarge { .. } => CODE_IGNORE_TOO_LARGE,
        }
    }

    /// What to do next.
    #[must_use]
    pub(crate) fn help(&self) -> String {
        match self {
            Self::RootUnreadable { root, .. } => format!(
                "Give the current user read and search access to the project root, for example \
                 with `chmod u+rx {}`",
                shell_quote(&root.display().to_string())
            ),
            Self::CartridgeLoad(_) => CartridgeLoadError::help(),
            Self::IgnoreTooLarge { help, .. } => help.clone(),
        }
    }

    /// The process exit status for this error: 74 when the project can't be read, 70 for a
    /// built-in cartridge that doesn't load, 78 for an ignore file over its configured ceiling.
    #[must_use]
    pub(crate) const fn status(&self) -> ExitStatus {
        match self {
            Self::RootUnreadable { .. } => ExitStatus::Io,
            Self::CartridgeLoad(_) => ExitStatus::Bug,
            Self::IgnoreTooLarge { .. } => ExitStatus::Config,
        }
    }
}

/// Scans the part of the project `scope` names: every link in its sources, what each target looks
/// like right now, and every problem met on the way, in a deterministic order. The walk
/// honors the global `excludes` file `main` found. Each file is
/// parsed at most once as a source and once as a target. The lockfile is never read.
///
/// Effect: reads the filesystem. Everything after the walk and the file reads is pure.
///
/// # Errors
///
/// [`ScanError::RootUnreadable`] when the project root can't be listed,
/// [`ScanError::IgnoreTooLarge`] when an ignore file the walk met is over `max-ignore-size`, and
/// [`ScanError::CartridgeLoad`] when a source or target's built-in cartridge doesn't load.
pub(crate) fn run(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    excludes: &GlobalExcludes,
) -> Result<Scan, ScanError> {
    run_with_settings(config, languages, scope, excludes, config.hash_settings())
}

/// [`run`], hashing every target under `settings` rather than the config's own, for a command
/// that read them from the lockfile it is about to re-record.
///
/// # Errors
///
/// As [`run`].
pub(crate) fn run_with_settings(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    excludes: &GlobalExcludes,
    settings: HashSettings,
) -> Result<Scan, ScanError> {
    assemble(config, languages, scope, excludes, settings).map(|(scan, _parsed, _untracked)| scan)
}

/// [`run`], also counting the links it left untracked, by reason, for `check -v`.
///
/// # Errors
///
/// As [`run`].
pub(crate) fn run_with_untracked(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    excludes: &GlobalExcludes,
) -> Result<(Scan, UntrackedLinks), ScanError> {
    assemble(config, languages, scope, excludes, config.hash_settings())
        .map(|(scan, _parsed, untracked)| (scan, untracked))
}

/// How many links a scan left untracked for each reason, one per place written.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct UntrackedLinks(BTreeMap<Untracked, usize>);

impl UntrackedLinks {
    /// Counts `count` more links left untracked for `reason`.
    fn add(&mut self, reason: Untracked, count: usize) {
        if count > 0 {
            *self.0.entry(reason).or_default() += count;
        }
    }

    /// Each reason at least one link was left untracked for, with how many, in reason order.
    pub(crate) fn counts(&self) -> impl Iterator<Item = (Untracked, usize)> + '_ {
        self.0.iter().map(|(reason, count)| (*reason, *count))
    }
}

/// [`run`], also handing back the files it parsed, so tests can see that none was parsed twice,
/// and the links it left untracked.
///
/// # Errors
///
/// As [`run`].
fn assemble(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    excludes: &GlobalExcludes,
    settings: HashSettings,
) -> Result<(Scan, Parsed, UntrackedLinks), ScanError> {
    let root = config.root();
    if let Err(error) = fs::read_dir(root.as_path()) {
        return Err(ScanError::RootUnreadable {
            root: root.clone(),
            error,
        });
    }

    let mut findings = Vec::new();
    let mut parsed = Parsed::default();
    let mut unread_sources = BTreeSet::new();
    let mut found = SourceLinks::default();
    let front = walk_checked(config, languages, scope, excludes)?;
    read_walked(
        config,
        languages,
        &mut parsed,
        &front.sources,
        |walked, file, raws| source_links(config, languages, walked, file, raws, &mut found),
        |walked, error| {
            findings.push(unreadable_source(&walked.source, &error));
            // A file only code-blocks reads stays out of linking's unread sources, so its locked
            // links are judged exactly as without code-blocks.
            if walked.wants.links {
                let _new = unread_sources.insert(walked.source.clone());
            }
        },
    )?;
    findings.extend(front.findings);
    findings.extend(
        front
            .code_blocks_unread_dirs
            .iter()
            .map(|dir| dir.finding(root)),
    );

    // The root itself was read and walked above, so every unread directory lies beneath it.
    let unread_dirs = front
        .unread_dirs
        .iter()
        .filter_map(|dir| dir.path.strip_prefix(root))
        .filter_map(|relative| SourcePath::from_relative(relative).ok())
        .collect();

    let (mut links, directory_links) =
        targets::resolve(config, languages, &mut parsed, settings, found.occurrences);
    if let Some(error) = parsed.cartridge_failure() {
        return Err(error.into());
    }
    let mut untracked = found.untracked;
    untracked.add(Untracked::Directory, directory_links);
    add_broken(&mut links, found.broken);
    let keyless_error_sources = found
        .unkeyed
        .iter()
        .filter_map(|finding| {
            finding
                .location
                .as_ref()
                .map(|location| location.file.clone())
        })
        .collect();
    findings.extend(found.unkeyed);
    findings.sort_by(|left, right| {
        position(left)
            .cmp(&position(right))
            .then_with(|| left.code.cmp(right.code))
            .then_with(|| left.message.cmp(&right.message))
    });
    let scan = Scan {
        links,
        findings,
        unread_sources,
        keyless_error_sources,
        unread_dirs,
        code_blocks: found.code_blocks,
    };
    Ok((scan, parsed, untracked))
}

/// What [`read_sources`] met besides the sources themselves.
#[derive(Debug)]
pub(crate) struct FrontHalf {
    /// The walk's findings.
    pub(crate) findings: Vec<Finding>,
    /// The directories the walk couldn't read.
    pub(crate) unread_dirs: Vec<UnreadDir>,
}

/// The front half of a scan, shared with `namespace rename`: walks the project under the global
/// `excludes` file, refuses an
/// oversized ignore file, then reads each source linking wants once under the config's read
/// deadline and hands its parsed file and raw links to `on_links`, or its failure to `on_unread`.
/// A source only code-blocks wants is never read here. Each file is dropped once its callback
/// returns.
///
/// Effect: reads the filesystem.
///
/// # Errors
///
/// [`ScanError::IgnoreTooLarge`] when an ignore file the walk met is over `max-ignore-size`, and
/// [`ScanError::CartridgeLoad`] when a source's built-in cartridge doesn't load.
pub(crate) fn read_sources(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    excludes: &GlobalExcludes,
    parsed: &mut Parsed,
    mut on_links: impl FnMut(&SourcePath, &SourceFile, Vec<RawLink>),
    mut on_unread: impl FnMut(&SourcePath, ReadError),
) -> Result<FrontHalf, ScanError> {
    let walked = walk_checked(config, languages, scope, excludes)?;
    read_walked(
        config,
        languages,
        parsed,
        walked.sources.iter().filter(|source| source.wants.links),
        |walked, file, raws| {
            on_links(&walked.source, file, raws);
            Ok(())
        },
        |walked, error| on_unread(&walked.source, error),
    )?;
    Ok(FrontHalf {
        findings: walked.findings,
        unread_dirs: walked.unread_dirs,
    })
}

/// Walks the project under the global `excludes` file and refuses an oversized ignore file.
///
/// Effect: reads directory listings, ignore files and file metadata.
///
/// # Errors
///
/// [`ScanError::IgnoreTooLarge`] when an ignore file the walk met is over `max-ignore-size`.
fn walk_checked(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    excludes: &GlobalExcludes,
) -> Result<Walk, ScanError> {
    let walked = walk(config, languages, scope, excludes);
    match ScanError::ignore_too_large(&walked.findings) {
        Some(error) => Err(error),
        None => Ok(walked),
    }
}

/// Reads each of `sources` once under the config's read deadline and hands its parsed file to
/// `on_read`, with its raw links when linking wants it and none otherwise, or its failure to
/// `on_unread`. Each file is dropped once its callback returns.
///
/// Effect: reads the filesystem.
///
/// # Errors
///
/// [`ScanError::CartridgeLoad`] when a source's built-in cartridge doesn't load.
fn read_walked<'walk>(
    config: &Config,
    languages: &Languages,
    parsed: &mut Parsed,
    sources: impl IntoIterator<Item = &'walk WalkedSource>,
    mut on_read: impl FnMut(&WalkedSource, &SourceFile, Vec<RawLink>) -> Result<(), ReadError>,
    mut on_unread: impl FnMut(&WalkedSource, ReadError),
) -> Result<(), ScanError> {
    let limit = config.limits_in_effect().parse_timeout.value;
    for walked in sources {
        let read = parsed
            .read(&walked.path, languages, limit)
            .and_then(|file| {
                let raws = if walked.wants.links {
                    links(file)?
                } else {
                    Vec::new()
                };
                Ok((raws, file))
            });
        match read {
            Ok((raws, file)) => {
                if let Err(error) = on_read(walked, file, raws) {
                    on_unread(walked, error);
                }
            }
            Err(ReadError::CartridgeLoad { error, .. }) => return Err(error.into()),
            Err(error) => on_unread(walked, error),
        }
        parsed.forget(&walked.path);
    }
    Ok(())
}

/// Where a finding sits, for sorting by file then position. Findings about no one place sort
/// first.
fn position(finding: &Finding) -> Option<(&SourcePath, usize, usize)> {
    finding
        .location
        .as_ref()
        .map(|location| (&location.file, location.range.start, location.range.end))
}

/// The supdoc links read from sources, in file order, before any target is read.
#[derive(Debug, Default)]
struct SourceLinks {
    /// Links that parsed, still to be resolved.
    occurrences: Vec<Occurrence>,
    /// Links meant for supdoc but wrong that still have a key: broken as written.
    broken: Vec<(LinkKey, Location, Finding)>,
    /// Links meant for supdoc but too malformed to have a key: only their finding is left.
    unkeyed: Vec<Finding>,
    /// Links that are not supdoc links, counted by reason.
    untracked: UntrackedLinks,
    /// Every code block in the sources code-blocks wants, in file order.
    code_blocks: Vec<CodeBlock>,
}

/// Adds `raws`, the links read from `walked`'s `file`, to `found`, in file order, and its code
/// blocks when code-blocks wants the file.
///
/// # Errors
///
/// The [`ReadError`] of a failed block parse; nothing is added then, and the source is unread.
fn source_links(
    config: &Config,
    languages: &Languages,
    walked: &WalkedSource,
    file: &SourceFile,
    raws: Vec<RawLink>,
    found: &mut SourceLinks,
) -> Result<(), ReadError> {
    let source = &walked.source;
    let text = file.text();
    let lines = LineIndex::new(text);
    if walked.wants.code_blocks {
        // Read first, so a source whose blocks can't be read adds nothing and is reported unread.
        let blocks = try_blocks_in(file, |range| {
            Location::in_indexed(source.clone(), text, &lines, range)
        })?;
        found.code_blocks.extend(blocks);
    }
    for raw in raws {
        let parsed = match parse_destination(&raw, source, config.namespaces(), languages) {
            Destination::Link(link) => Ok(link),
            Destination::Invalid(error) => Err(error),
            Destination::NotSupdoc { reason } => {
                found.untracked.add(reason, 1);
                continue;
            }
        };
        // A link's range comes from this file's own tree, so it always lies inside the text.
        let Some(location) = Location::in_indexed(source.clone(), text, &lines, raw.range.clone())
        else {
            continue;
        };
        let snippet = Snippet::capture_indexed(text, &lines, raw.range.clone());
        let written = Written::locate(text, &raw.range, &raw.destination);
        match parsed {
            Ok(link) => found.occurrences.push(Occurrence {
                key: LinkKey {
                    source: source.clone(),
                    target: link.target,
                    symbol: link.symbol,
                },
                location,
                snippet,
                written,
            }),
            Err(error) => {
                let key = unknown_namespace_key(source, &error);
                let repair = unique_repair(config, languages, &raw, source, written.as_ref());
                let finding = link_finding(
                    config,
                    &error,
                    location.clone(),
                    snippet,
                    written.as_ref(),
                    repair.as_ref(),
                );
                match key {
                    Some(key) => found.broken.push((key, location, finding)),
                    None => found.unkeyed.push(finding),
                }
            }
        }
    }
    Ok(())
}

/// The key `source`'s link would have if its unknown namespace were configured, so a locked link
/// whose namespace was renamed or removed in config is broken rather than obsolete.
///
/// `None` for every other link error, and when the rest of the destination is wrong too: an
/// invalid path or symbol can't be written as a key.
fn unknown_namespace_key(source: &SourcePath, error: &LinkError) -> Option<LinkKey> {
    let LinkError::UnknownNamespace {
        parsed: Some(link), ..
    } = error
    else {
        return None;
    };
    Some(LinkKey {
        source: source.clone(),
        target: link.target.clone(),
        symbol: link.symbol.clone(),
    })
}

/// Adds each keyed link error to `links` as a broken link. A link written more than once keeps
/// every location, and the finding of the place it is first written.
fn add_broken(
    links: &mut BTreeMap<LinkKey, ScannedLink>,
    broken: Vec<(LinkKey, Location, Finding)>,
) {
    for (key, location, finding) in broken {
        match links.entry(key) {
            Entry::Occupied(mut link) => link.get_mut().more.push(location),
            Entry::Vacant(slot) => {
                let _link = slot.insert(ScannedLink {
                    snippet: finding.snippet.clone(),
                    first: location,
                    more: Vec::new(),
                    target: TargetState::Broken { finding },
                });
            }
        }
    }
}

/// The corrected destination of `raw` and the fix that writes it, when the mechanical repair
/// (see [`repaired_destination`]) gives a destination that parses and the written text is found
/// exactly once in the source.
fn unique_repair(
    config: &Config,
    languages: &Languages,
    raw: &RawLink,
    source: &SourcePath,
    written: Option<&Written>,
) -> Option<(String, Written, std::ops::Range<usize>)> {
    let repaired = repaired_destination(&raw.destination)?;
    let candidate = RawLink {
        destination: repaired.clone(),
        ..raw.clone()
    };
    let Destination::Link(_parsed) =
        parse_destination(&candidate, source, config.namespaces(), languages)
    else {
        return None;
    };
    let written = written?;
    let range = raw.destination_range.clone();
    (written.text_at(&range)?.as_ref() == raw.destination.trim())
        .then(|| (repaired, written.clone(), range))
}

/// The finding for a destination that is meant for supdoc but wrong. An unknown namespace gets a
/// fix when exactly one configured namespace is a close spelling of it.
fn link_finding(
    config: &Config,
    error: &LinkError,
    location: Location,
    snippet: Option<Snippet>,
    written: Option<&Written>,
    repair: Option<&(String, Written, std::ops::Range<usize>)>,
) -> Finding {
    let repair_fix = |repaired: &str| {
        let (_, written, range) = repair?;
        Some(Fix {
            file: location.file.clone(),
            scanned: written.text_at(range)?,
            range: range.clone(),
            replacement: repaired.to_owned(),
        })
    };
    let (code, help, fix) = match error {
        LinkError::UnknownNamespace { prefix, known, .. } => {
            let close =
                crate::suggest::close_match(prefix, known.iter().map(NamespaceName::as_str));
            let fix = close.zip(written.and_then(|written| written.prefix_range(prefix)));
            let fix = fix.and_then(|(close, range)| {
                Some(Fix {
                    file: location.file.clone(),
                    scanned: written?.text_at(&range)?,
                    range,
                    replacement: close.to_owned(),
                })
            });
            let known: Vec<&str> = config
                .namespaces()
                .keys()
                .map(NamespaceName::as_str)
                .collect();
            let help = crate::namespace::unknown_namespace_help(prefix, &known);
            (CODE_UNKNOWN_NAMESPACE, help, fix)
        }
        LinkError::InvalidPath { .. } => {
            let rule = "Write the path with `/` between names, relative to this file or to the \
                        project root with a leading `/`";
            match repair {
                Some((repaired, ..)) => (
                    "link-invalid-path",
                    format!("Write `{repaired}`. {rule}"),
                    repair_fix(repaired),
                ),
                None => ("link-invalid-path", rule.to_owned(), None),
            }
        }
        LinkError::Directory { .. } => (
            "link-invalid-path",
            "Add the file's name to the path, so the link names the file that holds the symbol"
                .to_owned(),
            None,
        ),
        LinkError::InvalidSymbol { .. } => (
            "link-invalid-symbol",
            "Write the symbol as names joined by `.`, like `Config.validate`".to_owned(),
            None,
        ),
        LinkError::OutsideRoot { .. } => (
            targets::CODE_OUTSIDE_ROOT,
            targets::OUTSIDE_ROOT_HELP.to_owned(),
            None,
        ),
        LinkError::TextFragmentUnsupported { .. } => {
            let rule = "Remove the `:~:` text fragment from the link. supdoc can't read text \
                        fragments yet";
            match repair {
                Some((repaired, ..)) => (
                    "link-text-fragment-unsupported",
                    format!("Write `{repaired}`. {rule}"),
                    repair_fix(repaired),
                ),
                None => ("link-text-fragment-unsupported", rule.to_owned(), None),
            }
        }
    };

    let finding = Finding::new(code, Severity::Error, error.to_string())
        .with_location(location)
        .with_help(help);
    let finding = match snippet {
        Some(snippet) => finding.with_snippet(snippet),
        None => finding,
    };
    match fix {
        Some(fix) => finding.with_fix(fix),
        None => finding,
    }
}

/// The warning for a walked source that couldn't be read. Its links weren't checked, and its lock
/// entries are kept.
///
/// It points at the start of the file, since no text was read to point into.
fn unreadable_source(source: &SourcePath, error: &ReadError) -> Finding {
    let start = Location {
        file: source.clone(),
        range: 0..0,
        line: 1,
        column: 1,
    };
    // A file that outlasts the read deadline may be hostile, so it fails the run rather than
    // passing with its links unchecked (SEC-1).
    let severity = if matches!(error, ReadError::Timeout { .. }) {
        Severity::Error
    } else {
        Severity::Warning
    };
    Finding::new(error.code(), severity, error.message_naming(source))
        .with_location(start)
        .with_help(format!(
            "Links in {source} weren't checked. {}",
            error.help(ReadRole::ScannedSource, &source.to_string())
        ))
}

#[cfg(test)]
mod tests {
    #![cfg_attr(
        test,
        allow(
            clippy::missing_errors_doc,
            clippy::missing_panics_doc,
            reason = "tests report failures rather than document callable contracts"
        )
    )]

    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use crate::config::CONFIG_FILE_NAME;

    use super::*;
    use crate::check::reconcile::{Outcome, check_outcome, reconcile_at_root};
    use crate::hash::Hash;
    use crate::link::Target;
    use crate::lock::model::Lockfile;
    use crate::state::LinkState;

    /// Two functions, `add` and `sub`.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n\
                       pub(crate) fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n";

    /// A project on disk. Its root holds `supdoc.config.toml` and may sit below the temp dir, so
    /// a test can place files outside the root.
    struct Project {
        dir: TempDir,
        root: PathBuf,
    }

    impl Project {
        /// A project rooted at the temp dir, with an empty config.
        fn new() -> Self {
            Self::with_config("")
        }

        /// A project rooted at the temp dir, with `config` as its config file.
        fn with_config(config: &str) -> Self {
            Self::at(None, config)
        }

        /// A project rooted at `root` beneath the temp dir, or at the temp dir itself.
        fn at(root: Option<&str>, config: &str) -> Self {
            let dir = TempDir::new().expect("a temporary project directory should be creatable");
            let root = root.map_or_else(|| dir.path().to_path_buf(), |root| dir.path().join(root));
            let project = Self { dir, root };
            project.write(CONFIG_FILE_NAME, config);
            project
        }

        /// Writes `text` to `relative`, beneath the root.
        fn write(&self, relative: &str, text: &str) {
            self.write_bytes(relative, text.as_bytes());
        }

        fn write_bytes(&self, relative: &str, bytes: &[u8]) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be creatable");
            }
            fs::write(&path, bytes).expect("the fixture file should be writable");
        }

        /// Writes `text` to `relative`, beneath the temp dir rather than the root.
        fn write_outside(&self, relative: &str, text: &str) {
            let path = self.dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be creatable");
            }
            fs::write(&path, text).expect("the fixture file should be writable");
        }

        fn config(&self) -> Config {
            let file = AbsolutePath::new(&self.root.join(CONFIG_FILE_NAME))
                .expect("the temporary directory should be absolute");
            Config::load_file(&file).expect("the fixture config should be valid")
        }

        fn assemble(&self) -> (Scan, Parsed) {
            assemble(
                &self.config(),
                Languages::builtin().expect("the built-in cartridges should load"),
                &Scope::All,
                &GlobalExcludes::none(),
                self.config().hash_settings(),
            )
            .map(|(scan, parsed, _untracked)| (scan, parsed))
            .expect("a readable project root should scan")
        }

        /// The scan, with the links it left untracked as `(reason, count)` pairs.
        fn scan_untracked(&self) -> (Scan, Vec<(Untracked, usize)>) {
            let (scan, untracked) = run_with_untracked(
                &self.config(),
                Languages::builtin().expect("the built-in cartridges should load"),
                &Scope::All,
                &GlobalExcludes::none(),
            )
            .expect("a readable project root should scan");
            (scan, untracked.counts().collect())
        }

        fn scan(&self) -> Scan {
            run(
                &self.config(),
                Languages::builtin().expect("the built-in cartridges should load"),
                &Scope::All,
                &GlobalExcludes::none(),
            )
            .expect("a readable project root should scan")
        }

        /// The text of `relative` beneath the root, as now on disk.
        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.root.join(relative)).expect("the fixture file should exist")
        }
    }

    fn key(source: &str, target: &str, symbol: Option<&str>) -> LinkKey {
        LinkKey {
            source: source
                .parse()
                .expect("the fixture source path should be valid"),
            target: target.parse().expect("the fixture target should be valid"),
            symbol: symbol
                .map(|symbol| symbol.parse().expect("the fixture symbol should be valid")),
        }
    }

    fn link<'s>(scan: &'s Scan, key: &LinkKey) -> &'s ScannedLink {
        scan.links.get(key).unwrap_or_else(|| {
            panic!(
                "the scan should hold {key}; it holds {:?}",
                scan.links
                    .keys()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            )
        })
    }

    fn broken<'s>(scan: &'s Scan, key: &LinkKey) -> &'s Finding {
        match &link(scan, key).target {
            TargetState::Broken { finding } => finding,
            present @ TargetState::Present { .. } => {
                panic!("{key} should be broken, but its target is {present:?}")
            }
        }
    }

    fn assert_present(scan: &Scan, key: &LinkKey) {
        let state = &link(scan, key).target;
        assert!(
            matches!(state, TargetState::Present { .. }),
            "{key} should resolve, but its target is {state:?}"
        );
    }

    /// The source text a fix would replace.
    fn fixed_text<'t>(text: &'t str, fix: &Fix) -> &'t str {
        text.get(fix.range.clone())
            .expect("a fix's range should lie inside its source")
    }

    // Scanning

    #[test]
    fn markdown_and_code_comment_links_both_appear() {
        let project = Project::new();
        project.write(
            "docs/guide.md",
            "# Guide\n\nSee [add](../src/lib.rs#add).\n",
        );
        project.write(
            "src/lib.rs",
            &format!("// Explained in [the guide](../docs/guide.md#guide).\n{LIB}"),
        );

        let scan = project.scan();

        assert_eq!(
            scan.links.keys().cloned().collect::<Vec<_>>(),
            vec![
                key("docs/guide.md", "src/lib.rs", Some("add")),
                key("src/lib.rs", "docs/guide.md", Some("guide")),
            ],
        );
        assert_present(&scan, &key("docs/guide.md", "src/lib.rs", Some("add")));
        assert_present(&scan, &key("src/lib.rs", "docs/guide.md", Some("guide")));
        assert_eq!(scan.findings, Vec::new());
    }

    #[test]
    fn duplicate_link_is_one_entry_with_all_locations() {
        let project = Project::new();
        project.write("src/lib.rs", LIB);
        project.write(
            "docs/guide.md",
            "# Guide\n\nSee [add](../src/lib.rs#add).\n\nAgain, [add](../src/lib.rs#add).\n",
        );

        let scan = project.scan();

        assert_eq!(scan.links.len(), 1, "both copies should share one link");
        let lines: Vec<u32> = link(&scan, &key("docs/guide.md", "src/lib.rs", Some("add")))
            .locations()
            .map(|location| location.line)
            .collect();
        assert_eq!(
            lines,
            vec![3, 5],
            "every occurrence should be kept, in order"
        );
    }

    #[test]
    fn missing_target_is_broken_with_read_missing() {
        let project = Project::new();
        project.write("docs/guide.md", "See [gone](../src/gone.rs).\n");

        let scan = project.scan();

        let finding = broken(&scan, &key("docs/guide.md", "src/gone.rs", None));
        assert_eq!(finding.code, "read-missing");
        assert_eq!(finding.severity, Severity::Error);
        let location = finding
            .location
            .as_ref()
            .expect("a broken link's finding should point at the link");
        assert_eq!((location.line, location.column), (1, 5));
        assert_eq!(finding.fix, None);
    }

    #[test]
    fn missing_symbol_is_broken_with_nearest_and_fix() {
        let project = Project::new();
        project.write("src/lib.rs", LIB);
        project.write("docs/guide.md", "See [add](../src/lib.rs#ad).\n");

        let scan = project.scan();

        let finding = broken(&scan, &key("docs/guide.md", "src/lib.rs", Some("ad")));
        assert_eq!(finding.code, "resolve-not-found");
        let help = finding.help.as_deref().unwrap_or_default();
        assert!(
            help.contains("`add`"),
            "the help should name the nearest symbol: {help}"
        );
        let fix = finding
            .fix
            .as_ref()
            .expect("one close symbol should give a fix");
        assert_eq!(fix.replacement, "add");
        assert_eq!(fixed_text(&project.read("docs/guide.md"), fix), "ad");
    }

    /// The message and help of the broken link at `docs/guide.md` to `src/lib.rs#symbol`, joined.
    fn resolve_text(project: &Project, symbol: &str) -> String {
        let scan = project.scan();
        let finding = broken(&scan, &key("docs/guide.md", "src/lib.rs", Some(symbol)));
        format!(
            "{}\n{}",
            finding.message,
            finding.help.as_deref().unwrap_or_default()
        )
    }

    #[test]
    fn not_found_names_the_file_relative_to_the_root_not_absolutely() {
        for root in [None, Some("sub/project")] {
            let project = Project::at(root, "");
            project.write("src/lib.rs", LIB);
            project.write("docs/guide.md", "See [add](../src/lib.rs#ad).\n");

            let text = resolve_text(&project, "ad");

            assert_eq!(
                text,
                "no symbol `ad` in src/lib.rs\n\
                 did you mean `add`? run `supdoc resolve src/lib.rs` from the project root to list its symbols",
                "root {root:?}: the message and help should name the file from the project root"
            );
            assert!(
                !text.contains(project.dir.path().to_string_lossy().as_ref()),
                "root {root:?}: no temp-dir prefix should leak into: {text}"
            );
        }
    }

    #[test]
    fn ambiguous_names_the_file_relative_to_the_root_not_absolutely() {
        let project = Project::at(Some("sub/project"), "");
        project.write(
            "src/lib.rs",
            "struct A;\nstruct B;\nimpl A { fn new() {} }\nimpl B { fn new() {} }\n",
        );
        project.write("docs/guide.md", "See [new](../src/lib.rs#new).\n");

        let text = resolve_text(&project, "new");

        assert_eq!(
            text,
            "`new` matches 2 symbols in src/lib.rs\nwrite one of the full paths: `A.new`, `B.new`"
        );
    }

    #[test]
    fn two_near_matches_give_no_fix() {
        let project = Project::new();
        project.write("src/lib.rs", "pub fn add() {}\npub fn ads() {}\n");
        project.write("docs/guide.md", "See [add](../src/lib.rs#ad).\n");

        let scan = project.scan();

        let finding = broken(&scan, &key("docs/guide.md", "src/lib.rs", Some("ad")));
        assert_eq!(finding.code, "resolve-not-found");
        assert_eq!(
            finding.fix, None,
            "two close symbols leave nothing to choose"
        );
    }

    #[test]
    fn unknown_namespace_typo_gets_prefix_fix() {
        let project =
            Project::with_config("[namespaces]\nauth = \"services/auth\"\nweb = \"apps/web\"\n");
        project.write("docs/guide.md", "See [login](atuh:src/lib.rs#login).\n");

        let scan = project.scan();

        assert_eq!(
            scan.findings,
            Vec::new(),
            "the link's problem lives on the link"
        );
        let finding = broken(
            &scan,
            &key("docs/guide.md", "atuh:src/lib.rs", Some("login")),
        );
        assert_eq!(finding.code, "link-unknown-namespace");
        let fix = finding
            .fix
            .as_ref()
            .expect("one close namespace should give a fix");
        assert_eq!(fix.replacement, "auth");
        assert_eq!(fixed_text(&project.read("docs/guide.md"), fix), "atuh");
    }

    #[test]
    fn unknown_namespace_far_from_every_name_gets_no_fix() {
        let project = Project::with_config("[namespaces]\nauth = \"services/auth\"\n");
        project.write("docs/guide.md", "See [x](billing:src/lib.rs#x).\n");

        let scan = project.scan();

        let finding = broken(
            &scan,
            &key("docs/guide.md", "billing:src/lib.rs", Some("x")),
        );
        assert_eq!(finding.code, "link-unknown-namespace");
        assert_eq!(finding.fix, None);
    }

    // Link errors fail `check`

    /// Whether `check` passes on `project`, given `lock`, without and with `--deny-warnings`.
    fn outcomes(project: &Project, lock: &Lockfile) -> [Outcome; 2] {
        let report = reconcile_at_root(&project.scan(), lock, &Scope::All);
        [check_outcome(&report, false), check_outcome(&report, true)]
    }

    #[test]
    fn every_link_error_fails_check() {
        let cases = [
            ("link-unknown-namespace", "[a](atuh:src/lib.rs#add)"),
            ("link-invalid-symbol", "[a](../src/lib.rs#a..b)"),
            ("link-invalid-path", "[a](../src//lib.rs#add)"),
            (
                "link-text-fragment-unsupported",
                "[a](../src/lib.rs#add:~:text=b)",
            ),
        ];
        for (code, link) in cases {
            let project = Project::with_config("[namespaces]\nauth = \"src\"\n");
            project.write("src/lib.rs", LIB);
            project.write("docs/guide.md", &format!("See {link}.\n"));

            let scan = project.scan();
            let codes: Vec<&str> = scan
                .links
                .values()
                .filter_map(|link| match &link.target {
                    TargetState::Broken { finding } => Some(finding.code),
                    TargetState::Present { .. } => None,
                })
                .chain(scan.findings.iter().map(|finding| finding.code))
                .collect();
            assert_eq!(codes, [code], "{link} should report only {code}");
            assert_eq!(
                outcomes(&project, &Lockfile::default()),
                [Outcome::Failing; 2],
                "{code} should fail check, with and without --deny-warnings",
            );
        }
    }

    #[test]
    fn locked_link_whose_namespace_was_removed_is_broken_not_obsolete() {
        let project = Project::new();
        project.write("services/auth/login.rs", "pub fn login() {}\n");
        project.write("docs/guide.md", "See [login](auth:login.rs#login).\n");
        let locked = key("docs/guide.md", "auth:login.rs", Some("login"));
        let lock: Lockfile = [(locked.clone(), Hash::digest(b"pub fn login() {}"))]
            .into_iter()
            .collect();

        let report = reconcile_at_root(&project.scan(), &lock, &Scope::All);

        let states: Vec<(LinkKey, LinkState)> = report
            .links
            .iter()
            .map(|link| (link.key.clone(), link.state))
            .collect();
        assert_eq!(states, [(locked, LinkState::Broken)]);
        assert_eq!(outcomes(&project, &lock), [Outcome::Failing; 2]);
    }

    #[test]
    fn case_mismatch_is_broken_with_fix() {
        let project = Project::new();
        project.write("src/lib.rs", LIB);
        project.write("docs/guide.md", "See [add](../SRC/lib.rs#add).\n");

        let scan = project.scan();

        let finding = broken(&scan, &key("docs/guide.md", "SRC/lib.rs", Some("add")));
        assert_eq!(finding.code, "scan-case-mismatch");
        assert_eq!(
            finding.message,
            "../SRC/lib.rs doesn't match the real path ../src/lib.rs (case differs)"
        );
        let fix = finding
            .fix
            .as_ref()
            .expect("a case mismatch should give a fix");
        assert_eq!(fix.replacement, "../src/lib.rs");
        assert_eq!(
            fixed_text(&project.read("docs/guide.md"), fix),
            "../SRC/lib.rs"
        );
    }

    #[test]
    fn directory_link_is_not_tracked_but_counted() {
        let project = Project::new();
        project.write("src/lib.rs", LIB);
        project.write("src/dir.rs/mod.rs", LIB);
        project.write(
            "docs/guide.md",
            "See [src](../src/), [home](/), [lib](../src#add), [d](../src/dir.rs) and \
             [api](api.html#section).\n",
        );

        let (scan, untracked) = project.scan_untracked();

        assert_eq!(scan.links.len(), 0, "links to a directory are navigation");
        assert_eq!(scan.findings, Vec::new());
        assert_eq!(
            untracked,
            [(Untracked::Directory, 3), (Untracked::UnreadType, 2)],
            "`../src/`, `/` and the `dir.rs` directory are directories; `../src#add` and \
             `api.html#section` name no file a language reads",
        );
    }

    #[test]
    fn unreadable_source_becomes_finding_and_scan_continues() {
        let project = Project::new();
        project.write("src/lib.rs", LIB);
        project.write_bytes("docs/bad.md", b"See [add](../src/lib.rs#add) \xff\n");
        project.write("docs/good.md", "See [add](../src/lib.rs#add).\n");

        let scan = project.scan();

        let codes: Vec<&str> = scan.findings.iter().map(|finding| finding.code).collect();
        assert_eq!(codes, vec!["read-not-utf8"]);
        assert!(
            scan.findings
                .iter()
                .all(|finding| finding.message.starts_with("docs/bad.md is not UTF-8")),
            "the warning should name the file relative to the root: {:?}",
            scan.findings
        );
        assert_eq!(
            scan.unread_sources,
            BTreeSet::from(["docs/bad.md"
                .parse()
                .expect("the fixture source path should be valid")]),
        );
        assert_present(&scan, &key("docs/good.md", "src/lib.rs", Some("add")));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_root_is_fatal() {
        use std::os::unix::fs::PermissionsExt;

        let project = Project::new();
        let config = project.config();
        let locked = fs::Permissions::from_mode(0o000);
        fs::set_permissions(&project.root, locked).expect("the root should be lockable");

        let result = run(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes::none(),
        );

        fs::set_permissions(&project.root, fs::Permissions::from_mode(0o755))
            .expect("the root should be unlockable, so the temp dir can be removed");
        let error = result.expect_err("an unreadable root should stop the scan");
        assert_eq!(error.code(), "scan-root-unreadable");
    }

    /// Links in `docs`, code blocks in `skills` too: `skills` is in code-blocks' scope only.
    const CODE_BLOCKS_WIDER: &str =
        "exclude = [\"skills\"]\n[features.code-blocks]\nexclude = []\n";

    #[cfg(unix)]
    #[test]
    fn a_directory_only_code_blocks_walks_leaves_linking_unchanged_when_unreadable() {
        use std::os::unix::fs::PermissionsExt as _;

        let project = Project::with_config(CODE_BLOCKS_WIDER);
        project.write("src/lib.rs", LIB);
        project.write("docs/guide.md", "No links any more.\n");
        project.write("skills/a.md", "```rust\nfn main() {}\n```\n");
        let gone = key("docs/guide.md", "src/lib.rs", Some("add"));
        let lock: Lockfile = [(gone.clone(), Hash::digest(b"add"))].into_iter().collect();
        let skills = project.root.join("skills");
        fs::set_permissions(&skills, fs::Permissions::from_mode(0o000))
            .expect("the fixture directory permissions should be settable");

        let scan = project.scan();
        let config = project.config();
        let front = read_sources(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &Scope::All,
            &GlobalExcludes::none(),
            &mut Parsed::default(),
            |_source, _file, _raws| {},
            |source, error| panic!("{source} should be readable: {error:?}"),
        );

        fs::set_permissions(&skills, fs::Permissions::from_mode(0o755))
            .expect("the fixture directory should be unlockable, so the temp dir can be removed");
        let front = front.expect("the fixture project should be read");
        assert!(
            scan.unread_dirs.is_empty() && scan.unread_sources.is_empty(),
            "linking's unread state should hold only linking's scope: {:?} {:?}",
            scan.unread_dirs,
            scan.unread_sources
        );
        assert!(
            scan.findings
                .iter()
                .any(|finding| finding.code == "scan-unreadable-dir"),
            "the unreadable directory should still be reported: {:?}",
            scan.findings
        );
        let report = reconcile_at_root(&scan, &lock, &Scope::All);
        let states: Vec<(LinkKey, LinkState)> = report
            .links
            .iter()
            .map(|link| (link.key.clone(), link.state))
            .collect();
        assert_eq!(states, [(gone, LinkState::Obsolete)]);
        assert!(
            front.unread_dirs.is_empty(),
            "namespace rename should not refuse over a directory linking never reads: {:?}",
            front.unread_dirs
        );
    }

    #[test]
    fn a_file_only_code_blocks_reads_is_not_unread_for_linking() {
        let project = Project::with_config(CODE_BLOCKS_WIDER);
        project.write_bytes("skills/bad.md", b"not UTF-8 \xff\n");

        let scan = project.scan();

        assert!(scan.unread_sources.is_empty(), "{:?}", scan.unread_sources);
        let codes: Vec<&str> = scan.findings.iter().map(|finding| finding.code).collect();
        assert_eq!(codes, ["read-not-utf8"]);
    }

    #[test]
    fn unreadable_root_help_gives_chmod_with_the_root_and_no_other_directory_advice() {
        let error = ScanError::RootUnreadable {
            root: AbsolutePath::new(Path::new("/work/my repo")).expect("the path is absolute"),
            error: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };

        let help = error.help();

        assert!(help.contains("`chmod u+rx '/work/my repo'`"), "got: {help}");
        assert!(!help.contains("another directory"), "got: {help}");
    }

    #[test]
    fn target_is_parsed_once_and_a_source_that_is_also_a_target_is_parsed_again() {
        let project = Project::new();
        project.write(
            "docs/guide.md",
            "# Guide\n\nSee [add](../src/lib.rs#add).\n",
        );
        project.write(
            "src/lib.rs",
            &format!("// See [the guide](../docs/guide.md#guide).\n{LIB}"),
        );

        let (scan, parsed) = project.assemble();

        assert_eq!(scan.links.len(), 2, "both files link to each other");
        assert_eq!(
            parsed.parses, 4,
            "each file is parsed once as a source, dropped, then once as a target"
        );
    }

    #[test]
    fn scan_output_is_identical_across_runs() {
        let project = Project::with_config("[namespaces]\nauth = \"services/auth\"\n");
        project.write("src/lib.rs", LIB);
        project.write("services/auth/login.rs", "pub fn login() {}\n");
        project.write(
            "docs/b.md",
            "[a](../src/lib.rs#sub) [b](auth:login.rs#login) [c](../src/gone.rs)\n",
        );
        project.write(
            "docs/a.md",
            "[a](../src/lib.rs) [b](atuh:x.rs) [c](../src/lib.rs#ad)\n",
        );

        assert_eq!(project.scan(), project.scan());
    }

    // Targets are read directly, never through the walk

    #[cfg(unix)]
    #[test]
    fn link_through_symlinked_target_file_resolves() {
        let project = Project::new();
        project.write("src/real.rs", LIB);
        std::os::unix::fs::symlink(Path::new("real.rs"), project.root.join("src/alias.rs"))
            .expect("the fixture symlink should be creatable");
        project.write("docs/guide.md", "See [add](../src/alias.rs#add).\n");

        let scan = project.scan();

        assert_present(&scan, &key("docs/guide.md", "src/alias.rs", Some("add")));
    }

    #[test]
    fn excluded_or_gitignored_target_still_resolves() {
        let project = Project::with_config("exclude = [\"generated\"]\n");
        project.write(".gitignore", "build/\n");
        project.write("generated/api.rs", LIB);
        project.write("build/out.rs", LIB);
        project.write(
            "docs/guide.md",
            "[a](../generated/api.rs#add) and [b](../build/out.rs#sub)\n",
        );

        let scan = project.scan();

        assert_present(
            &scan,
            &key("docs/guide.md", "generated/api.rs", Some("add")),
        );
        assert_present(&scan, &key("docs/guide.md", "build/out.rs", Some("sub")));
    }

    /// A secret outside the project, as a runner might hold one.
    const SECRET: &str = "API_KEY = \"s3cr3t\"\nDEBUG = True\n";

    /// Fails unless `finding` is `link-outside-root` and shows nothing read from outside.
    fn assert_outside_root(finding: &Finding) {
        assert_eq!(finding.code, "link-outside-root", "{finding:?}");
        let shown = format!("{finding:?}");
        for leak in ["s3cr3t", "API_KEY = ", "DEBUG"] {
            assert!(
                !shown.contains(leak),
                "`{leak}` was read from outside: {shown}"
            );
        }
    }

    #[test]
    fn climbing_links_are_outside_root_and_never_keyed() {
        let project = Project::at(Some("project"), "");
        project.write_outside("home/settings.py", SECRET);
        project.write(
            "docs/guide.md",
            "[a](../../home/settings.py#API_KEY)\n\n[b](/../home/settings.py#API_KEY)\n\n\
             [c](../../home/settings.py#API_KEX)\n\n[d](../../missing.py#X)\n",
        );

        let scan = project.scan();

        assert_eq!(
            scan.links.len(),
            0,
            "nothing outside is keyed: {:?}",
            scan.links
        );
        assert_eq!(scan.findings.len(), 4, "{:?}", scan.findings);
        for finding in &scan.findings {
            assert_outside_root(finding);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_file_outside_the_root_is_not_read() {
        let project = Project::at(Some("project"), "");
        project.write_outside("home/settings.py", SECRET);
        project.write("docs/guide.md", "[a](pw.py)\n\n[b](pw.py#API_KEX)\n");
        std::os::unix::fs::symlink(
            project.dir.path().join("home/settings.py"),
            project.root.join("docs/pw.py"),
        )
        .expect("the symlink should be creatable");

        let scan = project.scan();

        for key in [
            key("docs/guide.md", "docs/pw.py", None),
            key("docs/guide.md", "docs/pw.py", Some("API_KEX")),
        ] {
            assert_outside_root(broken(&scan, &key));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_outside_the_root_is_not_read() {
        let project = Project::at(Some("project"), "");
        project.write_outside("home/settings.py", SECRET);
        project.write("docs/guide.md", "[a](/vendor/settings.py#API_KEY)\n");
        std::os::unix::fs::symlink(project.dir.path().join("home"), project.root.join("vendor"))
            .expect("the symlink should be creatable");

        let scan = project.scan();

        let finding = broken(
            &scan,
            &key("docs/guide.md", "vendor/settings.py", Some("API_KEY")),
        );
        assert_outside_root(finding);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_pointing_outside_looks_like_any_other_outside_link() {
        let project = Project::at(Some("project"), "");
        project.write("docs/guide.md", "[a](gone.py#X)\n");
        std::os::unix::fs::symlink(
            project.dir.path().join("home/gone.py"),
            project.root.join("docs/gone.py"),
        )
        .expect("the symlink should be creatable");

        let scan = project.scan();

        assert_outside_root(broken(
            &scan,
            &key("docs/guide.md", "docs/gone.py", Some("X")),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_the_root_still_resolves() {
        let project = Project::new();
        project.write("src/math.rs", LIB);
        project.write("docs/guide.md", "See [add](alias.rs#add).\n");
        std::os::unix::fs::symlink(
            project.root.join("src/math.rs"),
            project.root.join("docs/alias.rs"),
        )
        .expect("the symlink should be creatable");

        let scan = project.scan();

        assert_present(&scan, &key("docs/guide.md", "docs/alias.rs", Some("add")));
    }

    #[test]
    fn namespaced_target_resolves_in_its_directory() {
        let project = Project::with_config("[namespaces]\nauth = \"services/auth\"\n");
        project.write("services/auth/login.rs", "pub fn login() {}\n");
        project.write("docs/guide.md", "See [login](auth:login.rs#login).\n");

        let scan = project.scan();

        let key = key("docs/guide.md", "auth:login.rs", Some("login"));
        assert!(matches!(key.target, Target::Namespaced { .. }));
        assert_present(&scan, &key);
    }

    // Fixes must be verifiable by `supdoc fix`

    #[test]
    fn every_fix_lies_inside_its_findings_location_and_snippet() {
        let project = Project::with_config("[namespaces]\nauth = \"services/auth\"\n");
        project.write("src/lib.rs", LIB);
        project.write(
            "docs/guide.md",
            "# Guide\n\nSee [a](../src/lib.rs#ad),\n[b](../SRC/lib.rs#add) and\n\
             [c](atuh:x.rs#y).\n",
        );
        let text = project.read("docs/guide.md");

        let scan = project.scan();

        let linked = scan.links.values().filter_map(|link| match &link.target {
            TargetState::Broken { finding } => Some(finding),
            TargetState::Present { .. } => None,
        });
        let fixable: Vec<&Finding> = linked
            .chain(&scan.findings)
            .filter(|finding| finding.fix.is_some())
            .collect();
        let mut codes: Vec<&str> = fixable.iter().map(|finding| finding.code).collect();
        codes.sort_unstable();
        assert_eq!(
            codes,
            vec![
                "link-unknown-namespace",
                "resolve-not-found",
                "scan-case-mismatch"
            ],
        );

        for finding in fixable {
            let code = finding.code;
            let (Some(fix), Some(location), Some(snippet)) =
                (&finding.fix, &finding.location, &finding.snippet)
            else {
                panic!("{code}: a fix needs a location and a snippet to be verified: {finding:?}");
            };
            assert_eq!(
                fix.file, location.file,
                "{code}: the fix edits another file"
            );
            let start = location.range.start - snippet.highlight.start;
            let end = start + snippet.text.len();
            assert!(
                start <= fix.range.start && fix.range.end <= end,
                "{code}: the fix {:?} lies outside the snippet's bytes {start}..{end}",
                fix.range,
            );
            assert_eq!(
                text.get(start..end),
                Some(snippet.text.as_str()),
                "{code}: the snippet should be the file's own bytes at its position",
            );
        }
    }

    /// Rust whose `tags.scm` doesn't compile, standing in for a broken built-in.
    static BROKEN_RUST_QUERIES: std::sync::LazyLock<
        Result<crate::read::query::QueryCartridge, crate::read::query::QueryFileError>,
    > = std::sync::LazyLock::new(|| {
        crate::read::query::QueryCartridge::new(
            crate::cartridges::rust::ID,
            crate::cartridges::rust::EXTENSIONS,
            crate::cartridges::rust::grammar(),
            "(no_such_node) @name",
            "",
        )
    });

    /// The Rust cartridge, registered the way the built-ins are, with queries that don't compile.
    static BROKEN_RUST: crate::read::registry::LazyCartridge =
        crate::read::registry::LazyCartridge::new(
            crate::cartridges::rust::ID,
            crate::cartridges::rust::EXTENSIONS,
            crate::cartridges::rust::grammar,
            || crate::read::registry::compiled(BROKEN_RUST_QUERIES.as_ref()),
        );

    /// Scans `project` with Markdown and the broken Rust cartridge.
    fn scan_with_broken_rust(project: &Project) -> Result<Scan, ScanError> {
        let languages = Languages::new(vec![
            &BROKEN_RUST,
            crate::cartridges::markdown::cartridge::cartridge(),
        ]);
        run(
            &project.config(),
            &languages,
            &Scope::All,
            &GlobalExcludes::none(),
        )
    }

    /// The code and exit status `error` gives a command.
    fn cli_failure(error: ScanError) -> (&'static str, u8) {
        let error = crate::cli::CliError::from(error);
        (error.code(), error.exit_code())
    }

    #[test]
    fn a_broken_builtin_is_not_compiled_until_a_file_of_its_language_is_read() {
        let project = Project::new();
        project.write("docs/guide.md", "# Guide\n\nSee [the guide](other.md).\n");
        project.write("docs/other.md", "# Other\n");

        let scan = scan_with_broken_rust(&project);

        assert!(
            scan.is_ok(),
            "a project with no Rust file should scan with a broken Rust cartridge: {scan:?}"
        );
    }

    #[test]
    fn a_broken_builtin_fails_the_run_with_exit_70_when_a_source_of_its_language_is_read() {
        let project = Project::new();
        project.write("docs/guide.md", "# Guide\n");
        project.write("src/lib.rs", LIB);

        let error = scan_with_broken_rust(&project)
            .expect_err("reading a Rust source should fail the scan, not become a finding");

        assert_eq!(cli_failure(error), ("read-cartridge-load", 70));
    }

    #[test]
    fn a_broken_builtin_fails_the_run_with_exit_70_when_a_target_of_its_language_is_read() {
        let project = Project::with_config("include = [\"docs/**\"]\n");
        project.write("docs/guide.md", "See [add](../src/lib.rs#add).\n");
        project.write("src/lib.rs", LIB);

        let error = scan_with_broken_rust(&project)
            .expect_err("reading a Rust target should fail the scan, not break the link");

        assert_eq!(cli_failure(error), ("read-cartridge-load", 70));
    }
}
