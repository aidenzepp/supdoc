//! Grouping links by target and reading each target once (04, From links to targets, steps 3-6).
//!
//! Every distinct target is located on disk, checked for case (S12) and for being a directory
//! (S13), then read through [`Parsed`] so a file that is also a source is never parsed twice (S9).
//! Each link into it then hashes the whole file or the one symbol it names. Anything that goes
//! wrong becomes that link's [`TargetState::Broken`] finding, with a [`Fix`] only when exactly one
//! edit is right.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::ffi::OsStr;
use std::fs;
use std::ops::Range;
use std::path::Path;

use crate::config::{CONFIG_FILE_NAME, Config};
use crate::finding::{Finding, Fix, Location, Severity, Snippet};
use crate::link::{LinkKey, Target};
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile, SymbolTable};
use crate::read::error::{ReadError, ResolveError};
use crate::read::hash::{hash_file, hash_symbol};
use crate::scan::model::{ScannedLink, TargetState};

/// Finding code for a target path whose case differs from the real path (04 S12).
const CODE_CASE_MISMATCH: &str = "scan/case-mismatch";

/// Finding code for a namespace the config doesn't define (03 R10).
pub(crate) const CODE_UNKNOWN_NAMESPACE: &str = "link/unknown-namespace";

/// Files parsed during one run, keyed by path (04 S9). Owned by one scan, never global.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    /// Every file read so far.
    files: BTreeMap<AbsolutePath, SourceFile>,
    /// How many files were parsed, so a test can prove none was parsed twice.
    #[cfg(test)]
    pub(crate) parses: usize,
}

impl Parsed {
    /// The file at `path`, read and parsed on the first request and reused after that.
    ///
    /// A failed read is not remembered, so asking again reads again; it parsed nothing.
    ///
    /// # Errors
    ///
    /// The [`ReadError`] from [`SourceFile::read`].
    pub(crate) fn read(
        &mut self,
        path: &AbsolutePath,
        languages: &Languages,
    ) -> Result<&SourceFile, ReadError> {
        match self.files.entry(path.clone()) {
            Entry::Occupied(parsed) => Ok(parsed.into_mut()),
            Entry::Vacant(slot) => {
                let file = SourceFile::read(path.clone(), languages)?;
                #[cfg(test)]
                {
                    self.parses += 1;
                }
                Ok(slot.insert(file))
            }
        }
    }
}

/// One place a link is written, with what a finding about it needs from the source text.
#[derive(Debug, Clone)]
pub(crate) struct Occurrence {
    /// The link's identity.
    pub(crate) key: LinkKey,
    /// Where the whole link is.
    pub(crate) location: Location,
    /// The lines the link covers, captured while the source text is at hand (07 G11).
    pub(crate) snippet: Option<Snippet>,
    /// The destination as written, when it can be found inside the link itself.
    pub(crate) written: Option<Written>,
}

/// A link destination exactly as written in its source, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    /// The destination's bytes in the source file.
    range: Range<usize>,
    /// The destination's text.
    text: String,
}

impl Written {
    /// Finds `destination` inside the link at `link` in `text`: the one place it follows `](`,
    /// optionally after whitespace and `<`.
    ///
    /// `None` when it isn't written there exactly once, as for a reference link whose destination
    /// lives in its definition. A fix is never guessed from such a link.
    pub(crate) fn locate(text: &str, link: &Range<usize>, destination: &str) -> Option<Self> {
        let destination = destination.trim();
        let link_text = text.get(link.clone())?;
        if destination.is_empty() {
            return None;
        }
        let mut starts = link_text
            .match_indices(destination)
            .map(|(start, _)| start)
            .filter(|start| {
                link_text.get(..*start).is_some_and(|before| {
                    let before = before.strip_suffix('<').unwrap_or(before);
                    before.trim_end().ends_with("](")
                })
            });
        let (Some(start), None) = (starts.next(), starts.next()) else {
            return None;
        };
        let start = link.start + start;
        Some(Self {
            range: start..start + destination.len(),
            text: destination.to_owned(),
        })
    }

    /// The range of `prefix` when the destination starts with `prefix:`.
    pub(crate) fn prefix_range(&self, prefix: &str) -> Option<Range<usize>> {
        self.text
            .strip_prefix(prefix)?
            .starts_with(':')
            .then(|| self.range.start..self.range.start + prefix.len())
    }

