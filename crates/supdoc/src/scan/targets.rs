//! Grouping links by target and reading each target once (04, From links to targets, steps 3-6).
//!
//! Every distinct target is located on disk, checked to lie inside the project root once symlinks
//! are followed, checked for case (S12) and for being a directory (S13), then read through [`Parsed`] so a target is parsed once. A source is dropped once its links are read, so one that is also a target is parsed a second time (S9).
//! Each link into it then hashes the whole file or the one symbol it names. Anything that goes
//! wrong becomes that link's [`TargetState::Broken`] finding, with a [`Fix`] only when exactly one
//! edit is right.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::ops::Range;
use std::path::Path;

use crate::config::Config;
use crate::finding::{Finding, Fix, Location, Severity, Snippet};
use crate::hash::HashSettings;
use crate::limits::TimeLimit;
use crate::link::{LinkKey, Target};
use crate::namespace::NamespaceName;
use crate::output::paths::relative_to;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile, SymbolTable};
use crate::read::error::{ReadError, ReadRole, ResolveError};
use crate::read::hash::{hash_file, hash_symbol};
use crate::scan::model::{ScannedLink, TargetState};
use crate::suggest::close_match;

/// Finding code for a target path whose case differs from the real path (04 S12).
const CODE_CASE_MISMATCH: &str = "scan-case-mismatch";

/// Finding code for a namespace the config doesn't define (03 R10).
pub(crate) const CODE_UNKNOWN_NAMESPACE: &str = "link-unknown-namespace";

/// Finding code for a link whose target lies outside the project root.
pub(crate) const CODE_OUTSIDE_ROOT: &str = "link-outside-root";

