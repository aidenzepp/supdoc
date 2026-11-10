//! Planning and applying `supdoc fix`, and carrying lock entries to rewritten links.
//!
//! [`plan_fixes`] is pure: it picks the scan's unambiguous [`Fix`]es and remembers what each one
//! expects to find on disk. [`apply_fixes`] is the effect: it verifies each file against that
//! expectation and only then rewrites it, atomically, last edit first. A file that changed since
//! the scan is skipped whole and reported; the other files are still fixed. The fixer never
//! guesses and never overwrites a concurrent edit.

use crate::remedies::{self, Access};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::check::moves::Move;
use crate::config::confine::{Boundary, Escape, write_atomically};
use crate::finding::{Finding, Fix, Location, Severity};
use crate::link::{LinkKey, SourcePath, SymbolPath, Target, TargetPath};
use crate::lock::model::Lockfile;
use crate::path::AbsolutePath;
use crate::scan::model::{Scan, TargetState};

/// Finding code: the symbol of a link has exactly one near match.
const CODE_SYMBOL_NOT_FOUND: &str = "resolve-not-found";

/// Finding code: the link's path differs from the real path only by case.
const CODE_CASE_MISMATCH: &str = "scan-case-mismatch";

/// Finding code: a file changed between the scan and the fix.
const CODE_FILE_CHANGED: &str = "fix-file-changed";

/// Finding code: a file could not be rewritten.
const CODE_WRITE_FAILED: &str = "fix-write-failed";

/// The edits `supdoc fix` would make. The preview is this value rendered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FixPlan {
    /// Sorted by file, then position. Edits to one file never overlap.
    fixes: Vec<PlannedFix>,
}

impl FixPlan {
    /// A plan of `fixes`, sorted by file then position, with every fix that overlaps an earlier
    /// one in the same file dropped.
    #[must_use]
    pub(crate) fn new(mut fixes: Vec<PlannedFix>) -> Self {
        fixes.sort_by(|a, b| order(&a.fix, &b.fix));
        Self {
            fixes: without_overlaps(fixes, |planned| &planned.fix),
        }
    }

    /// The edits, sorted by file then position, never overlapping.
    #[must_use]
    pub(crate) fn fixes(&self) -> &[PlannedFix] {
        &self.fixes
    }
}

/// One edit, the finding it repairs, and what the file must still contain for it to be safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedFix {
    /// The exact edit.
    pub(crate) fix: Fix,
    /// The finding the edit repairs, for the preview.
    pub(crate) finding: Finding,
    /// Where the finding's link is; it contains the edit.
    pub(crate) location: Location,
    /// The link as scanned: the whole link when the finding's snippet holds it, else the bytes the
    /// edit replaces.
    pub(crate) old_link: String,
    /// The link with the edit applied.
    pub(crate) new_link: String,
    /// The lock entry move when this edit rewrites a link wholly, so its entry can follow.
    pub(crate) moved: Option<Move>,
}

/// What [`apply_fixes`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FixResult {
    /// Edits written to disk, in plan order.
    pub(crate) applied: Vec<AppliedFix>,
    /// Why files were left alone: `fix-file-changed` and `fix-write-failed` findings.
    pub(crate) skipped: Vec<Skipped>,
}

/// Why a file's fixes were not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkippedKind {
    /// The file changed since the scan (`fix-file-changed`).
    FileChanged,
    /// The file could not be rewritten (`fix-write-failed`).
    WriteFailed,
}

/// A file left as it was, and the finding that says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skipped {
    /// What went wrong, for callers that count or branch on it.
    pub(crate) kind: SkippedKind,
    /// The finding reported for it.
    pub(crate) finding: Finding,
}

/// An edit that is now on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedFix {
    /// The edit.
    pub(crate) fix: Fix,
    /// The move of the lock entry of a link this edit rewrote, for [`move_lock_entries`].
    pub(crate) moved: Option<Move>,
}

/// Picks every fix the scan offers that can be verified before it is applied.
///
/// A fix is planned only if its finding has a location that contains it and `read` returns the
/// source, because the bytes it will replace are read from the source, never from a display snippet. Of two overlapping fixes in one
/// file the earlier one wins; the other link simply stays broken and shows up again on the next
/// scan.
#[must_use]
pub(crate) fn plan_fixes(scan: &Scan) -> FixPlan {
    let linked = scan
        .links
        .iter()
        .filter_map(|(key, link)| match &link.target {
            TargetState::Broken { finding } => Some((Some(key), finding)),
            TargetState::Present { .. } => None,
        });
    let loose = scan.findings.iter().map(|finding| (None, finding));

    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    for (key, finding) in linked.chain(loose) {
        let Some(fix) = finding.fix.as_deref() else {
            continue;
        };
        if !vouches_for(finding, fix) {
            continue;
        }
        if !candidates.iter().any(|other| other.fix == fix) {
            candidates.push(Candidate { key, finding, fix });
        }
    }
    candidates.sort_by(|a, b| order(a.fix, b.fix));

    let kept = without_overlaps(candidates, |candidate| candidate.fix);

    let fixes = kept
        .iter()
        .filter_map(|candidate| {
            let Candidate { key, finding, fix } = *candidate;
            let location = finding.location.clone()?;
            let moved = key.and_then(|key| moved_by(scan, key, finding, fix, &kept));
            let (old_link, new_link) = link_text(finding, &location, fix);
            Some(PlannedFix {
                fix: fix.clone(),
                finding: finding.clone(),
                location,
                old_link,
                new_link,
                moved,
            })
        })
        .collect();
    FixPlan::new(fixes)
}