    /// The range of the fragment after `#`, when there is one.
    fn fragment_range(&self) -> Option<Range<usize>> {
        let hash = self.text.find('#')?;
        let start = self.range.start + hash + 1;
        (start < self.range.end).then_some(start..self.range.end)
    }

    /// The path as written, between any `namespace:` prefix and any `#fragment`, with its range.
    fn path(&self, namespaced: bool) -> Option<(&str, Range<usize>)> {
        let start = if namespaced {
            self.text.find(':')? + 1
        } else {
            0
        };
        let end = self.text.find('#').unwrap_or(self.text.len());
        let path = self.text.get(start..end)?;
        Some((path, self.range.start + start..self.range.start + end))
    }
}

/// Resolves every link: groups them by key (S11) and by target, reads each target once, and
/// records what each target looks like now. Links to a directory are dropped (S13).
///
/// Effect: reads target files and the directory listings along their paths.
pub(crate) fn resolve(
    config: &Config,
    languages: &Languages,
    parsed: &mut Parsed,
    occurrences: Vec<Occurrence>,
) -> BTreeMap<LinkKey, ScannedLink> {
    let mut by_target: BTreeMap<Target, BTreeMap<LinkKey, Vec<Occurrence>>> = BTreeMap::new();
    for occurrence in occurrences {
        by_target
            .entry(occurrence.key.target.clone())
            .or_default()
            .entry(occurrence.key.clone())
            .or_default()
            .push(occurrence);
    }

    let mut links = BTreeMap::new();
    for (target, keyed) in by_target {
        for (key, state, occurrences) in target_states(config, languages, parsed, &target, keyed) {
            let snippet = occurrences
                .first()
                .and_then(|occurrence| occurrence.snippet.clone());
            let link = ScannedLink {
                snippet,
                locations: occurrences
                    .into_iter()
                    .map(|occurrence| occurrence.location)
                    .collect(),
                target: state,
            };
            let _replaced = links.insert(key, link);
        }
    }
    links
}

/// The state of each link into `target`, or nothing when the target isn't tracked.
fn target_states(
    config: &Config,
    languages: &Languages,
    parsed: &mut Parsed,
    target: &Target,
    keyed: BTreeMap<LinkKey, Vec<Occurrence>>,
) -> Vec<(LinkKey, TargetState, Vec<Occurrence>)> {
    let broken_by = |finding: &dyn Fn(&Occurrence) -> Finding| {
        keyed
            .iter()
            .filter_map(|(key, occurrences)| {
                let finding = finding(occurrences.first()?);
                Some((
                    key.clone(),
                    TargetState::Broken { finding },
                    occurrences.clone(),
                ))
            })
            .collect::<Vec<_>>()
    };

    let path = match locate(config, target) {
        Located::Directory => return Vec::new(),
        Located::UnknownNamespace => return broken_by(&|first| unknown_namespace(target, first)),
        Located::CaseMismatch(real) => {
            return broken_by(&|first| case_mismatch(target, &real, first));
        }
        Located::File(path) => path,
    };
    let file = match parsed.read(&path, languages) {
        Ok(file) => file,
        Err(error) => return broken_by(&|first| read_failed(&error, first)),
    };

    let mut table: Option<SymbolTable> = None;
    keyed
        .into_iter()
        .filter_map(|(key, occurrences)| {
            let first = occurrences.first()?;
            let syntax_errors = file.has_syntax_errors();
            let state = match &key.symbol {
                None => TargetState::Present {
                    hash: hash_file(file),
                    syntax_errors,
                },
                Some(symbol) => {
                    let table = table.get_or_insert_with(|| file.language().symbols(file));
                    match table.resolve(file.path(), symbol) {
                        Ok(found) => TargetState::Present {
                            hash: hash_symbol(file, found),
                            syntax_errors,
                        },
                        Err(error) => TargetState::Broken {
                            finding: resolve_failed(&error, table, first),
                        },
                    }
                }
            };
            Some((key, state, occurrences))
        })
        .collect()
}