/// What to do about a link whose target lies outside the project root.
pub(crate) const OUTSIDE_ROOT_HELP: &str = "Link to a file inside the project root, or move \
     `supdoc.config.toml` up to a directory that holds both";

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
        limit: TimeLimit,
    ) -> Result<&SourceFile, ReadError> {
        match self.files.entry(path.clone()) {
            Entry::Occupied(parsed) => Ok(parsed.into_mut()),
            Entry::Vacant(slot) => {
                let file = SourceFile::read(path.clone(), languages, limit)?;
                #[cfg(test)]
                {
                    self.parses += 1;
                }
                Ok(slot.insert(file))
            }
        }
    }

    /// Drops every file except those in `keep`, so sources that are never read as targets free
    /// their text and tree before any target is hashed.
    fn retain_only(&mut self, keep: &BTreeSet<AbsolutePath>) {
        self.files.retain(|path, _file| keep.contains(path));
    }

    /// Drops the file at `path` once its last link has been hashed.
    pub(crate) fn forget(&mut self, path: &AbsolutePath) {
        let _dropped = self.files.remove(path);
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

    /// The source bytes at `range`, a part of this destination, as scanned.
    pub(crate) fn text_at(&self, range: &Range<usize>) -> Option<Box<str>> {
        let from = range.start.checked_sub(self.range.start)?;
        let to = range.end.checked_sub(self.range.start)?;
        self.text.get(from..to).map(Box::from)
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
    settings: HashSettings,
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

    let located: Vec<_> = by_target
        .into_iter()
        .map(|(target, keyed)| (locate(config, &target), target, keyed))
        .collect();
    let to_read: BTreeSet<AbsolutePath> = located
        .iter()
        .filter_map(|(at, _target, _keyed)| match at {
            Located::File(path) => Some(path.clone()),
            _ => None,
        })
        .collect();
    parsed.retain_only(&to_read);

    let mut links = BTreeMap::new();
    for (at, target, keyed) in located {
        for (key, state, occurrences) in
            target_states(config, languages, parsed, settings, at, &target, keyed)
        {
            let mut occurrences = occurrences.into_iter();
            let Some(first) = occurrences.next() else {
                continue;
            };
            let link = ScannedLink {
                snippet: first.snippet,
                first: first.location,
                more: occurrences.map(|occurrence| occurrence.location).collect(),
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
    settings: HashSettings,
    at: Located,
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

    let path = match at {
        Located::Directory => return Vec::new(),
        Located::UnknownNamespace => {
            return broken_by(&|first| unknown_namespace(config, target, first));
        }
        Located::OutsideRoot => return broken_by(&|first| outside_root(target, first)),
        Located::CaseMismatch(real) => {
            return broken_by(&|first| case_mismatch(target, &real, first));
        }
        Located::File(path) => path,
    };
    let limit = config.limits_in_effect().parse_timeout.value;
    let file = match parsed.read(&path, languages, limit) {
        Ok(file) => file,
        Err(error) => return broken_by(&|first| read_failed(&error, config.root(), first)),
    };
    // Symbols are read once, before any link, so a deadline that stops the query breaks every
    // link into the file alike and no link resolves against a partial table.
    let needs_symbols = keyed.keys().any(|key| key.symbol.is_some());
    let table = if needs_symbols {
        match file.language().symbols(file) {
            Ok(table) => Some(table),
            Err(error) => {
                let states = broken_by(&|first| read_failed(&error, config.root(), first));
                parsed.forget(&path);
                return states;
            }
        }
    } else {
        None
    };

    let states: Vec<_> = keyed
        .into_iter()
        .filter_map(|(key, occurrences)| {
            let first = occurrences.first()?;
            let syntax_errors = file.has_syntax_errors();
            let state = match &key.symbol {
                None => TargetState::Present {
                    hash: hash_file(file, settings),
                    syntax_errors,
                },
                Some(symbol) => {
                    let table = table.as_ref()?;
                    match table.resolve(file.path(), symbol) {
                        Ok(found) => match hash_symbol(file, found, settings) {
                            Ok(hash) => TargetState::Present {
                                hash,
                                syntax_errors,
                            },
                            Err(_outside) => TargetState::Broken {
                                finding: read_failed(
                                    &ReadError::Internal {
                                        path: file.path().clone(),
                                        reason: "a symbol's byte range lies outside the file",
                                    },
                                    config.root(),
                                    first,
                                ),
                            },
                        },
                        Err(error) => TargetState::Broken {
                            finding: resolve_failed(&error, config.root(), table, first),
                        },
                    }
                }
            };
            Some((key, state, occurrences))
        })
        .collect();
    parsed.forget(&path);
    states
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
    /// The target lies outside the project root once symlinks are followed, or climbs out with
    /// `..`. Nothing about it is read: no contents, no hash, no symbol names.
    OutsideRoot,
}

/// Finds `target` on disk: against the root, or against its namespace's directory (01).
///
/// The target must lie inside the project root once every symlink along it is followed, so an
/// in-root symlink to a file outside, even a dangling one, is [`Located::OutsideRoot`].
///
/// Effect: follows symlinks along the path and lists the in-root directories along it to compare
/// its case with the real names.
fn locate(config: &Config, target: &Target) -> Located {
    let (base, path) = match target {
        Target::Path(path) => (config.root(), path),
        Target::Namespaced { namespace, path } => match config.namespace(namespace) {
            Some(directory) => (directory, path),
            None => return Located::UnknownNamespace,
        },
    };
    let segments: Vec<&str> = path.as_str().split('/').collect();
    let file = base.join_path(Path::new(path.as_str()));
    if !config.contains(base.as_path()) || !config.contains(file.as_path()) {
        return Located::OutsideRoot;
    }

    if let Some(real) = real_case(config, base.as_path(), &segments) {
        if !config.contains(&base.as_path().join(real.join("/"))) {
            return Located::OutsideRoot;
        }
        return Located::CaseMismatch(real.join("/"));
    }
    if fs::metadata(file.as_path()).is_ok_and(|metadata| metadata.is_dir()) {
        return Located::Directory;
    }
    Located::File(file)
}

/// The real names of `segments` beneath `start`, when they exist on disk but at least one is
/// spelled with different case.
///
/// `None` when every name matches exactly, when one is missing or matches several entries
/// case-insensitively, or when a directory can't be listed or lies outside the project root:
/// reading the file then reports what is wrong.
fn real_case(config: &Config, start: &Path, segments: &[&str]) -> Option<Vec<String>> {
    let mut directory = start.to_path_buf();
    let mut real = Vec::with_capacity(segments.len());
    let mut differs = false;
    for segment in segments {
        if !config.contains(&directory) {
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

/// The edit replacing `range` in the occurrence's source with `replacement`, remembering the
/// bytes `range` held in the scanned text. `None` when `range` is not inside the written
/// destination.
fn fix_at(first: &Occurrence, range: Range<usize>, replacement: impl Into<String>) -> Option<Fix> {
    let scanned = first.written.as_ref()?.text_at(&range)?;
    Some(Fix {
        file: first.location.file.clone(),
        range,
        replacement: replacement.into(),
        scanned,
    })
}

/// `scan-case-mismatch`: the path names a real file with different case, fixed by writing the
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
        let finding = broken(
            CODE_CASE_MISMATCH,
            format!("{path} doesn't match the real path {replacement} (case differs)"),
            format!("Change the link to `{replacement}`. `supdoc fix` can do it"),
            first,
        );
        return match fix_at(first, range, replacement) {
            Some(fix) => finding.with_fix(fix),
            None => finding,
        };
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

/// A `link-outside-root` finding for a target that lies outside the project root.
///
/// The message names only the target as written: nothing outside the root is read to build it.
fn outside_root(target: &Target, first: &Occurrence) -> Finding {
    broken(
        CODE_OUTSIDE_ROOT,
        format!("`{target}` is outside the project root"),
        OUTSIDE_ROOT_HELP.to_owned(),
        first,
    )
}

/// A `link-unknown-namespace` finding for a namespace the config doesn't define.
fn unknown_namespace(config: &Config, target: &Target, first: &Occurrence) -> Finding {
    let namespace = target
        .namespace()
        .map_or_else(String::new, ToString::to_string);
    let known: Vec<&str> = config
        .namespaces()
        .keys()
        .map(NamespaceName::as_str)
        .collect();
    broken(
        CODE_UNKNOWN_NAMESPACE,
        format!("unknown namespace `{namespace}` in `{target}`"),
        crate::namespace::unknown_namespace_help(&namespace, &known),
        first,
    )
}

/// The finding for a target that couldn't be read, with the error's own code and help.
///
/// The message names the file relative to the project `root`, so no finding carries a
/// machine-specific absolute path.
fn read_failed(error: &ReadError, root: &AbsolutePath, first: &Occurrence) -> Finding {
    let shown = relative_to(root, error.path());
    broken(
        error.code(),
        error.message_naming(&shown),
        error.help(ReadRole::Target, &shown),
        first,
    )
}

/// The finding for a symbol that doesn't resolve. A `resolve-not-found` gets a fix when exactly
/// one symbol in the file is a close match (03: the fixer never guesses).
///
/// The message and help name the file relative to the project `root`, so no finding carries a
/// machine-specific absolute path. A file outside the root climbs out of it with `..`.
fn resolve_failed(
    error: &ResolveError,
    root: &AbsolutePath,
    table: &SymbolTable,
    first: &Occurrence,
) -> Finding {
    let shown = relative_to(root, error.file());
    let finding = broken(
        error.code(),
        error.message_naming(&shown),
        error.help_from_root(&shown),
        first,
    );
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
    match close
        .zip(range)
        .and_then(|(close, range)| fix_at(first, range, close))
    {
        Some(fix) => finding.with_fix(fix),
        None => finding,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::Config;
    use crate::hash::{HashSettings, LineEndings};
    use crate::limits::Limits;
    use crate::link::SourcePath;

    fn absolute(raw: &str) -> AbsolutePath {
        AbsolutePath::new(Path::new(raw)).expect("the fixture path should be absolute")
    }

    /// The first place `docs/a.md` links to `target`.
    fn occurrence(target: &str) -> Occurrence {
        let source: SourcePath = "docs/a.md"
            .parse()
            .expect("the fixture source should be valid");
        Occurrence {
            key: LinkKey {
                source: source.clone(),
                target: target.parse().expect("the fixture target should be valid"),
                symbol: None,
            },
            location: Location {
                file: source,
                range: 0..1,
                line: 1,
                column: 1,
            },
            snippet: None,
            written: None,
        }
    }

    #[test]
    fn read_failures_name_the_file_relative_to_the_root_not_absolutely() {
        let root = absolute("/home/me/project");
        let errors = [
            ReadError::Missing {
                path: absolute("/home/me/project/src/nope.rs"),
            },
            ReadError::NotUtf8 {
                path: absolute("/home/me/project/src/bad.rs"),
                offset: 3,
            },
            ReadError::UnsupportedLanguage {
                path: absolute("/home/me/project/logo.png"),
                extension: "png".to_owned(),
                supported: vec!["rs"],
            },
        ];

        let messages: Vec<String> = errors
            .iter()
            .map(|error| read_failed(error, &root, &occurrence("src/nope.rs")).message)
            .collect();

        assert_eq!(
            messages,
            [
                "src/nope.rs does not exist",
                "src/bad.rs is not UTF-8 (first invalid byte at offset 3)",
                "no language for `.png` files (logo.png)",
            ],
            "read findings should name the file from the project root"
        );
    }

    /// The hash `resolve` records for a CRLF Rust file linked whole, under `config_text`.
    fn hash_of_crlf_target(config_text: &str) -> crate::hash::Hash {
        let dir = tempfile::TempDir::new().expect("a temp dir should be creatable");
        std::fs::write(dir.path().join("supdoc.config.toml"), config_text)
            .expect("the fixture config should be writable");
        std::fs::write(dir.path().join("a.rs"), "fn a() {\r\n}\r\n")
            .expect("the fixture target should be writable");
        let config = Config::load(
            &AbsolutePath::new(dir.path()).expect("a temp dir path should be absolute"),
        )
        .expect("the fixture config should load");
        let occurrence = occurrence("a.rs");
        let key = occurrence.key.clone();
        let links = resolve(
            &config,
            Languages::builtin().expect("the built-in cartridges should load"),
            &mut Parsed::default(),
            config.hash_settings(),
            vec![occurrence],
        );
        match links.get(&key).map(|link| &link.target) {
            Some(TargetState::Present { hash, .. }) => *hash,
            other => panic!("expected a present target, got {other:?}"),
        }
    }

    #[test]
    fn crlf_target_hashes_follow_configured_line_endings() {
        let expected = |line_endings| {
            let dir = tempfile::TempDir::new().expect("a temp dir should be creatable");
            let path = dir.path().join("a.rs");
            std::fs::write(&path, "fn a() {\r\n}\r\n").expect("the fixture should be writable");
            let languages = Languages::builtin().expect("the built-in cartridges should load");
            let file = SourceFile::read(
                AbsolutePath::new(&path).expect("a temp dir path should be absolute"),
                languages,
                Limits::PARSE_TIMEOUT,
            )
            .expect("the fixture should read");
            hash_file(&file, HashSettings { line_endings })
        };

        assert_eq!(
            hash_of_crlf_target(""),
            expected(LineEndings::Normalize),
            "the default should normalize"
        );
        assert_eq!(
            hash_of_crlf_target("[features.hash]\nline-endings = \"exact\"\n"),
            expected(LineEndings::Exact),
            "exact should hash every byte"
        );
        assert_ne!(
            expected(LineEndings::Normalize),
            expected(LineEndings::Exact)
        );
    }
}