/// A fix the scan offers, with the link it belongs to (when it has one) and the finding it repairs.
#[derive(Clone, Copy)]
struct Candidate<'a> {
    /// The link the finding is about, `None` for a finding of no link.
    key: Option<&'a LinkKey>,
    /// The finding the fix repairs.
    finding: &'a Finding,
    /// The finding's fix.
    fix: &'a Fix,
}

/// Keeps each item of `sorted` whose fix does not start inside an earlier kept fix of the same
/// file; of two overlapping fixes the earlier one wins.
fn without_overlaps<T>(sorted: Vec<T>, fix: impl Fn(&T) -> &Fix) -> Vec<T> {
    let mut kept: Vec<T> = Vec::new();
    for item in sorted {
        let later = fix(&item);
        let overlaps = kept.iter().any(|earlier| {
            let earlier = fix(earlier);
            earlier.file == later.file && later.range.start < earlier.range.end
        });
        if !overlaps {
            kept.push(item);
        }
    }
    kept
}

/// The link before and after `fix`. The snippet is a bounded window; the whole link is shown only
/// when the window holds it, else the replaced bytes and the replacement stand for it.
fn link_text(finding: &Finding, location: &Location, fix: &Fix) -> (String, String) {
    let link_range = &location.range;
    let scanned = |wanted: &Range<usize>| -> Option<String> {
        let snippet = finding.snippet.as_ref()?;
        let origin = link_range.start.checked_sub(snippet.highlight.start)?;
        let from = wanted.start.checked_sub(origin)?;
        let to = wanted.end.checked_sub(origin)?;
        snippet.text.get(from..to).map(str::to_owned)
    };
    let old_link = scanned(link_range).unwrap_or_else(|| fix.scanned.to_string());
    let new_link = fix
        .range
        .start
        .checked_sub(link_range.start)
        .zip(fix.range.end.checked_sub(link_range.start))
        .and_then(|(from, to)| {
            Some(format!(
                "{}{}{}",
                old_link.get(..from)?,
                fix.replacement,
                old_link.get(to..)?
            ))
        })
        .unwrap_or_else(|| fix.replacement.clone());
    (old_link, new_link)
}

/// Sorts fixes by file, then by position.
fn order(a: &Fix, b: &Fix) -> std::cmp::Ordering {
    let key = |fix: &Fix| (fix.file.clone(), fix.range.start, fix.range.end);
    key(a).cmp(&key(b))
}

/// Whether the finding can vouch for `fix`: it has a location in the same file that contains the
/// edit, and the edit remembers the bytes it replaces.
fn vouches_for(finding: &Finding, fix: &Fix) -> bool {
    finding.location.as_ref().is_some_and(|location| {
        location.file == fix.file
            && fix.range.start >= location.range.start
            && fix.range.start <= fix.range.end
            && fix.range.end <= location.range.end
            && fix.scanned.len() == fix.range.len()
    })
}

