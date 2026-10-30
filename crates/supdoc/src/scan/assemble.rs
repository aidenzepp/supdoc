//! The scan's entry point, [`run`]: walk the project, read every source once, extract its links,
//! then resolve each target once (04).
//!
//! Only an unreadable project root stops the run. Every other problem, with one file or one link,
//! becomes a finding and the scan goes on (S7).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use crate::config::{CONFIG_FILE_NAME, Config};
use crate::finding::{Finding, Fix, Location, Severity, Snippet};
use crate::link::destination::{LinkError, parse_destination};
use crate::link::{LinkKey, RawLink, SourcePath};
use crate::namespace::NamespaceName;
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile};
use crate::read::error::ReadError;
use crate::read::links::links;
use crate::scan::model::{Scan, ScannedLink, Scope, TargetState};
use crate::scan::targets::{self, CODE_UNKNOWN_NAMESPACE, Occurrence, Parsed, Written};
use crate::scan::walk::walk;

/// Exit status for a scan that couldn't read the project (06 X7).
const EXIT_IO: u8 = 74;

/// Why a scan couldn't run at all (04 S7). Every smaller problem is a finding instead.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The project root itself can't be listed, so no result would mean anything.
    #[error("cannot read the project root {}: {error}", .root.display())]
    RootUnreadable {
        /// The project root.
        root: AbsolutePath,
        /// What the operating system reported.
        error: std::io::Error,
    },
}

impl ScanError {
    /// The stable code for this error: `scan-root-unreadable`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RootUnreadable { .. } => "scan-root-unreadable",
        }
    }

    /// What to do next.
    #[must_use]
    pub fn help(&self) -> &'static str {
        match self {
            Self::RootUnreadable { .. } => {
                "Give the current user read access to the project root, for example with \
                 `chmod u+r` on it, or run from a directory you can read"
            }
        }
    }

    /// The process exit status for this error (06 X7): 74, since the project can't be read.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::RootUnreadable { .. } => EXIT_IO,
        }
    }
}

/// Scans the part of the project `scope` names: every link in its sources, what each target looks
/// like right now, and every problem met on the way, in a deterministic order (S8). Each file is
/// parsed at most once (S9). The lockfile is never read.
///
/// Effect: reads the filesystem. Everything after the walk and the file reads is pure.
///
/// # Errors
///
/// [`ScanError::RootUnreadable`] when the project root can't be listed.
pub fn run(config: &Config, languages: &Languages, scope: &Scope) -> Result<Scan, ScanError> {
    assemble(config, languages, scope).map(|(scan, _parsed)| scan)
}

/// [`run`], also handing back the files it parsed, so tests can see that none was parsed twice.
///
/// # Errors
///
/// As [`run`].
fn assemble(
    config: &Config,
    languages: &Languages,
    scope: &Scope,
) -> Result<(Scan, Parsed), ScanError> {
    let root = config.root();
    if let Err(error) = fs::read_dir(root.as_path()) {
        return Err(ScanError::RootUnreadable {
            root: root.clone(),
            error,
        });
    }

    let walked = walk(config, languages, scope);
    let mut findings = walked.findings;
    let mut parsed = Parsed::default();
    let mut unread_sources = BTreeSet::new();
    let mut found = SourceLinks::default();
    for (path, _cartridge) in &walked.sources {
        // The walk only returns files beneath the root whose paths are valid source paths.
        let Some(source) = path
            .strip_prefix(root)
            .and_then(|relative| SourcePath::from_relative(relative).ok())
        else {
            continue;
        };
        match parsed.read(path, languages) {
            Ok(file) => source_links(config, languages, &source, file, &mut found),
            Err(error) => {
                findings.push(unreadable_source(&source, &error));
                let _new = unread_sources.insert(source);
            }
        }
    }

    // The root itself was read above, so every unread directory lies beneath it.
    let unread_dirs = walked
        .unread_dirs
        .iter()
        .filter_map(|dir| dir.path.strip_prefix(root))
        .filter_map(|relative| SourcePath::from_relative(relative).ok())
        .collect();

    let mut links = targets::resolve(config, languages, &mut parsed, found.occurrences);
    add_broken(&mut links, found.broken);
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
        unread_dirs,
    };
    Ok((scan, parsed))
}

