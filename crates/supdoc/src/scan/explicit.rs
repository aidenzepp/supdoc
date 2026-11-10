//! Judging the paths a scan names explicitly.

use std::fs;
use std::path::Path;

use ignore::Match;

use crate::config::{CONFIG_FILE_NAME, Config, PatternDefinition};
use crate::finding::{Finding, Severity};
use crate::link::SourcePath;
use crate::lock::error::shell_quote;
use crate::output::paths::relative_to;
use crate::path::AbsolutePath;
use crate::read::cartridge::Languages;
use crate::scan::ignore::{IgnoreRules, IgnoredBy};
use crate::scan::model::Scope;
use crate::scan::walk::{UnreadDir, WalkedSource, is_nested_project, is_vcs_dir};

/// Warnings for each explicitly named path that the walk skipped because of the config or an
/// ignore file. A path beneath one of `unread_dirs` gets none: that directory's own
/// warning already explains it.
pub(super) fn explicit_path_warnings(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
    rules: &IgnoreRules,
    sources: &[WalkedSource],
    unread_dirs: &[UnreadDir],
) -> Vec<Finding> {
    let Scope::Paths(listed) = scope else {
        return Vec::new();
    };
    listed
        .iter()
        .filter(|path| {
            let path = config.root().beneath(*path);
            !unread_dirs
                .iter()
                .any(|dir| path.as_path().starts_with(dir.path.as_path()))
        })
        .filter_map(|path| explicit_path_warning(config, languages, rules, sources, path))
        .collect()
}

/// The warning for one explicitly named path, or `None` when it was walked or is missing. A path
/// in a version-control or nested-project directory, or of a type no cartridge reads, gets a
/// `scan-explicit-path-unscanned` warning. A path that is a symlink, or lies under a symlinked directory, is never walked, so it always
/// gets a warning.
pub(super) fn explicit_path_warning(
    config: &Config,
    languages: &Languages,
    rules: &IgnoreRules,
    sources: &[WalkedSource],
    listed: &SourcePath,
) -> Option<Finding> {
    let root = config.root();
    let path = root.beneath(listed);
    let kind = fs::symlink_metadata(path.as_path()).ok()?.file_type();
    if let Some(skipped) = skipped_dir(root, listed) {
        return Some(unscanned(listed, &Unscanned::Dir(skipped)));
    }
    if kind.is_symlink() {
        return Some(symlinked(root, listed, &path, Symlink::Itself));
    }
    if !kind.is_file() && !kind.is_dir() {
        return None;
    }

    let ignored_by = ignore_rule(rules, root, &path, kind.is_dir());
    let is_source_file = kind.is_file() && languages.for_path(path.as_path()).is_some();
    let walked = sources
        .binary_search_by(|walked| walked.path.cmp(&path))
        .is_ok();

    if let Some(rule) = ignored_by {
        return Some(ignored(listed, &rule));
    }
    let code_blocks = CodeBlocksScope::of(config, &path, kind.is_dir());
    let excluded_by = config
        .excluded_by(&path)
        .filter(|_| code_blocks != CodeBlocksScope::Reads);
    if let Some(pattern) = excluded_by.filter(|_| is_source_file || kind.is_dir()) {
        return Some(excluded(
            listed,
            Some(pattern),
            &defined_in(root, config.exclude_definition()),
            code_blocks,
        ));
    }
    if is_source_file && !config.scans(&path) && code_blocks != CodeBlocksScope::Reads {
        return Some(excluded(
            listed,
            None,
            &defined_in(root, config.include_definition()),
            code_blocks,
        ));
    }
    // The walk never enters a symlinked directory, so nothing under one is walked and no rule
    // names it.
    if has_symlinked_parent(root, listed) {
        return Some(symlinked(root, listed, &path, Symlink::Parent));
    }
    // The walk skipped it, but asking each rule again found none that does: say so, rather than
    // blame a rule that may not exist.
    if is_source_file && !walked {
        return Some(unscanned(
            listed,
            &Unscanned::Unreached {
                in_work_tree: rules.in_work_tree,
            },
        ));
    }
    (kind.is_file() && !is_source_file).then(|| unscanned(listed, &Unscanned::UnreadType))
}

/// Why the walk never reads an explicitly named path.
pub(super) enum Unscanned {
    /// A directory on the way is skipped whatever the user names.
    Dir(SkippedDir),
    /// No cartridge reads files of this type.
    UnreadType,
    /// The walk didn't reach it, and no rule found explains why, as when it is named in a case
    /// other than its own on a case-insensitive file system. In a git work tree,
    /// `git check-ignore` may name a rule supdoc missed.
    Unreached {
        /// Whether the root lies in a git work tree.
        in_work_tree: bool,
    },
}

/// A directory the walk never enters, whatever the user names.
pub(super) enum SkippedDir {
    /// A version-control directory, at this project-relative path.
    Vcs(String),
    /// A separate project with its own config, at this project-relative path.
    Project(String),
}