/// The old and new key when `fix` rewrites every place `key` is written, so the link has moved
/// wholesale. `None` when the new key can't be told exactly, would merge into a link that already
/// exists, or leaves some copy of the old link behind.
fn moved_by(
    scan: &Scan,
    key: &LinkKey,
    finding: &Finding,
    fix: &Fix,
    kept: &[Candidate<'_>],
) -> Option<Move> {
    let new = rewritten_key(key, finding.code, &fix.replacement)?;
    if new == *key || scan.links.contains_key(&new) {
        return None;
    }
    let link = scan.links.get(key)?;
    let rewritten = |range: &Range<usize>, file: &SourcePath| {
        kept.iter()
            .filter(|other| other.key == Some(key))
            .any(|other| {
                other.fix.file == *file
                    && other.fix.range.start >= range.start
                    && other.fix.range.end <= range.end
            })
    };
    link.locations()
        .all(|location| rewritten(&location.range, &location.file))
        .then(|| Move {
            from: key.clone(),
            to: new,
        })
}

/// `key` after a fix of kind `code` wrote `replacement` into it. Only the two kinds whose result
/// is exactly determined by the replacement are handled; anything else carries no history.
fn rewritten_key(key: &LinkKey, code: &str, replacement: &str) -> Option<LinkKey> {
    match code {
        CODE_SYMBOL_NOT_FOUND => {
            let symbol: SymbolPath = replacement.parse().ok()?;
            Some(LinkKey {
                symbol: Some(symbol),
                ..key.clone()
            })
        }
        CODE_CASE_MISMATCH => {
            // A case fix changes letters only, so the replacement's names are the trailing names
            // of the old key path; the segments before them (a directory the link climbs out of
            // or descends into, `..` past the root) are kept as the key has them.
            let written = replacement
                .split('/')
                .filter(|part| !matches!(*part, "" | "." | ".."));
            let names: Vec<&str> = written.collect();
            let old: Vec<&str> = key.target.path().as_str().split('/').collect();
            let kept = old.len().checked_sub(names.len())?;
            let path: TargetPath = old
                .get(..kept)?
                .iter()
                .chain(&names)
                .copied()
                .collect::<Vec<&str>>()
                .join("/")
                .parse()
                .ok()?;
            let target = match &key.target {
                Target::Path(_) => Target::Path(path),
                Target::Namespaced { namespace, .. } => Target::Namespaced {
                    namespace: namespace.clone(),
                    path,
                },
            };
            Some(LinkKey {
                target,
                ..key.clone()
            })
        }
        _ => None,
    }
}

/// Verifies each file against the scan, then rewrites it atomically with its edits applied last
/// first, so earlier offsets stay valid. `boundary` is the project root that source paths are
/// relative to, as the config resolved it when it loaded. Every file is admitted against it
/// immediately before it is written, so a file a symlink leads out of the root is skipped, never
/// written.
///
/// A file is rewritten whole or not at all: if any of its edits no longer finds the bytes it was
/// planned against, nothing in that file is touched. Other files are unaffected.
///
/// `rerun` is the command a skipped file's help says to run again. A write failure's help says the
/// other fixes were applied only when some were.
#[must_use]
pub(crate) fn apply_fixes(plan: &FixPlan, boundary: &Boundary, rerun: &str) -> FixResult {
    let root = boundary.dir();
    // A refused boundary has no directory, so each file stays relative, which `admit` refuses.
    let base = AbsolutePath::new(root);
    let mut by_file: BTreeMap<&SourcePath, Vec<&PlannedFix>> = BTreeMap::new();
    for planned in plan.fixes() {
        by_file.entry(&planned.fix.file).or_default().push(planned);
    }

    let mut result = FixResult::default();
    let mut unwritten = Vec::new();
    for (file, planned) in by_file {
        let path = base.as_ref().map_or_else(
            || PathBuf::from(file.as_str()),
            |base| base.beneath(file).as_path().to_path_buf(),
        );
        match rewrite_file(boundary, &path, &planned) {
            Ok(()) => result
                .applied
                .extend(planned.iter().map(|planned| AppliedFix {
                    fix: planned.fix.clone(),
                    moved: planned.moved.clone(),
                })),
            Err(Skip::Changed) => result.skipped.push(Skipped {
                kind: SkippedKind::FileChanged,
                finding: file_changed(file, planned.len(), rerun),
            }),
            Err(Skip::Unwritable(cause)) => unwritten.push((file, cause)),
        }
    }
    let others_applied = !result.applied.is_empty();
    for (file, cause) in unwritten {
        result.skipped.push(Skipped {
            kind: SkippedKind::WriteFailed,
            finding: write_failed(file, &cause, root, rerun, others_applied),
        });
    }
    result
}

/// Why a file was left as it was.
#[derive(Debug)]
enum Skip {
    /// The file no longer matches the scan.
    Changed,
    /// The file could not be read or written.
    Unwritable(Unwritable),
}

/// Why a file that still exists could not be rewritten.
#[derive(Debug)]
enum Unwritable {
    /// The source is a symlink.
    Symlink {
        /// The file the link resolves to, if that can be found.
        real: Option<PathBuf>,
    },
    /// The source is neither a regular file nor a symlink.
    NotRegular,
    /// The source leads out of the project root: a path on the way changed after the scan.
    OutsideRoot,
    /// The operating system refused the read or the write.
    Io(io::Error),
}

impl From<io::Error> for Skip {
    /// A file that vanished since the scan changed; any other error is an I/O failure.
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::Changed
        } else {
            Self::Unwritable(Unwritable::Io(error))
        }
    }
}

/// Reads `path`, checks every edit's expectation, applies them last first and writes the result,
/// once `boundary` admits `path`.
///
/// # Errors
///
/// [`Skip::Changed`] if any edit's expected bytes are gone; [`Skip::Unwritable`] if the file is
/// not a regular file, lies outside `boundary`, or cannot be read or written. A file that
/// vanished is `Changed`. The file is unchanged in both cases.
fn rewrite_file(boundary: &Boundary, path: &Path, planned: &[&PlannedFix]) -> Result<(), Skip> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(Skip::Unwritable(if metadata.is_symlink() {
            Unwritable::Symlink {
                real: fs::canonicalize(path).ok(),
            }
        } else {
            Unwritable::NotRegular
        }));
    }
    let mut bytes = fs::read(path)?;
    let unchanged = planned.iter().all(|planned| {
        bytes
            .get(planned.fix.range.clone())
            .is_some_and(|found| found == planned.fix.scanned.as_bytes())
    });
    if !unchanged {
        return Err(Skip::Changed);
    }
    for planned in planned.iter().rev() {
        let range = planned.fix.range.clone();
        if bytes.get(range.clone()).is_none() {
            return Err(Skip::Changed);
        }
        let _removed: Vec<u8> = bytes
            .splice(range, planned.fix.replacement.bytes())
            .collect();
    }
    let admitted = AbsolutePath::new(path)
        .ok_or(Escape::Lexical)
        .and_then(|target| boundary.admit(&target))
        .map_err(|_escape| Skip::Unwritable(Unwritable::OutsideRoot))?;
    write_atomically(&admitted, &bytes, Some(&metadata.permissions()))?;
    Ok(())
}

