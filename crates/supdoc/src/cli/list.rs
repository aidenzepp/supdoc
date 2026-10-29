//! `supdoc list`: shows every link and its state, or only the links to one target (06 X11).
//!
//! Read-only, and always exit 0: `check` is the gate. The run is `load_config → read_lock → scan →
//! reconcile → filter by TARGET and --state → cap at --limit`, with effects only at the two ends.

use std::io::{self, Write};

use schemars::JsonSchema;
use serde::Serialize;

use crate::check::model::LinkReport;
use crate::check::reconcile::reconcile;
use crate::config::Config;
use crate::finding::Finding;
use crate::link::{LinkKey, LinkRef, SymbolPath, Target};
use crate::output::json::Envelope;
use crate::output::summary::render_summary;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, Report, Summary, TextContext};
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile, SymbolTable};
use crate::scan::model::Scope;

use super::args::ListArgs;
use super::check::LinkJson;
use super::project::{load_config, read_lock};
use super::{CliError, Context, Outcome};

/// Runs `supdoc list`: prints each link in the chosen states, optionally only those pointing at
/// `TARGET`, up to `--limit`, then the summary of every match.
///
/// A missing `supdoc.lock` is an empty one, so every link is `new`. `TARGET` is read relative to
/// the current directory, and `ns:path` names the same file as the plain path to it. It is not an
/// error for `TARGET` to name a file that doesn't exist or has no links.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), the lockfile's own error (78 when
/// invalid, 74 when unreadable), `scan/root-unreadable` (74), and `cli/list-output` when the
/// report cannot be written (74).
pub fn run(args: &ListArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let config = load_config(context, output, "list")?;
    let lock = read_lock(config.root())?;
    let scan = crate::scan::assemble::run(&config, &Languages::builtin(), &Scope::All).map_err(
        |error| {
            CliError::new(
                error.code(),
                error.to_string(),
                error.help(),
                error.exit_code(),
            )
        },
    )?;
    let model = reconcile(&scan, &lock, &Scope::All);

    let wanted = args
        .target
        .as_ref()
        .map(|target| Wanted::new(target, &config, &context.cwd));
    let matched: Vec<LinkReport> = model
        .links
        .into_iter()
        .filter(|link| args.state.is_empty() || args.state.contains(&link.state))
        .filter(|link| {
            wanted
                .as_ref()
                .is_none_or(|wanted| wanted.matches(&link.key, &config))
        })
        .collect();

    let total = matched.len();
    let shown = if args.limit == 0 { total } else { args.limit };
    let report = ListReport::new(&matched, shown, model.findings);
    output
        .report(&report, true)
        .map_err(|error| output_failed(&error))?;
    if report.truncated {
        output
            .note(&format!(
                "Showing {} of {total} links. Use --limit 0 to show all.",
                report.links.len()
            ))
            .map_err(|error| output_failed(&error))?;
    } else if total == 0 {
        let note = match &args.target {
            Some(target) => format!("No links to {target}."),
            None => "No links found.".to_owned(),
        };
        output.note(&note).map_err(|error| output_failed(&error))?;
    }
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc list`'s `--json` output: the whole envelope, with `data` typed as
/// [`ListReport`].
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, ListReport>))
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/list-output",
        format!("cannot write the link list: {error}"),
        "Check that standard output and standard error are open and writable",
        CliError::IO_EXIT_CODE,
    )
}

/// What `list` prints: the counts of every match, the links to show, and the scan's findings not
/// tied to one link (07 G8).
///
/// JSON puts `summary` and `links` under `data`; the envelope carries the loose findings.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ListReport {
    /// How many links are in each state, counting every match, not only those shown.
    summary: Summary,
    /// The links shown, at most `--limit`, each with the finding that explains its state.
    links: Vec<LinkJson>,
    /// Whether `--limit` left matching links out.
    #[serde(skip)]
    truncated: bool,
    /// The findings that explain the links shown, for GitHub annotations.
    #[serde(skip)]
    link_findings: Vec<Finding>,
    /// Findings not tied to one link.
    #[serde(skip)]
    loose: Vec<Finding>,
}

impl ListReport {
    /// The report for `matched`, showing the first `shown` links.
    fn new(matched: &[LinkReport], shown: usize, loose: Vec<Finding>) -> Self {
        let summary = Summary::tally(matched.iter().map(|link| link.state));
        let truncated = matched.len() > shown;
        let listed: Vec<&LinkReport> = matched.iter().take(shown).collect();
        Self {
            summary,
            links: listed.iter().map(|link| LinkJson::from(*link)).collect(),
            truncated,
            link_findings: listed
                .iter()
                .filter_map(|link| link.finding.clone())
                .collect(),
            loose,
        }
    }
}

impl Report for ListReport {
    fn command(&self) -> &'static str {
        "list"
    }

    fn summary(&self) -> Option<&Summary> {
        Some(&self.summary)
    }

    fn findings(&self) -> &[Finding] {
        &self.loose
    }

    fn annotated_findings(&self) -> Vec<&Finding> {
        self.link_findings.iter().chain(&self.loose).collect()
    }

    fn truncated(&self) -> bool {
        self.truncated
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        for link in &self.links {
            render_link(context, link, out)?;
        }
        for finding in &self.loose {
            render_finding(context, finding, out)?;
        }
        if !self.links.is_empty() {
            render_summary(context, &self.summary, out)?;
        }
        Ok(())
    }
}

/// `failed    docs/guide.md:12  →  src/lib.rs#add`: the state, where the link is written (the first
/// place, plus `(+N more)`; nothing for an obsolete entry), and what it points at.
///
/// # Errors
///
/// Fails if writing to `out` fails.
fn render_link(context: &TextContext, link: &LinkJson, out: &mut dyn Write) -> io::Result<()> {
    let arrow = if context.unicode { "→" } else { "->" };
    let mut place = visible(&context.paths.text(&link.key.source));
    if let Some(first) = link.locations.first() {
        place = format!("{place}:{}", first.line);
        let more = link.locations.len() - 1;
        if more > 0 {
            place = format!("{place} (+{more} more)");
        }
    }
    let mut target = link.key.target.to_string();
    if let Some(symbol) = &link.key.symbol {
        target = format!("{target}#{symbol}");
    }
    let state = link.state.to_string();
    writeln!(out, "{state:<8}  {place}  {arrow}  {}", visible(&target))
}

/// The `TARGET` a user typed, resolved once so each link can be compared with it.
struct Wanted {
    /// The file `TARGET` names.
    place: Place,
    /// Which links to that file count.
    symbol: SymbolFilter,
}

/// A file a link or `TARGET` points at, as far as it can be located.
#[derive(Debug, PartialEq, Eq)]
enum Place {
    /// The file's absolute path, so `auth:login.rs` and `services/auth/login.rs` are one place.
    File(AbsolutePath),
    /// A target in a namespace the config doesn't define: compared as written.
    Unlocated(Target),
}

impl Place {
    /// Where `target` points. A plain path is joined onto `plain_base`, and a namespaced one onto
    /// its namespace's directory.
    fn of(target: &Target, plain_base: &AbsolutePath, config: &Config) -> Self {
        let base = match target {
            Target::Path(_) => plain_base,
            Target::Namespaced { namespace, .. } => match config.namespace(namespace) {
                Some(directory) => directory,
                None => return Self::Unlocated(target.clone()),
            },
        };
        // Joining a relative path onto an absolute one is always absolute.
        let file = AbsolutePath::new(&base.as_path().join(target.path().as_str()))
            .unwrap_or_else(|| base.clone());
        Self::File(file)
    }
}

/// Which symbol links to the wanted file count.
enum SymbolFilter {
    /// The whole file was named: links to the file and to any symbol in it.
    Any,
    /// The symbol could not be resolved in the file, so links match by the text they were written
    /// with.
    Text(SymbolPath),
    /// Links match when they were written with the same text, or resolve to the same symbol.
    Resolved {
        /// What `TARGET` was written with.
        typed: SymbolPath,
        /// The file's symbols.
        table: SymbolTable,
        /// The file the table is from.
        file: AbsolutePath,
        /// The full path of the symbol `typed` resolves to.
        symbol: SymbolPath,
    },
}

impl Wanted {
    /// Resolves what `target` names: relative to `cwd` for a plain path, then in the file itself
    /// for a symbol.
    fn new(target: &LinkRef, config: &Config, cwd: &AbsolutePath) -> Self {
        let place = Place::of(&target.target, cwd, config);
        let symbol = match (&target.symbol, &place) {
            (None, _) => SymbolFilter::Any,
            (Some(typed), Place::Unlocated(_)) => SymbolFilter::Text(typed.clone()),
            (Some(typed), Place::File(path)) => Self::resolve_symbol(typed, path),
        };
        Self { place, symbol }
    }

    /// `typed` resolved in `path`, or the plain text when the file can't be read or has no such
    /// symbol.
    fn resolve_symbol(typed: &SymbolPath, path: &AbsolutePath) -> SymbolFilter {
        let Ok(file) = SourceFile::read(path.clone(), &Languages::builtin()) else {
            return SymbolFilter::Text(typed.clone());
        };
        let table = file.language().symbols(&file);
        let Some(symbol) = table
            .resolve(file.path(), typed)
            .ok()
            .map(|found| found.path.clone())
        else {
            return SymbolFilter::Text(typed.clone());
        };
        SymbolFilter::Resolved {
            typed: typed.clone(),
            table,
            file: file.path().clone(),
            symbol,
        }
    }

    /// Whether the link `key` points at the wanted file, and at the wanted symbol when one was
    /// named.
    fn matches(&self, key: &LinkKey, config: &Config) -> bool {
        if Place::of(&key.target, config.root(), config) != self.place {
            return false;
        }
        match (&self.symbol, &key.symbol) {
            (SymbolFilter::Any, _) => true,
            (_, None) => false,
            (SymbolFilter::Text(typed), Some(written)) => typed == written,
            (
                SymbolFilter::Resolved {
                    typed,
                    table,
                    file,
                    symbol,
                },
                Some(written),
            ) => {
                typed == written
                    || table
                        .resolve(file, written)
                        .is_ok_and(|found| found.path == *symbol)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex};

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::{Cli, Command};
    use crate::lock::model::{LOCK_FILE_NAME, Lockfile};
    use crate::output::{OutputSettings, PathStyle, Verbosity};
    use crate::scan::model::TargetState;

    /// The code the guides point at.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n\
                       pub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n";

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

    /// What one `supdoc list` run returned and printed.
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

        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.stdout)
                .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", self.stdout))
        }

        /// Each listed link as `source target#symbol state` text, in output order.
        fn listed(&self) -> Vec<String> {
            let json = self.json();
            let links = json
                .pointer("/data/links")
                .and_then(serde_json::Value::as_array)
                .unwrap_or_else(|| panic!("`/data/links` should be an array: {json}"));
            links
                .iter()
                .map(|link| {
                    let field = |name: &str| {
                        link.get(name)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_owned()
                    };
                    match link.get("symbol") {
                        Some(_) => format!(
                            "{} {}#{} {}",
                            field("source"),
                            field("target"),
                            field("symbol"),
                            field("state")
                        ),
                        None => {
                            format!("{} {} {}", field("source"), field("target"), field("state"))
                        }
                    }
                })
                .collect()
        }
    }

    /// A project in a fresh temporary directory.
    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        fn new() -> Self {
            let project = Self {
                dir: tempfile::tempdir().expect("a fresh temp directory should be created"),
            };
            project.write("src/lib.rs", LIB);
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

        /// Records every present link's current hash in `supdoc.lock`.
        fn record_all(&self) {
            let config = Config::load(&self.root()).expect("the fixture config should load");
            let scan = crate::scan::assemble::run(&config, &Languages::builtin(), &Scope::All)
                .expect("the fixture project should scan");
            let mut lock = Lockfile::default();
            for (key, link) in scan.links {
                if let TargetState::Present { hash, .. } = link.target {
                    let _previous = lock.insert(key, hash);
                }
            }
            let path = AbsolutePath::new(&self.dir.path().join(LOCK_FILE_NAME))
                .expect("the lockfile path should be absolute");
            lock.write(&path)
                .expect("the fixture lockfile should be written");
        }

        /// Runs `supdoc <args>` from the project root. `args` must start with `list`.
        fn list(&self, args: &[&str]) -> Run {
            self.list_from("", args)
        }

        /// Runs `supdoc <args>` from the directory `relative` under the root.
        fn list_from(&self, relative: &str, args: &[&str]) -> Run {
            let cwd = AbsolutePath::new(&self.dir.path().join(relative))
                .expect("a directory under the root should be absolute");
            let command_line: Vec<&str> = std::iter::once("supdoc")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&command_line).unwrap_or_else(|error| {
                panic!("`{}` should parse: {error}", command_line.join(" "))
            });
            let Command::List(list_args) = &cli.command else {
                panic!("`{}` should be a list command", command_line.join(" "));
            };
            let settings = OutputSettings {
                format: cli.global.format(),
                verbosity: Verbosity::from_flags(cli.global.quiet, cli.global.verbose),
                color: false,
                unicode: false,
                width: 80,
                paths: PathStyle::new(cwd.clone(), cwd.clone(), None),
            };
            let (stdout, stderr) = (Capture::default(), Capture::default());
            let mut output =
                Output::new(settings, Box::new(stdout.clone()), Box::new(stderr.clone()));
            let context = Context {
                global: cli.global.clone(),
                cwd,
                home: None,
                claude_config_dir: None,
            };
            let result = run(list_args, &context, &mut output);
            Run {
                result,
                stdout: stdout.text(),
                stderr: stderr.text(),
            }
        }
    }

    /// A project with one passed link (`docs/a.md` to `add`) and one new link (`docs/b.md` to
    /// `sub`).
    fn passed_and_new() -> Project {
        let project = Project::new();
        project.write("docs/a.md", "See [add](../src/lib.rs#add).\n");
        project.record_all();
        project.write("docs/b.md", "See [sub](../src/lib.rs#sub).\n");
        project
    }

    #[test]
    fn list_without_arguments_shows_every_state_including_passed() {
        let project = passed_and_new();
        project.write("docs/c.md", "See [gone](../src/lib.rs#gone).\n");

        let run = project.list(&["--json", "list"]);

        assert_eq!(
            run.listed(),
            [
                "docs/a.md src/lib.rs#add passed",
                "docs/b.md src/lib.rs#sub new",
                "docs/c.md src/lib.rs#gone broken",
            ],
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn list_file_target_matches_whole_file_and_symbol_links() {
        let project = Project::new();
        project.write("src/other.rs", "pub fn other() {}\n");
        project.write(
            "docs/a.md",
            "[whole](../src/lib.rs), [add](../src/lib.rs#add) and [other](../src/other.rs).\n",
        );

        let run = project.list(&["--json", "list", "src/lib.rs"]);

        assert_eq!(
            run.listed(),
            ["docs/a.md src/lib.rs new", "docs/a.md src/lib.rs#add new"],
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn list_symbol_target_excludes_other_symbols() {
        let project = Project::new();
        project.write(
            "docs/a.md",
            "[whole](../src/lib.rs), [add](../src/lib.rs#add), [sub](../src/lib.rs#sub).\n",
        );

        let run = project.list(&["--json", "list", "src/lib.rs#add"]);

        assert_eq!(
            run.listed(),
            ["docs/a.md src/lib.rs#add new"],
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn list_symbol_target_matches_a_link_that_no_longer_resolves_by_its_text() {
        let project = Project::new();
        project.write("docs/a.md", "[gone](../src/lib.rs#gone).\n");

        let run = project.list(&["--json", "list", "src/lib.rs#gone"]);

        assert_eq!(
            run.listed(),
            ["docs/a.md src/lib.rs#gone broken"],
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn list_namespaced_and_plain_path_targets_match_the_same_links() {
        let project = Project::new();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nauth = \"services/auth\"\n",
        );
        project.write("services/auth/login.rs", "pub fn login() {}\n");
        project.write(
            "docs/a.md",
            "[viaNs](auth:login.rs#login) and [plain](../services/auth/login.rs#login).\n",
        );

        let namespaced = project.list(&["--json", "list", "auth:login.rs#login"]);
        let plain = project.list(&["--json", "list", "services/auth/login.rs#login"]);

        assert_eq!(
            namespaced.listed().len(),
            2,
            "stdout: {}",
            namespaced.stdout
        );
        assert_eq!(namespaced.listed(), plain.listed());
    }

    #[test]
    fn list_target_is_relative_to_cwd_from_a_subdirectory() {
        let project = Project::new();
        project.write("supdoc.config.toml", "");
        project.write("docs/a.md", "[add](../src/lib.rs#add).\n");

        let from_docs = project.list_from("docs", &["--json", "list", "../src/lib.rs#add"]);
        let wrong_base = project.list_from("docs", &["--json", "list", "src/lib.rs#add"]);

        assert_eq!(
            from_docs.listed(),
            ["docs/a.md src/lib.rs#add new"],
            "stdout: {}",
            from_docs.stdout
        );
        assert_eq!(
            wrong_base.listed(),
            Vec::<String>::new(),
            "`src/lib.rs` from `docs/` is `docs/src/lib.rs`, which nothing links to"
        );
        let text = project.list_from("docs", &["list", "../src/lib.rs"]);
        assert!(
            text.stdout
                .starts_with("new       a.md:1  ->  src/lib.rs#add\n"),
            "text paths are relative to the current directory: {}",
            text.stdout
        );
    }

    #[test]
    fn list_state_filter_is_a_union() {
        let project = passed_and_new();
        project.write("docs/c.md", "See [gone](../src/lib.rs#gone).\n");

        let union = project.list(&["--json", "list", "--state", "passed,broken"]);
        let new_only = project.list(&["--json", "list", "--state", "new"]);

        assert_eq!(
            union.listed(),
            [
                "docs/a.md src/lib.rs#add passed",
                "docs/c.md src/lib.rs#gone broken"
            ],
            "stdout: {}",
            union.stdout
        );
        assert_eq!(new_only.listed(), ["docs/b.md src/lib.rs#sub new"]);
    }

    /// A project where 101 documents link to `src/lib.rs`.
    fn many_links() -> Project {
        let project = Project::new();
        for number in 0..101 {
            project.write(&format!("docs/n{number:03}.md"), "[lib](../src/lib.rs).\n");
        }
        project
    }

    #[test]
    fn list_caps_at_100_and_signals_truncation() {
        let project = many_links();

        let json = project.list(&["--json", "list"]);
        let text = project.list(&["list"]);

        assert_eq!(json.listed().len(), 100, "the default cap is 100");
        assert_eq!(json.json().pointer("/truncated"), Some(&true.into()));
        assert!(
            text.stderr
                .contains("Showing 100 of 101 links. Use --limit 0 to show all."),
            "stderr: {}",
            text.stderr
        );
        let json_stderr = project.list(&["--json", "list"]).stderr;
        assert_eq!(json_stderr, "", "JSON output hides the note");
    }

    #[test]
    fn limit_0_means_no_limit() {
        let project = many_links();

        let run = project.list(&["--json", "list", "--limit", "0"]);

        assert_eq!(run.listed().len(), 101);
        assert_eq!(run.json().pointer("/truncated"), Some(&false.into()));
    }

    #[test]
    fn list_summary_counts_matches_beyond_the_limit() {
        let project = many_links();

        let run = project.list(&["--json", "list", "--limit", "1"]);

        assert_eq!(run.listed().len(), 1);
        assert_eq!(
            run.json().pointer("/data/summary/new"),
            Some(&101.into()),
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn list_no_match_exits_0_with_stderr_note_and_empty_stdout() {
        let project = Project::new();
        project.write("docs/a.md", "[add](../src/lib.rs#add).\n");
        project.write("src/other.rs", "pub fn other() {}\n");

        let run = project.list(&["list", "src/other.rs"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!(run.stdout, "");
        assert_eq!(run.stderr, "No links to src/other.rs.\n");
    }

    #[test]
    fn list_with_failing_links_still_exits_0() {
        let project = passed_and_new();
        project.write("docs/c.md", "See [gone](../src/lib.rs#gone).\n");
        project.write("src/lib.rs", "pub fn add() {}\n");

        let run = project.list(&["list"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert!(
            run.stdout.contains("failed") && run.stdout.contains("broken"),
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn text_shows_state_first_place_and_summary() {
        let project = Project::new();
        project.write(
            "docs/a.md",
            "[add](../src/lib.rs#add)\n\nAgain [add](../src/lib.rs#add).\n",
        );
        project.record_all();
        project.write("docs/a.md", "No links.\n");

        let run = project.list(&["list"]);

        assert_eq!(
            run.stdout,
            "obsolete  docs/a.md  ->  src/lib.rs#add\nLinks  1 obsolete\n"
        );
        project.write(
            "docs/a.md",
            "[add](../src/lib.rs#add)\n\nAgain [add](../src/lib.rs#add).\n",
        );
        let run = project.list(&["list"]);
        assert_eq!(
            run.stdout,
            "passed    docs/a.md:1 (+1 more)  ->  src/lib.rs#add\nLinks  1 passed\n"
        );
    }

    /// Every way `json` breaks the schema `schema()` publishes for `supdoc list`.
    fn schema_violations(json: &serde_json::Value) -> Vec<String> {
        let schema = schema().expect("list is built, so it has a schema");
        let schema = serde_json::to_value(schema).expect("the list schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        validator
            .iter_errors(json)
            .map(|error| error.to_string())
            .collect()
    }

    #[test]
    fn list_json_validates_against_its_schema() {
        let project = passed_and_new();
        project.write("docs/c.md", "See [gone](../src/lib.rs#gone).\n");

        let json = project.list(&["--json", "list"]).json();

        let errors = schema_violations(&json);
        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
    }

    #[test]
    fn list_schema_rejects_an_envelope_without_its_schema_version() {
        let project = passed_and_new();
        let mut json = project.list(&["--json", "list"]).json();
        let _version = json
            .as_object_mut()
            .expect("the envelope should be a JSON object")
            .remove("schema_version")
            .expect("the envelope should carry schema_version");

        assert!(
            !schema_violations(&json).is_empty(),
            "an envelope without schema_version should fail the schema: {json}"
        );
    }
}