/// The finding for an explicit path the walk never reads for a reason that isn't a user setting.
pub(super) fn unscanned(listed: &SourcePath, why: &Unscanned) -> Finding {
    let listed = listed.as_str();
    let (reason, help) = match why {
        Unscanned::Dir(SkippedDir::Vcs(dir)) => (
            format!("is inside `{dir}`, a version-control directory supdoc never scans"),
            "Pass a path outside it, or stop passing this one".to_owned(),
        ),
        Unscanned::Dir(SkippedDir::Project(dir)) => (
            format!("is inside `{dir}`, a separate project with its own {CONFIG_FILE_NAME}"),
            format!("Run `supdoc check` inside `{dir}`, or stop passing this path"),
        ),
        Unscanned::UnreadType => (
            "has a type no language supdoc reads".to_owned(),
            "Pass a `.md` or `.rs` file instead, or stop passing this one".to_owned(),
        ),
        Unscanned::Unreached { in_work_tree } => (
            "the walk didn't reach it, and supdoc found no rule that skips it".to_owned(),
            if *in_work_tree {
                format!(
                    "Check that `{listed}` matches its name on disk, case included. `git \
                     check-ignore -v -- {}` may show a rule that skips it",
                    shell_quote(listed),
                )
            } else {
                format!(
                    "Check that `{listed}` matches its name on disk, case included, and can be \
                     read, or stop passing it"
                )
            },
        ),
    };
    Finding::new(
        "scan-explicit-path-unscanned",
        Severity::Warning,
        format!("{listed} was named on the command line but {reason}"),
    )
    .with_help(help)
}

/// Whether a directory above `listed`, inside `root`, is a symlink.
pub(super) fn has_symlinked_parent(root: &AbsolutePath, listed: &SourcePath) -> bool {
    let Some(parents) = root
        .beneath(listed)
        .parent()
        .and_then(|parent| root.steps_to(&parent))
    else {
        return false;
    };
    let mut parents = parents;
    parents.any(|parent| {
        fs::symlink_metadata(parent.as_path()).is_ok_and(|metadata| metadata.is_symlink())
    })
}

/// Where the symlink sits that keeps the walk from an explicitly named path.
#[derive(Clone, Copy)]
pub(super) enum Symlink {
    /// The named path is itself a symlink, which the walk doesn't follow.
    Itself,
    /// A directory above the named path is a symlink, which the walk doesn't enter.
    Parent,
}

/// The finding for an explicit path that is a symlink or is reached through a symlinked
/// directory, neither of which the walk follows. It suggests the real path only when that lies
/// inside the project.
pub(super) fn symlinked(
    root: &AbsolutePath,
    listed: &SourcePath,
    path: &AbsolutePath,
    symlink: Symlink,
) -> Finding {
    let listed = listed.as_str();
    let message = match symlink {
        Symlink::Itself => format!(
            "{listed} was named on the command line but is a symlink, which supdoc doesn't follow",
        ),
        Symlink::Parent => format!(
            "{listed} was named on the command line but is reached through a symlinked directory, which supdoc doesn't enter",
        ),
    };
    let finding = Finding::new("scan-explicit-path-symlinked", Severity::Warning, message);
    let real = fs::canonicalize(path.as_path()).ok().and_then(|real| {
        let real_root = fs::canonicalize(root.as_path()).ok()?;
        let relative = real.strip_prefix(real_root).ok()?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        (!relative.is_empty()).then_some(relative)
    });
    match real {
        Some(real) => finding.with_help(format!("Pass the real path `{real}` instead")),
        None => finding.with_help("Pass the real path inside the project instead"),
    }
}

/// The config file that set a pattern key, relative to the project `root`, or the default config
/// name when no file sets it.
pub(super) fn defined_in(root: &AbsolutePath, definition: Option<PatternDefinition<'_>>) -> String {
    definition.map_or_else(
        || CONFIG_FILE_NAME.to_owned(),
        |definition| relative_to(root, definition.file()),
    )
}

/// Whether code-blocks reads an explicitly named path, judged by its own `include` and `exclude`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CodeBlocksScope {
    /// `[features.code-blocks]` is off.
    Off,
    /// Code-blocks reads the path, so linking's scope alone never leaves it unread.
    Reads,
    /// Code-blocks leaves the path out by this key of `[features.code-blocks]`.
    LeavesOut(&'static str),
}

impl CodeBlocksScope {
    /// Where `path`, a directory when `is_dir`, stands in code-blocks' scope. The walk enters any
    /// directory code-blocks' `exclude` doesn't name, so only `exclude` leaves a directory out.
    fn of(config: &Config, path: &AbsolutePath, is_dir: bool) -> Self {
        let Some(code_blocks) = config.code_blocks() else {
            return Self::Off;
        };
        if code_blocks.excludes(path) {
            Self::LeavesOut("exclude")
        } else if is_dir || code_blocks.scans(path) {
            Self::Reads
        } else {
            Self::LeavesOut("include")
        }
    }
}