/// The `fix-file-changed` finding for `file`, which had `count` fixes.
fn file_changed(file: &SourcePath, count: usize, rerun: &str) -> Finding {
    let fixes = if count == 1 {
        "its 1 fix was not applied".to_owned()
    } else {
        format!("its {count} fixes were not applied")
    };
    Finding::new(
        CODE_FILE_CHANGED,
        Severity::Warning,
        format!("{file} changed since it was scanned; {fixes}"),
    )
    .with_help(format!(
        "Run `{rerun}` again. Nothing in that file was modified"
    ))
}

/// The `fix-write-failed` finding for `file`.
fn write_failed(
    file: &SourcePath,
    cause: &Unwritable,
    root: &Path,
    rerun: &str,
    others_applied: bool,
) -> Finding {
    let others = if others_applied {
        ". The other fixes were applied"
    } else {
        ""
    };
    let (reason, help) = match cause {
        Unwritable::Symlink { real } => {
            let real = real.as_deref().map_or_else(
                || "the file it points to".to_owned(),
                |real| format!("`{}`", real.strip_prefix(root).unwrap_or(real).display()),
            );
            (
                "it is a symlink".to_owned(),
                format!(
                    "supdoc never rewrites a symlinked file. Apply the fix to {real} by hand, then run `{rerun}` again{others}"
                ),
            )
        }
        Unwritable::OutsideRoot => (
            "it now leads outside the project root: a path on the way changed after supdoc checked it"
                .to_owned(),
            format!(
                "supdoc never writes outside the project root. Check what changed on the way to {file}, then run `{rerun}` again{others}"
            ),
        ),
        Unwritable::NotRegular => (
            "it is not a regular file".to_owned(),
            format!("supdoc only rewrites regular files. Apply the fix by hand{others}"),
        ),
        Unwritable::Io(error) => (
            error.to_string(),
            if error.kind() == io::ErrorKind::PermissionDenied {
                format!(
                    "The file was left as it was. Run `{grant}`, then run `{rerun}` again{others}",
                    grant = remedies::grant_on(Access::Write, &file.to_string(), remedies::WINDOWS)
                )
            } else {
                format!(
                    "The file was left as it was. Make it writable and run `{rerun}` again{others}"
                )
            },
        ),
    };
    Finding::new(
        CODE_WRITE_FAILED,
        Severity::Error,
        format!("cannot write {file}: {reason}"),
    )
    .with_help(help)
}