/// Where a finding sits, for sorting by file then position (S8). Findings about no one place sort
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
    /// Links meant for supdoc but wrong (03 R10) that still have a key: broken as written.
    broken: Vec<(LinkKey, Location, Finding)>,
    /// Links meant for supdoc but too malformed to have a key: only their finding is left.
    unkeyed: Vec<Finding>,
}

/// Adds every supdoc link in `file` to `found`, in file order (03 R10).
fn source_links(
    config: &Config,
    languages: &Languages,
    source: &SourcePath,
    file: &SourceFile,
    found: &mut SourceLinks,
) {
    let text = file.text();
    for raw in links(file) {
        let Some(parsed) = parse_destination(&raw, source, config.namespaces(), languages) else {
            continue;
        };
        // A link's range comes from this file's own tree, so it always lies inside the text.
        let Some(location) = Location::in_text(source.clone(), text, raw.range.clone()) else {
            continue;
        };
        let snippet = Snippet::capture(text, raw.range.clone());
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
                let key = unknown_namespace_key(config, languages, &raw, source, &error);
                let finding =
                    link_finding(config, &error, location.clone(), snippet, written.as_ref());
                match key {
                    Some(key) => found.broken.push((key, location, finding)),
                    None => found.unkeyed.push(finding),
                }
            }
        }
    }
}

/// The key `raw` would have if its unknown namespace were configured, so a locked link whose
/// namespace was renamed or removed in config is broken rather than obsolete (05 C1, C3).
///
/// `None` for every other link error, and when the rest of the destination is wrong too: an
/// invalid path or symbol can't be written as a key.
fn unknown_namespace_key(
    config: &Config,
    languages: &Languages,
    raw: &RawLink,
    source: &SourcePath,
    error: &LinkError,
) -> Option<LinkKey> {
    let LinkError::UnknownNamespace { prefix, .. } = error else {
        return None;
    };
    let namespace: NamespaceName = prefix.parse().ok()?;
    let mut namespaces = config.namespaces().clone();
    // Parsing never reads a namespace's directory, so the root stands in for the missing one.
    let _absent = namespaces.insert(namespace, config.root().clone());
    let link = parse_destination(raw, source, &namespaces, languages)?.ok()?;
    Some(LinkKey {
        source: source.clone(),
        target: link.target,
        symbol: link.symbol,
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
            Entry::Occupied(mut link) => link.get_mut().locations.push(location),
            Entry::Vacant(slot) => {
                let _link = slot.insert(ScannedLink {
                    snippet: finding.snippet.clone(),
                    locations: vec![location],
                    target: TargetState::Broken { finding },
                });
            }
        }
    }
}