/// The finding for an explicit path that the config excludes, naming the `exclude` pattern when
/// one matched and otherwise blaming `include`, each in the config `file` that set it. When
/// code-blocks is on, it leaves the path out too, and `code_blocks` names its key as well.
pub(super) fn excluded(
    listed: &SourcePath,
    pattern: Option<&str>,
    file: &str,
    code_blocks: CodeBlocksScope,
) -> Finding {
    let listed = listed.as_str();
    let (also, to_check) = match code_blocks {
        CodeBlocksScope::LeavesOut(key) => (
            format!(", and by `[features.code-blocks] {key}`"),
            format!(
                "to check its links, change `[features.code-blocks] {key}` to check its code \
                 blocks,"
            ),
        ),
        CodeBlocksScope::Off | CodeBlocksScope::Reads => (String::new(), "to check it,".to_owned()),
    };
    match pattern {
        Some(pattern) => Finding::new(
            "scan-explicit-path-excluded",
            Severity::Warning,
            format!(
                "{listed} was named on the command line but is excluded by `{pattern}` in `exclude` in {file}{also}",
            ),
        )
        .with_help(format!(
            "Remove `{pattern}` from `exclude` in {file} {to_check} or stop passing it"
        )),
        None => Finding::new(
            "scan-explicit-path-excluded",
            Severity::Warning,
            format!(
                "{listed} was named on the command line but is not matched by `include` in {file}{also}",
            ),
        )
        .with_help(format!(
            "Add `{listed}` to `include` in {file} {to_check} or stop passing it"
        )),
    }
}

/// The finding for an explicit path that an ignore file skips, naming the rule and its file as
/// far as `by` may, and in a git work tree offering `git check-ignore` to show the rule.
pub(super) fn ignored(listed: &SourcePath, by: &IgnoredBy) -> Finding {
    let listed = listed.as_str();
    let show_rule = if by.check_ignore {
        format!(
            ". `git check-ignore -v -- {}` shows the rule",
            shell_quote(listed)
        )
    } else {
        String::new()
    };
    let help = format!(
        "Remove that rule from {file} to check `{listed}`, or stop passing it{show_rule}",
        file = by.file,
    );
    Finding::new(
        "scan-explicit-path-ignored",
        Severity::Warning,
        format!(
            "{listed} was named on the command line but is ignored by {rule}",
            rule = by.rule,
        ),
    )
    .with_help(help)
}

/// The version-control directory or nested project that `listed` or one of its parent directories
/// is, which the walk skips whatever the user names. A symlink is never judged here:
/// the walk doesn't follow it, so it gets the symlink warning.
pub(super) fn skipped_dir(root: &AbsolutePath, listed: &SourcePath) -> Option<SkippedDir> {
    let mut relative = String::new();
    let steps = root.steps_to(&root.beneath(listed))?;
    for (segment, step) in listed.as_str().split('/').zip(steps) {
        let current = step.as_path();
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(segment);
        if is_vcs_dir(current) {
            return Some(SkippedDir::Vcs(relative));
        }
        let is_real_dir = fs::symlink_metadata(current).is_ok_and(|metadata| metadata.is_dir());
        if is_real_dir && is_nested_project(current) {
            return Some(SkippedDir::Project(relative));
        }
    }
    None
}

/// Describes the ignore rule that keeps the walk from reaching `path`, or `None` when none does.
///
/// It asks what the walk asked, with the same [`IgnoreRules`] and in the order the walk would:
/// each directory on the way down from the root, then `path` itself. The walk never enters a
/// symlinked directory, so nothing beneath one is judged. A rule is quoted with its file only when
/// that file lies inside the root and is no symlink; any other is blamed on "an ignore file", and
/// its file named by kind and, unless above the root, by path, so no line from outside the root
/// is ever printed.
///
/// Effect: reads ignore files and file metadata beneath the root.
pub(super) fn ignore_rule(
    rules: &IgnoreRules,
    root: &AbsolutePath,
    path: &AbsolutePath,
    is_dir: bool,
) -> Option<IgnoredBy> {
    let mut steps = root.steps_to(path)?.peekable();
    let mut above = root.clone();
    while let Some(step) = steps.next() {
        let is_last = steps.peek().is_none();
        let dir = rules.level(above.as_path());
        if !is_last && is_symlink(step.as_path()) {
            return None;
        }
        if let Match::Ignore(rule) =
            rules.matched(dir.as_deref(), step.as_path(), !is_last || is_dir)
        {
            return Some(rule.describe(root, rules.in_work_tree));
        }
        above = step;
    }
    None
}

/// Whether `path` is itself a symlink.
///
/// Effect: reads `path`'s metadata.
pub(super) fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}