/// Moves the lock entry of every rewritten link to its new key, so a fixed link keeps its
/// history: unchanged code reports `passed`, changed code reports `failed`.
///
/// An entry is never overwritten: if the new key already has one, both stay as they are. Returns
/// the moves that happened, so a caller reports only those.
pub(crate) fn move_lock_entries<'a>(
    lock: &mut Lockfile,
    wanted: impl IntoIterator<Item = &'a Move>,
) -> Vec<Move> {
    let mut moved = Vec::new();
    for planned in wanted {
        if lock.get(&planned.to).is_none()
            && let Some(hash) = lock.remove(&planned.from)
        {
            let _previous = lock.insert(planned.to.clone(), hash);
            moved.push(planned.clone());
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Location, Snippet};
    use crate::hash::Hash;
    use crate::scan::model::ScannedLink;

    const DOC: &str = "# Guide\nCall [add](src/lib.rs#ad) and [sub](src/lib.rs#su).\n";

    fn key(source: &str, symbol: &str) -> LinkKey {
        LinkKey {
            source: source.parse().expect("the fixture source should be valid"),
            target: "src/lib.rs"
                .parse()
                .expect("the fixture target should be valid"),
            symbol: Some(symbol.parse().expect("the fixture symbol should be valid")),
        }
    }

    /// A broken link in `text` whose `old` symbol text is fixed to `new`.
    fn broken(source: &str, text: &str, old: &str, new: &str) -> (LinkKey, ScannedLink) {
        let fragment = format!("#{old})");
        let at = text
            .find(&fragment)
            .expect("the fixture text should hold the link")
            + 1;
        let range = at..at + old.len();
        let link_start = text
            .get(..at)
            .and_then(|before| before.rfind('['))
            .expect("the fixture link should open");
        let link_range = link_start..at + old.len() + 1;
        let file: SourcePath = source.parse().expect("the fixture source should be valid");
        let location = |range| {
            Location::in_text(file.clone(), text, range).expect("the fixture range should locate")
        };
        let finding = Finding::new(CODE_SYMBOL_NOT_FOUND, Severity::Error, "no such symbol")
            .with_location(location(range.clone()))
            .with_snippet(Snippet::capture(text, range.clone()).expect("the range should capture"))
            .with_fix(Fix {
                file: file.clone(),
                scanned: old.into(),
                range,
                replacement: new.to_owned(),
            });
        (
            key(source, old),
            ScannedLink {
                snippet: None,
                first: location(link_range),
                more: Vec::new(),
                target: TargetState::Broken { finding },
            },
        )
    }

    fn scan_of(links: impl IntoIterator<Item = (LinkKey, ScannedLink)>) -> Scan {
        Scan {
            links: links.into_iter().collect(),
            ..Scan::default()
        }
    }

    fn first(plan: &FixPlan) -> &PlannedFix {
        plan.fixes()
            .first()
            .expect("the plan should hold at least one fix")
    }

    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        fn with(files: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().expect("a temp directory should be creatable");
            for (name, text) in files {
                let path = dir.path().join(name);
                fs::create_dir_all(path.parent().expect("the fixture path has a parent"))
                    .expect("the fixture directory should be creatable");
                fs::write(path, text).expect("the fixture file should be writable");
            }
            Self { dir }
        }

        fn root(&self) -> AbsolutePath {
            AbsolutePath::new(self.dir.path()).expect("a temp directory path should be absolute")
        }

        fn boundary(&self) -> Boundary {
            Boundary::resolve(&self.root())
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.dir.path().join(name)).expect("the file should be readable")
        }
    }

    #[test]
    fn plan_picks_the_fix_of_a_broken_link() {
        let scan = scan_of([broken("docs/a.md", DOC, "ad", "add")]);

        let plan = plan_fixes(&scan);

        assert_eq!(plan.fixes().len(), 1);
        assert_eq!(first(&plan).fix.replacement, "add");
    }

    #[test]
    fn plan_ignores_findings_without_a_fix() {
        let (key, mut link) = broken("docs/a.md", DOC, "ad", "add");
        if let TargetState::Broken { finding } = &mut link.target {
            finding.fix = None;
        }

        assert_eq!(plan_fixes(&scan_of([(key, link)])).fixes(), []);
    }

    #[test]
    fn plan_sorts_fixes_by_position_and_drops_overlaps() {
        let second = broken("docs/a.md", DOC, "su", "sub");
        let first = broken("docs/a.md", DOC, "ad", "add");
        let mut clash = broken("docs/a.md", DOC, "ad", "adds");
        clash.0 = key("docs/a.md", "adx");

        let plan = plan_fixes(&scan_of([second, first, clash]));

        let replacements: Vec<&str> = plan
            .fixes()
            .iter()
            .map(|planned| planned.fix.replacement.as_str())
            .collect();
        assert_eq!(replacements.len(), 2, "the overlapping fix must be dropped");
        assert_eq!(replacements.last(), Some(&"sub"));
    }

    #[test]
    fn symbol_fix_moves_the_old_key_to_the_new_one() {
        let scan = scan_of([broken("docs/a.md", DOC, "ad", "add")]);

        let plan = plan_fixes(&scan);

        assert_eq!(
            first(&plan).moved,
            Some(Move {
                from: key("docs/a.md", "ad"),
                to: key("docs/a.md", "add")
            })
        );
    }

    #[test]
    fn fix_into_an_existing_link_moves_nothing() {
        let existing = (
            key("docs/a.md", "add"),
            ScannedLink {
                snippet: None,
                first: Location::in_text(
                    "docs/a.md"
                        .parse()
                        .expect("the fixture source should be valid"),
                    "[a](b)",
                    0..6,
                )
                .expect("the fixture range should be locatable"),
                more: Vec::new(),
                target: TargetState::Present {
                    hash: Hash::digest(b"body"),
                    syntax_errors: false,
                },
            },
        );
        let scan = scan_of([broken("docs/a.md", DOC, "ad", "add"), existing]);

        let plan = plan_fixes(&scan);

        assert_eq!(
            first(&plan).moved,
            None,
            "the old hash must not be merged into a link that already has its own"
        );
    }

    #[test]
    fn a_link_written_twice_with_one_fix_moves_nothing() {
        let (key, mut link) = broken("docs/a.md", DOC, "ad", "add");
        let mut second = link.first.clone();
        second.range = 0..1;
        link.more.push(second);

        let plan = plan_fixes(&scan_of([(key, link)]));

        assert_eq!(
            first(&plan).moved,
            None,
            "a copy of the old link would remain"
        );
    }

    /// A case fix of `written` to `fixed` in `source`, moving the key `old` to `new`; asserts the
    /// lock entry follows it.
    fn assert_case_fix_moves(source: &str, written: &str, fixed: &str, old: &str, new: &str) {
        let text = format!("[x]({written}#add)\n");
        let link_key = |path: &str| LinkKey {
            source: source.parse().expect("the fixture source should be valid"),
            target: path.parse().expect("the fixture target should be valid"),
            symbol: Some("add".parse().expect("the fixture symbol should be valid")),
        };
        let range = 4..4 + written.len();
        let file: SourcePath = source.parse().expect("the fixture source should be valid");
        let location = Location::in_text(file.clone(), &text, range.clone())
            .expect("the fixture range should locate");
        let finding = Finding::new(CODE_CASE_MISMATCH, Severity::Error, "case differs")
            .with_location(location.clone())
            .with_snippet(Snippet::capture(&text, range.clone()).expect("the range should capture"))
            .with_fix(Fix {
                file,
                scanned: written.into(),
                range,
                replacement: fixed.to_owned(),
            });
        let link = ScannedLink {
            snippet: None,
            first: location,
            more: Vec::new(),
            target: TargetState::Broken { finding },
        };
        let plan = plan_fixes(&scan_of([(link_key(old), link)]));
        assert_eq!(
            first(&plan).moved,
            Some(Move {
                from: link_key(old),
                to: link_key(new)
            }),
            "the key for `{written}` in {source} should move to the real path"
        );

        let hash = Hash::digest(b"body");
        let mut lock: Lockfile = [(link_key(old), hash)].into_iter().collect();
        let applied: Vec<AppliedFix> = plan
            .fixes()
            .iter()
            .map(|planned| AppliedFix {
                fix: planned.fix.clone(),
                moved: planned.moved.clone(),
            })
            .collect();
        let _moved = move_lock_entries(&mut lock, applied.iter().filter_map(|a| a.moved.as_ref()));
        assert_eq!(
            lock.get(&link_key(new)),
            Some(hash),
            "the lock entry for `{written}` in {source} should keep its history"
        );
        assert_eq!(lock.get(&link_key(old)), None);
    }

    #[test]
    fn case_fix_moves_the_key_of_a_link_climbing_to_the_root() {
        assert_case_fix_moves(
            "docs/a.md",
            "../SRC/lib.rs",
            "../src/lib.rs",
            "SRC/lib.rs",
            "src/lib.rs",
        );
    }

    #[test]
    fn case_fix_moves_the_key_of_a_same_directory_link() {
        assert_case_fix_moves(
            "docs/a.md",
            "LIB.rs",
            "lib.rs",
            "docs/LIB.rs",
            "docs/lib.rs",
        );
    }

    #[test]
    fn case_fix_moves_the_key_of_a_subdirectory_link() {
        assert_case_fix_moves(
            "docs/a.md",
            "Api/LIB.rs",
            "api/lib.rs",
            "docs/Api/LIB.rs",
            "docs/api/lib.rs",
        );
    }

    #[test]
    fn a_link_with_two_kilobytes_of_text_still_gets_its_fix_applied() {
        let label = "x".repeat(2048);
        let text = format!("# Guide\nCall [{label}](src/lib.rs#ad) now.\n");
        let project = Project::with(&[("docs/a.md", text.as_str())]);
        let (key, mut link) = broken("docs/a.md", &text, "ad", "add");
        let link_range =
            text.find('[').expect("the fixture should hold a link")..text.len() - " now.\n".len();
        let snippet =
            Snippet::capture(&text, link_range.clone()).expect("the link range should capture");
        assert!(
            snippet.text.len() < text.len(),
            "the snippet should be a bounded window, not the whole line"
        );
        let finding = match &mut link.target {
            TargetState::Broken { finding } => Some(finding),
            TargetState::Present { .. } => None,
        }
        .expect("the fixture link should be broken");
        finding.location = Some(
            Location::in_text(
                "docs/a.md"
                    .parse()
                    .expect("the fixture source should be valid"),
                &text,
                link_range,
            )
            .expect("the link range should locate"),
        );
        finding.snippet = Some(snippet);

        let plan = plan_fixes(&scan_of([(key, link)]));
        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(
            plan.fixes().len(),
            1,
            "the long link's fix should be planned"
        );
        assert_eq!(result.skipped, vec![]);
        assert_eq!(
            project.read("docs/a.md"),
            text.replacen("#ad)", "#add)", 1),
            "the fix should rewrite the symbol of the long link"
        );
    }

    #[test]
    fn a_doc_edited_after_the_scan_is_skipped_as_changed() {
        let project = Project::with(&[("docs/a.md", DOC)]);
        let plan = plan_fixes(&scan_of([broken("docs/a.md", DOC, "ad", "add")]));
        let edited = DOC
            .replacen("Call", "Now call", 1)
            .replacen("#ad)", "#xy)", 1);
        fs::write(project.dir.path().join("docs/a.md"), &edited)
            .expect("the edited doc should be written");

        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(result.applied, vec![]);
        assert_eq!(result.skipped.len(), 1, "{:?}", result.skipped);
        assert_eq!(
            project.read("docs/a.md"),
            edited,
            "a file that no longer matches the scan must not be rewritten"
        );
    }

    #[test]
    fn apply_rewrites_the_link_in_the_file() {
        let project = Project::with(&[("docs/a.md", DOC)]);
        let plan = plan_fixes(&scan_of([broken("docs/a.md", DOC, "ad", "add")]));

        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.skipped, vec![]);
        assert_eq!(
            project.read("docs/a.md"),
            "# Guide\nCall [add](src/lib.rs#add) and [sub](src/lib.rs#su).\n"
        );
    }

    #[test]
    fn multiple_fixes_in_one_file_apply_bottom_up() {
        let project = Project::with(&[("docs/a.md", DOC)]);
        let plan = plan_fixes(&scan_of([
            broken("docs/a.md", DOC, "ad", "add_numbers"),
            broken("docs/a.md", DOC, "su", "s"),
        ]));

        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(result.applied.len(), 2);
        assert_eq!(
            project.read("docs/a.md"),
            "# Guide\nCall [add](src/lib.rs#add_numbers) and [sub](src/lib.rs#s).\n",
            "the shorter and longer edits must not disturb each other's offsets"
        );
    }

    #[test]
    fn a_file_edited_after_the_scan_is_skipped_and_others_still_fixed() {
        let project = Project::with(&[("docs/a.md", DOC), ("docs/b.md", DOC)]);
        let plan = plan_fixes(&scan_of([
            broken("docs/a.md", DOC, "ad", "add"),
            broken("docs/a.md", DOC, "su", "sub"),
            broken("docs/b.md", DOC, "ad", "add"),
        ]));
        let edited = DOC.replace("Call", "Invoke");
        fs::write(project.dir.path().join("docs/a.md"), &edited)
            .expect("the fixture file should be writable");

        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(
            project.read("docs/a.md"),
            edited,
            "the edited file is untouched"
        );
        assert!(project.read("docs/b.md").contains("#add)"));
        assert_eq!(result.applied.len(), 1);
        let [skipped] = result.skipped.as_slice() else {
            panic!(
                "expected exactly one skipped finding, got {:?}",
                result.skipped
            );
        };
        assert_eq!(skipped.finding.code, CODE_FILE_CHANGED);
        assert_eq!(
            skipped.finding.message,
            "docs/a.md changed since it was scanned; its 2 fixes were not applied"
        );
    }

    #[test]
    fn a_vanished_file_is_reported_as_changed() {
        let project = Project::with(&[("docs/b.md", DOC)]);
        let plan = plan_fixes(&scan_of([broken("docs/a.md", DOC, "ad", "add")]));

        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(result.applied, vec![]);
        let [skipped] = result.skipped.as_slice() else {
            panic!(
                "expected exactly one skipped finding, got {:?}",
                result.skipped
            );
        };
        assert_eq!(skipped.finding.code, CODE_FILE_CHANGED);
        assert_eq!(skipped.finding.severity, Severity::Warning);
    }

    #[test]
    fn applying_leaves_no_temporary_file_behind() {
        let project = Project::with(&[("docs/a.md", DOC)]);
        let plan = plan_fixes(&scan_of([broken("docs/a.md", DOC, "ad", "add")]));

        let _result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        let names: Vec<_> = fs::read_dir(project.dir.path().join("docs"))
            .expect("the docs directory should be readable")
            .map(|entry| entry.expect("a directory entry should read").file_name())
            .collect();
        assert_eq!(names, ["a.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn applying_keeps_the_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let project = Project::with(&[("docs/a.md", DOC)]);
        let path = project.dir.path().join("docs/a.md");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
            .expect("the fixture permissions should be settable");
        let plan = plan_fixes(&scan_of([broken("docs/a.md", DOC, "ad", "add")]));

        let _result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        let mode = fs::metadata(&path)
            .expect("the file should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_source_is_never_replaced() {
        let project = Project::with(&[("real.md", DOC)]);
        std::os::unix::fs::symlink("real.md", project.dir.path().join("docs-link.md"))
            .expect("the fixture symlink should be creatable");
        let plan = plan_fixes(&scan_of([broken("docs-link.md", DOC, "ad", "add")]));

        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(result.applied, vec![]);
        let [skipped] = result.skipped.as_slice() else {
            panic!(
                "expected exactly one skipped finding, got {:?}",
                result.skipped
            );
        };
        assert_eq!(skipped.finding.code, CODE_WRITE_FAILED);
        let help = skipped.finding.help.as_deref().unwrap_or_default();
        assert!(
            help.contains("never rewrites a symlinked file") && help.contains("real.md"),
            "the help should say supdoc never rewrites a symlink and name the real file, got {help:?}"
        );
        assert_eq!(project.read("real.md"), DOC);
    }

    #[cfg(unix)]
    #[test]
    fn a_source_under_a_directory_symlinked_out_of_the_root_is_skipped_and_left_alone() {
        let project = Project::with(&[]);
        let outside = tempfile::tempdir().expect("an outside directory should be creatable");
        let victim = outside.path().join("guide.md");
        fs::write(&victim, DOC).expect("the outside doc should be writable");
        std::os::unix::fs::symlink(outside.path(), project.dir.path().join("docs"))
            .expect("the fixture symlink should be creatable");
        let plan = plan_fixes(&scan_of([broken("docs/guide.md", DOC, "ad", "add")]));

        let result = apply_fixes(&plan, &project.boundary(), "supdoc fix --yes");

        assert_eq!(result.applied, vec![]);
        let [skipped] = result.skipped.as_slice() else {
            panic!(
                "expected exactly one skipped finding, got {:?}",
                result.skipped
            );
        };
        assert_eq!(
            (skipped.kind, skipped.finding.code),
            (SkippedKind::WriteFailed, CODE_WRITE_FAILED)
        );
        assert_eq!(
            fs::read(&victim).expect("the outside doc should be readable"),
            DOC.as_bytes(),
            "the outside file should be byte-identical"
        );
        assert_eq!(
            fs::read_dir(outside.path())
                .expect("the outside dir should be listable")
                .count(),
            1,
            "no temporary file should be left outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_root_swapped_for_a_symlink_out_after_the_config_loaded_writes_nothing_outside() {
        let dir = tempfile::tempdir().expect("a temp directory should be creatable");
        let (root, outside) = (dir.path().join("root"), dir.path().join("outside"));
        for tree in [&root, &outside] {
            fs::create_dir_all(tree.join("docs")).expect("the fixture dirs should be creatable");
            fs::write(tree.join("docs/guide.md"), DOC).expect("the fixture doc should be writable");
        }
        let boundary = Boundary::resolve(
            &AbsolutePath::new(&root).expect("a temp directory path should be absolute"),
        );
        let plan = plan_fixes(&scan_of([broken("docs/guide.md", DOC, "ad", "add")]));
        fs::rename(&root, dir.path().join("moved")).expect("the root should be movable");
        std::os::unix::fs::symlink(&outside, &root).expect("the symlink should be creatable");

        let result = apply_fixes(&plan, &boundary, "supdoc fix --yes");

        assert_eq!(result.applied, vec![]);
        let [skipped] = result.skipped.as_slice() else {
            panic!(
                "expected exactly one skipped finding, got {:?}",
                result.skipped
            );
        };
        assert_eq!(
            (skipped.kind, skipped.finding.code),
            (SkippedKind::WriteFailed, CODE_WRITE_FAILED)
        );
        assert!(
            skipped
                .finding
                .message
                .contains("now leads outside the project root"),
            "the finding should say the file now leads outside: {}",
            skipped.finding.message
        );
        assert_eq!(
            fs::read(outside.join("docs/guide.md")).expect("the outside doc should be readable"),
            DOC.as_bytes(),
            "the outside file should be byte-identical"
        );
        assert_eq!(
            fs::read_dir(outside.join("docs"))
                .expect("the outside dir should be listable")
                .count(),
            1,
            "no temporary file should be left outside"
        );
    }

    #[test]
    fn lock_entry_follows_the_rewritten_link() {
        let hash = Hash::digest(b"body");
        let mut lock: Lockfile = [(key("docs/a.md", "ad"), hash)].into_iter().collect();
        let applied = [AppliedFix {
            fix: Fix {
                file: "docs/a.md"
                    .parse()
                    .expect("the fixture source should be valid"),
                range: 0..1,
                replacement: String::new(),
                scanned: Box::default(),
            },
            moved: Some(Move {
                from: key("docs/a.md", "ad"),
                to: key("docs/a.md", "add"),
            }),
        }];

        let moved = move_lock_entries(&mut lock, applied.iter().filter_map(|a| a.moved.as_ref()));
        assert_eq!(
            moved,
            vec![Move {
                from: key("docs/a.md", "ad"),
                to: key("docs/a.md", "add")
            }]
        );

        assert_eq!(lock.get(&key("docs/a.md", "add")), Some(hash));
        assert_eq!(lock.get(&key("docs/a.md", "ad")), None);
    }

    #[test]
    fn lock_move_never_overwrites_an_existing_entry() {
        let (old_hash, kept_hash) = (Hash::digest(b"old"), Hash::digest(b"kept"));
        let mut lock: Lockfile = [
            (key("docs/a.md", "ad"), old_hash),
            (key("docs/a.md", "add"), kept_hash),
        ]
        .into_iter()
        .collect();
        let applied = [AppliedFix {
            fix: Fix {
                file: "docs/a.md"
                    .parse()
                    .expect("the fixture source should be valid"),
                range: 0..1,
                replacement: String::new(),
                scanned: Box::default(),
            },
            moved: Some(Move {
                from: key("docs/a.md", "ad"),
                to: key("docs/a.md", "add"),
            }),
        }];

        let moved = move_lock_entries(&mut lock, applied.iter().filter_map(|a| a.moved.as_ref()));
        assert_eq!(
            moved,
            vec![],
            "nothing should be reported moved when the new key was taken"
        );

        assert_eq!(lock.get(&key("docs/a.md", "add")), Some(kept_hash));
        assert_eq!(lock.get(&key("docs/a.md", "ad")), Some(old_hash));
    }
}