/// The finding for a destination that is meant for supdoc but wrong. An unknown namespace gets a
/// fix when exactly one configured namespace is a close spelling of it.
fn link_finding(
    config: &Config,
    error: &LinkError,
    location: Location,
    snippet: Option<Snippet>,
    written: Option<&Written>,
) -> Finding {
    let (code, help, fix) = match error {
        LinkError::UnknownNamespace { prefix, known, .. } => {
            let close = targets::close_match(prefix, known.iter().map(NamespaceName::as_str));
            let fix = close.zip(written.and_then(|written| written.prefix_range(prefix)));
            let fix = fix.map(|(close, range)| Fix {
                file: location.file.clone(),
                range,
                replacement: close.to_owned(),
            });
            let help = namespace_help(prefix, close, config);
            (CODE_UNKNOWN_NAMESPACE, help, fix)
        }
        LinkError::InvalidPath { .. } => (
            "link-invalid-path",
            "Write the path with `/` between names, relative to this file or to the project \
             root with a leading `/`"
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
        LinkError::TextFragmentUnsupported { .. } => (
            "link-text-fragment-unsupported",
            "Remove the `:~:` text fragment from the link. supdoc can't read text fragments yet"
                .to_owned(),
            None,
        ),
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

/// What to do about the unknown namespace `prefix`: the likely intended one, then every
/// configured namespace.
fn namespace_help(prefix: &str, close: Option<&str>, config: &Config) -> String {
    let configured: Vec<String> = config
        .namespaces()
        .keys()
        .map(|name| format!("`{name}`"))
        .collect();
    match close {
        _ if configured.is_empty() => format!(
            "No namespaces are configured. Define `{prefix}` under `[namespaces]` in \
             {CONFIG_FILE_NAME}, or remove the prefix"
        ),
        Some(close) => format!(
            "did you mean `{close}`? `supdoc fix` can change it. Configured namespaces: {}",
            configured.join(", ")
        ),
        None => format!(
            "Configured namespaces: {}. Define `{prefix}` under `[namespaces]` in \
             {CONFIG_FILE_NAME}, or correct the prefix",
            configured.join(", ")
        ),
    }
}

/// The warning for a walked source that couldn't be read. Its links weren't checked, and its lock
/// entries are kept (05 C5).
///
/// It points at the start of the file, since no text was read to point into.
fn unreadable_source(source: &SourcePath, error: &ReadError) -> Finding {
    let start = Location {
        file: source.clone(),
        range: 0..0,
        line: 1,
        column: 1,
    };
    Finding::new(
        error.code(),
        Severity::Warning,
        error.message_naming(source),
    )
    .with_location(start)
    .with_help(format!(
        "Links in {source} weren't checked. {}",
        error.help()
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

    use super::*;
    use crate::check::reconcile::{Outcome, check_outcome, reconcile};
    use crate::hash::Hash;
    use crate::link::Target;
    use crate::lock::model::Lockfile;
    use crate::state::LinkState;

    /// Two functions, `add` and `sub`.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n\
                       pub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n";

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
            assemble(&self.config(), &Languages::builtin(), &Scope::All)
                .expect("a readable project root should scan")
        }

        fn scan(&self) -> Scan {
            run(&self.config(), &Languages::builtin(), &Scope::All)
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
            .locations
            .iter()
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
                 did you mean `add`, `sub`? run `supdoc resolve src/lib.rs` from the project root to list its symbols",
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

    // Link errors fail `check` (03 R10, 05 C3)

    /// Whether `check` passes on `project`, given `lock`, without and with `--deny-warnings`.
    fn outcomes(project: &Project, lock: &Lockfile) -> [Outcome; 2] {
        let report = reconcile(&project.scan(), lock, &Scope::All);
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

        let report = reconcile(&project.scan(), &lock, &Scope::All);

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
    fn directory_link_is_not_tracked() {
        let project = Project::new();
        project.write("src/lib.rs", LIB);
        project.write(
            "docs/guide.md",
            "See [src](../src/) and [src](../src#add).\n",
        );

        let scan = project.scan();

        assert_eq!(scan.links.len(), 0, "links to a directory are navigation");
        assert_eq!(scan.findings, Vec::new());
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

        let result = run(&config, &Languages::builtin(), &Scope::All);

        fs::set_permissions(&project.root, fs::Permissions::from_mode(0o755))
            .expect("the root should be unlockable, so the temp dir can be removed");
        let error = result.expect_err("an unreadable root should stop the scan");
        assert_eq!(error.code(), "scan-root-unreadable");
    }

    #[test]
    fn file_that_is_source_and_target_is_parsed_once() {
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
        assert_eq!(parsed.parses, 2, "each file is both a source and a target");
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

    // Targets are read directly, never through the walk (S3, S5)

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

    // Fixes must be verifiable by `supdoc fix` (05 C8)

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
}