/// Where a target is on disk.
#[derive(Debug)]
enum Located {
    /// A path to read. It may still be missing: reading it says so.
    File(AbsolutePath),
    /// A directory, which is navigation and never tracked (S13).
    Directory,
    /// The path names a real file, but with different case (S12). Holds the real target path,
    /// written like the target's own path.
    CaseMismatch(String),
    /// The namespace isn't in the config. `parse_destination` rejects these first, so a scan
    /// never meets one, but a target is never resolved against a guessed directory.
    UnknownNamespace,
}

/// Finds `target` on disk: against the root, or against its namespace's directory (01).
///
/// Effect: lists the directories along the path to compare its case with the real names.
fn locate(config: &Config, target: &Target) -> Located {
    let (base, path) = match target {
        Target::Path(path) => (config.root(), path),
        Target::Namespaced { namespace, path } => match config.namespace(namespace) {
            Some(directory) => (directory, path),
            None => return Located::UnknownNamespace,
        },
    };
    let segments: Vec<&str> = path.as_str().split('/').collect();
    let climbs = segments
        .iter()
        .take_while(|segment| **segment == "..")
        .count();
    let (up, down) = segments.split_at(climbs);

    // Joining a relative path onto an absolute one is always absolute, so the base never stands
    // in for the file.
    let start =
        AbsolutePath::new(&base.as_path().join(up.join("/"))).unwrap_or_else(|| base.clone());
    if let Some(real) = real_case(start.as_path(), down) {
        let real: Vec<&str> = up
            .iter()
            .copied()
            .chain(real.iter().map(String::as_str))
            .collect();
        return Located::CaseMismatch(real.join("/"));
    }

    let file =
        AbsolutePath::new(&base.as_path().join(path.as_str())).unwrap_or_else(|| base.clone());
    if fs::metadata(file.as_path()).is_ok_and(|metadata| metadata.is_dir()) {
        return Located::Directory;
    }
    Located::File(file)
}

/// The real names of `segments` beneath `start`, when they exist on disk but at least one is
/// spelled with different case.
///
/// `None` when every name matches exactly, when one is missing or matches several entries
/// case-insensitively, when a directory can't be listed, or when `..` follows a name: reading the
/// file then reports what is wrong.
fn real_case(start: &Path, segments: &[&str]) -> Option<Vec<String>> {
    let mut directory = start.to_path_buf();
    let mut real = Vec::with_capacity(segments.len());
    let mut differs = false;
    for segment in segments {
        if *segment == ".." {
            return None;
        }
        let names: Vec<_> = fs::read_dir(&directory)
            .ok()?
            .filter_map(|entry| Some(entry.ok()?.file_name()))
            .collect();
        let name = if names
            .iter()
            .any(|name| name.as_os_str() == OsStr::new(segment))
        {
            (*segment).to_owned()
        } else {
            let wanted = segment.to_lowercase();
            let mut matching = names
                .iter()
                .filter_map(|name| name.to_str())
                .filter(|name| name.to_lowercase() == wanted);
            let (Some(only), None) = (matching.next(), matching.next()) else {
                return None;
            };
            differs = true;
            only.to_owned()
        };
        directory.push(&name);
        real.push(name);
    }
    differs.then_some(real)
}

/// A broken link's finding, at its first occurrence, with that occurrence's snippet.
fn broken(code: &'static str, message: String, help: String, first: &Occurrence) -> Finding {
    let finding = Finding::new(code, Severity::Error, message)
        .with_location(first.location.clone())
        .with_help(help);
    match &first.snippet {
        Some(snippet) => finding.with_snippet(snippet.clone()),
        None => finding,
    }
}

/// The edit replacing `range` in the occurrence's source with `replacement`.
fn fix_at(first: &Occurrence, range: Range<usize>, replacement: impl Into<String>) -> Fix {
    Fix {
        file: first.location.file.clone(),
        range,
        replacement: replacement.into(),
    }
}

/// `scan/case-mismatch`: the path names a real file with different case, fixed by writing the
/// real case where the link's own spelling can be rewritten exactly.
fn case_mismatch(target: &Target, real: &str, first: &Occurrence) -> Finding {
    let namespaced = target.namespace().is_some();
    let written = first
        .written
        .as_ref()
        .and_then(|written| written.path(namespaced));
    let rewrite = written.as_ref().and_then(|(path, range)| {
        let replacement = recased(path, real)?;
        Some((*path, range.clone(), replacement))
    });
    if let Some((path, range, replacement)) = rewrite {
        return broken(
            CODE_CASE_MISMATCH,
            format!("{path} doesn't match the real path {replacement} (case differs)"),
            format!("Change the link to `{replacement}`. `supdoc fix` can do it"),
            first,
        )
        .with_fix(fix_at(first, range, replacement));
    }
    let real = match target.namespace() {
        Some(namespace) => format!("{namespace}:{real}"),
        None => real.to_owned(),
    };
    broken(
        CODE_CASE_MISMATCH,
        format!("{target} doesn't match the real path {real} (case differs)"),
        format!("Change the link to match the real path `{real}` exactly"),
        first,
    )
}

/// `written` with its names replaced by the last names of `real`, keeping its leading `/`, `.`
/// and `..` segments. `None` when that can't be done exactly: an escape, a `..` after a name, or
/// a name that differs from the real one by more than case.
fn recased(written: &str, real: &str) -> Option<String> {
    if written.contains(['%', '\\']) {
        return None;
    }
    let (anchor, relative) = match written.strip_prefix('/') {
        Some(relative) => ("/", relative),
        None => ("", written),
    };
    let segments: Vec<&str> = relative.split('/').collect();
    let leading = segments
        .iter()
        .take_while(|segment| matches!(**segment, "." | ".."))
        .count();
    let (dots, names) = segments.split_at(leading);
    if names.iter().any(|name| matches!(*name, "" | "." | "..")) {
        return None;
    }

    let real: Vec<&str> = real.split('/').collect();
    let real_names = real.get(real.len().checked_sub(names.len())?..)?;
    let same_letters = names
        .iter()
        .zip(real_names)
        .all(|(name, real)| name.to_lowercase() == real.to_lowercase());
    same_letters.then(|| {
        let joined: Vec<&str> = dots.iter().chain(real_names).copied().collect();
        format!("{anchor}{}", joined.join("/"))
    })
}

/// A `link/unknown-namespace` finding for a namespace the config doesn't define.
fn unknown_namespace(target: &Target, first: &Occurrence) -> Finding {
    let namespace = target
        .namespace()
        .map_or_else(String::new, ToString::to_string);
    broken(
        CODE_UNKNOWN_NAMESPACE,
        format!("unknown namespace `{namespace}` in `{target}`"),
        format!("Define `{namespace}` under `[namespaces]` in {CONFIG_FILE_NAME}"),
        first,
    )
}

/// The finding for a target that couldn't be read, with the error's own code and help.
fn read_failed(error: &ReadError, first: &Occurrence) -> Finding {
    broken(error.code(), error.to_string(), error.help(), first)
}

/// The finding for a symbol that doesn't resolve. A `resolve/not-found` gets a fix when exactly
/// one symbol in the file is a close match (03: the fixer never guesses).
fn resolve_failed(error: &ResolveError, table: &SymbolTable, first: &Occurrence) -> Finding {
    let finding = broken(error.code(), error.to_string(), error.help(), first);
    let ResolveError::NotFound { query, .. } = error else {
        return finding;
    };
    let names: Vec<String> = table
        .iter()
        .flat_map(|symbol| std::iter::once(&symbol.path).chain(&symbol.aliases))
        .map(ToString::to_string)
        .collect();
    let close = close_match(&query.to_string(), names.iter().map(String::as_str));
    let range = first.written.as_ref().and_then(Written::fragment_range);
    match close.zip(range) {
        Some((close, range)) => finding.with_fix(fix_at(first, range, close)),
        None => finding,
    }
}

/// The one candidate that is a likely misspelling of `wanted`, if exactly one is.
///
/// Close means within `max(len, 3) / 3` single-character edits of `wanted`, counting a swap of
/// neighbors as one edit, so `atuh` is close to `auth`. Repeated candidates count once.
pub(crate) fn close_match<'c>(
    wanted: &str,
    candidates: impl IntoIterator<Item = &'c str>,
) -> Option<&'c str> {
    let limit = wanted.chars().count().max(3) / 3;
    let mut close: Vec<&str> = candidates
        .into_iter()
        .filter(|candidate| *candidate != wanted)
        .filter(|candidate| strsim::osa_distance(wanted, candidate) <= limit)
        .collect();
    close.sort_unstable();
    close.dedup();
    match close.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}
