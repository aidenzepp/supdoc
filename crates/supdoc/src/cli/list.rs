//! `supdoc list`: shows every link and its state, or only the links to one target (06 X11).
//!
//! Read-only, and exits 0 whatever state the links are in: `check` is the gate. A `TARGET` that
//! names nothing (a missing file, an unknown namespace, a symbol that doesn't resolve) is refused,
//! so a typo never reads as "no docs depend on this". The run is `load_config → parse TARGET → read_lock → scan →
//! reconcile → filter by TARGET and --state → cap at --limit`, with effects only at the two ends.

use std::io::{self, Write};
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;

use crate::check::model::LinkReport;
use crate::check::reconcile::reconcile_in;
use crate::config::Config;
use crate::exit::ExitStatus;
use crate::finding::Finding;
use crate::link::{LinkKey, LinkRef, SymbolPath, Target};
use crate::lock::error::shell_quote;
use crate::namespace::NamespaceName;
use crate::output::json::Envelope;
use crate::output::paths::relative_to;
use crate::output::summary::render_summary;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, PathStyle, Report, Summary, TextContext};
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile, SymbolTable};
use crate::read::error::{ReadError, ReadRole, ResolveError};
use crate::scan::model::Scope;

use super::args::{Limit, ListArgs, TargetArg};
use super::check::LinkJson;
use super::fix::timed_out;
use super::project::{
    absolute, command_line, ensure_exists, load_config, path_outside_config, read_lock,
};
use super::{CliError, Context, Outcome};

/// Runs `supdoc list`: prints each link in the chosen states, optionally only those pointing at
/// `TARGET`, up to `--limit`, then the summary of every match.
///
/// A missing `supdoc.lock` is an empty one, so every link is `new`. `TARGET` is read relative to
/// the current directory, so `./src/lib.rs`, `../src/lib.rs` and an absolute path inside the
/// project all work, and `ns:path` names the same file as the plain path to it. A real `TARGET`
/// with no links says so and exits 0. When no link points at `TARGET`, it is checked: a missing
/// file, an unknown namespace or a symbol that doesn't resolve is an error. Links recorded for a
/// file that is gone still list.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), the lockfile's own error (78 when
/// invalid, 74 when unreadable), `cli-path-outside-project` (2) for a plain `TARGET` outside the
/// project, `cli-target-invalid` (2) for a `TARGET` that isn't a target; when no link points at
/// `TARGET`, `link-unknown-namespace` or `cli-path-missing` (2), `cli-path-unreadable` (74), or
/// `resolve-not-found` or `resolve-ambiguous` (1); `scan-root-unreadable` (74), and
/// `cli-list-output` when the report cannot be written (74).
pub fn run(args: &ListArgs, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let config = load_config(context, output)?;
    let target = args
        .target
        .as_ref()
        .map(|typed| parse_target(typed, &config, &context.cwd))
        .transpose()?;
    let lock = read_lock(&config, &context.cwd)?;
    let scan = crate::scan::assemble::run(&config, &Languages::builtin(), &Scope::All)
        .map_err(CliError::from)?;
    let paths = PathStyle::new(context.cwd.clone(), config.root().clone(), None);
    let model = reconcile_in(&scan, &lock, &Scope::All, &paths);

    let wanted = target
        .as_ref()
        .map(|target| Wanted::new(target, &config))
        .transpose()?;
    let linked: Vec<LinkReport> = model
        .links
        .into_iter()
        .filter(|link| {
            wanted
                .as_ref()
                .is_none_or(|wanted| wanted.matches(&link.key, &config))
        })
        .collect();
    if linked.is_empty()
        && let (Some(target), Some(wanted), Some(typed)) = (&target, &wanted, &args.target)
        && let Some(error) = wanted.refusal(target, typed, &config, context)
    {
        return Err(error);
    }
    let matched: Vec<LinkReport> = linked
        .into_iter()
        .filter(|link| args.state.is_empty() || args.state.contains(&link.state))
        .collect();

    let total = matched.len();
    let timed_out = timed_out(&scan).next().is_some();
    let report = ListReport::new(&matched, args.limit, model.findings);
    output
        .report(&report, !timed_out)
        .map_err(|error| CliError::output_failed("cli-list-output", "the link list", &error))?;
    if report.truncated {
        output
            .note(
                &args
                    .limit
                    .truncation_note(report.links.len(), total, "links"),
            )
            .map_err(|error| CliError::output_failed("cli-list-output", "the link list", &error))?;
    } else if total == 0 {
        let note = match &target {
            Some(target) => format!("No links to {target}."),
            None => "No links found.".to_owned(),
        };
        output
            .note(&note)
            .map_err(|error| CliError::output_failed("cli-list-output", "the link list", &error))?;
    }
    // A file that outlasted `parse-timeout` left its links unchecked, so the run fails (D1).
    Ok(if timed_out {
        Outcome::Failing
    } else {
        Outcome::Clean
    })
}

/// The JSON Schema of `supdoc list`'s `--json` output: the whole envelope, with `data` typed as
/// [`ListReport`].
#[must_use]
pub fn schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, ListReport>)
}

/// The `TARGET` as typed, made into a [`LinkRef`] whose plain path is relative to the project root.
///
/// A plain path is taken relative to `cwd` and normalized, so `./src/lib.rs` and an absolute path
/// inside the project name the same file as `src/lib.rs`. A `ns:path` is left alone.
///
/// # Errors
///
/// Returns `cli-path-outside-project` for a plain path outside the project and
/// `cli-target-invalid` for text that is not a target. Both exit 2.
fn parse_target(arg: &TargetArg, config: &Config, cwd: &AbsolutePath) -> Result<LinkRef, CliError> {
    let typed = arg.typed();
    let rewritten;
    let text = if arg.is_namespaced() {
        typed
    } else {
        let path = arg.file();
        let absolute = absolute(cwd, Path::new(path));
        let relative = absolute
            .strip_prefix(config.root())
            .ok_or_else(|| path_outside_config(Path::new(path), config))?;
        let inside = relative
            .to_str()
            .filter(|inside| !inside.is_empty())
            .ok_or_else(|| target_invalid(typed, "it must name a file inside the project"))?;
        let inside = inside.replace(std::path::MAIN_SEPARATOR, "/");
        rewritten = match arg.symbol() {
            Some(symbol) => format!("{inside}#{symbol}"),
            None => inside,
        };
        &rewritten
    };
    text.parse::<LinkRef>()
        .map_err(|error| target_invalid(typed, &error.to_string()))
}

/// The error for a `TARGET` that is not `path`, `path#symbol` or `ns:path#symbol`.
fn target_invalid(typed: &str, reason: &str) -> CliError {
    CliError::new(
        "cli-target-invalid",
        format!("`{typed}` is not a link target: {reason}"),
        "Write TARGET as `path`, `path#symbol` or `ns:path#symbol`",
        ExitStatus::Usage,
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
    /// The report for `matched`, showing at most `limit` links.
    fn new(matched: &[LinkReport], limit: Limit, loose: Vec<Finding>) -> Self {
        let summary = Summary::tally(matched.iter().map(|link| link.state));
        let truncated = limit.truncated(matched.len());
        let listed: Vec<&LinkReport> = limit.take(matched).collect();
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
    let arrow = context.arrow();
    let mut place = visible(&context.paths.text(&link.key.source));
    if let Some(first) = link.locations.first() {
        place = format!("{place}:{}", first.line);
        let more = link.locations.len() - 1;
        if more > 0 {
            place = format!("{place} (+{more} more)");
        }
    }
    let target = link.key.target_text();
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
    /// A target in a namespace the config doesn't define, or one that lies outside the project
    /// root once symlinks are followed: compared as written, and never read.
    Unlocated(Target),
}

impl Place {
    /// Where `target` points. A plain path is joined onto `plain_base`, and a namespaced one onto
    /// its namespace's directory.
    ///
    /// Effect: follows symlinks along the joined path to check it stays inside the project root.
    fn of(target: &Target, plain_base: &AbsolutePath, config: &Config) -> Self {
        let base = match target {
            Target::Path(_) => plain_base,
            Target::Namespaced { namespace, .. } => match config.namespace(namespace) {
                Some(directory) => directory,
                None => return Self::Unlocated(target.clone()),
            },
        };
        let file = base.join_path(Path::new(target.path().as_str()));
        if !config.contains(base.as_path()) || !config.contains(file.as_path()) {
            return Self::Unlocated(target.clone());
        }
        Self::File(file)
    }
}

/// Which symbol links to the wanted file count.
enum SymbolFilter {
    /// The whole file was named: links to the file and to any symbol in it.
    Any,
    /// The file could not be located or read, so links match by the text they were written with.
    Text(SymbolPath),
    /// The file has no single symbol by that path, so links match by the text they were written
    /// with.
    Unresolved {
        /// What `TARGET` was written with.
        typed: SymbolPath,
        /// Why it names no single symbol in the file.
        error: ResolveError,
    },
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
    /// Resolves what `target` names: a plain path is relative to the project root, as
    /// [`parse_target`] leaves it, then the symbol is looked up in the file itself.
    ///
    /// # Errors
    ///
    /// `read-timeout` (1) when reading the file to resolve the symbol outlasts `parse-timeout`.
    fn new(target: &LinkRef, config: &Config) -> Result<Self, CliError> {
        let place = Place::of(&target.target, config.root(), config);
        let symbol = match (&target.symbol, &place) {
            (None, _) => SymbolFilter::Any,
            (Some(typed), Place::Unlocated(_)) => SymbolFilter::Text(typed.clone()),
            (Some(typed), Place::File(path)) => Self::resolve_symbol(typed, path, config)?,
        };
        Ok(Self { place, symbol })
    }

    /// `typed` resolved in `path`, or the plain text when the file can't be read or has no such
    /// symbol.
    ///
    /// # Errors
    ///
    /// `read-timeout` (1) when reading the file outlasts `parse-timeout`: the run never falls
    /// back to matching text for a file it stopped reading.
    fn resolve_symbol(
        typed: &SymbolPath,
        path: &AbsolutePath,
        config: &Config,
    ) -> Result<SymbolFilter, CliError> {
        let limit = config.limits_in_effect().parse_timeout.value;
        let read = SourceFile::read(path.clone(), &Languages::builtin(), limit)
            .and_then(|file| Ok((file.language().symbols(&file)?, file)));
        let (table, file) = match read {
            Ok(read) => read,
            Err(error @ ReadError::Timeout { .. }) => {
                let shown = relative_to(config.root(), error.path());
                return Err(CliError::new(
                    error.code(),
                    error.message_naming(&shown),
                    error.help(ReadRole::Target, &shown),
                    ExitStatus::Failed,
                ));
            }
            Err(_) => return Ok(SymbolFilter::Text(typed.clone())),
        };
        let symbol = match table.resolve(file.path(), typed) {
            Ok(found) => found.path.clone(),
            Err(error) => {
                return Ok(SymbolFilter::Unresolved {
                    typed: typed.clone(),
                    error,
                });
            }
        };
        Ok(SymbolFilter::Resolved {
            typed: typed.clone(),
            table,
            file: file.path().clone(),
            symbol,
        })
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
            (SymbolFilter::Text(typed) | SymbolFilter::Unresolved { typed, .. }, Some(written)) => {
                typed == written
            }
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

    /// Why `target`, typed as `typed`, names nothing: its namespace isn't defined, its file is
    /// outside the project or missing, or its symbol doesn't resolve. `None` when it names a real
    /// file (and symbol), or when the file can't be read to check the symbol.
    ///
    /// Effect: looks up the file's metadata.
    fn refusal(
        &self,
        target: &LinkRef,
        typed: &TargetArg,
        config: &Config,
        context: &Context,
    ) -> Option<CliError> {
        let cwd = &context.cwd;
        if let Some(name) = target.target.namespace()
            && config.namespace(name).is_none()
        {
            return Some(unknown_namespace(config, name));
        }
        let typed_file = Path::new(typed.file());
        let file = match &self.place {
            Place::File(file) => file,
            Place::Unlocated(_) => return Some(path_outside_config(typed_file, config)),
        };
        if let Err(error) = ensure_exists(file, typed_file, cwd) {
            let namespaced = target
                .target
                .namespace()
                .and_then(|name| Some((name, config.namespace(name)?)));
            return Some(match namespaced {
                Some((name, directory)) if error.code() == "cli-path-missing" => {
                    namespaced_missing(typed_file, name, directory, cwd)
                }
                _ => error,
            });
        }
        match &self.symbol {
            SymbolFilter::Unresolved { error, .. } => Some(CliError::new(
                error.code(),
                error.message_naming(&relative_to(config.root(), error.file())),
                error.help_running(&command_line(
                    context,
                    "resolve",
                    &[typed_file.display().to_string()],
                    &[],
                )),
                ExitStatus::Failed,
            )),
            SymbolFilter::Any | SymbolFilter::Text(_) | SymbolFilter::Resolved { .. } => None,
        }
    }
}

/// The error for a namespaced `TARGET` whose file doesn't exist. Its path is relative to the
/// namespace's `directory`, not the current directory, so the help lists that directory as seen
/// from `cwd`.
fn namespaced_missing(
    typed: &Path,
    name: &NamespaceName,
    directory: &AbsolutePath,
    cwd: &AbsolutePath,
) -> CliError {
    let shown = match relative_to(cwd, directory) {
        empty if empty.is_empty() => ".".to_owned(),
        shown => shown,
    };
    CliError::new(
        "cli-path-missing",
        format!("{} does not exist", typed.display()),
        format!(
            "The path after `{name}:` is relative to the `{name}` namespace's directory. \
             `ls {}` lists what is there",
            shell_quote(&shown)
        ),
        ExitStatus::Usage,
    )
}

/// The error for a namespace the config doesn't define, naming the closest defined one.
fn unknown_namespace(config: &Config, name: &NamespaceName) -> CliError {
    let defined: Vec<&str> = config
        .namespaces()
        .keys()
        .map(NamespaceName::as_str)
        .collect();
    let help = match crate::suggest::close_match(name.as_str(), defined.iter().copied()) {
        Some(nearest) => format!(
            "did you mean `{nearest}`? Defined namespaces: `{}`",
            defined.join("`, `")
        ),
        None if defined.is_empty() => format!(
            "Define `{name}` under `[namespaces]` in {}",
            crate::config::CONFIG_FILE_NAME
        ),
        None => format!("Defined namespaces: `{}`", defined.join("`, `")),
    };
    CliError::new(
        "link-unknown-namespace",
        format!("unknown namespace `{name}`"),
        help,
        ExitStatus::Usage,
    )
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cli::args::Command;
    use crate::cli::testing::{Project, Run, Session};

    /// The code the guides point at.
    const LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n\
                       pub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n";

    /// A project with the code the guides point at.
    fn project() -> Project {
        let project = Project::empty();
        project.write("src/lib.rs", LIB);
        project
    }

    impl Run {
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

    impl Project {
        /// The absolute path of `relative` under the root.
        fn absolute(&self, relative: &str) -> String {
            self.dir.path().join(relative).display().to_string()
        }

        /// Runs `supdoc <args>` from the project root. `args` must start with `list`.
        fn list(&self, args: &[&str]) -> Run {
            self.list_from("", args)
        }

        /// Runs `supdoc <args>` from the directory `relative` under the root.
        fn list_from(&self, relative: &str, args: &[&str]) -> Run {
            let mut session = Session::new(self.root_of(relative), args);
            let Command::List(list_args) = &session.cli.command else {
                panic!("`supdoc {}` should be a list command", args.join(" "));
            };
            let result = run(list_args, &session.context, &mut session.output);
            session.finished(result)
        }
    }

    /// A project with one passed link (`docs/a.md` to `add`) and one new link (`docs/b.md` to
    /// `sub`).
    fn passed_and_new() -> Project {
        let project = project();
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
        let project = project();
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
        let project = project();
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
        let project = project();
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
        let project = project();
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
        let project = project();
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
            wrong_base.result.as_ref().err().map(CliError::code),
            Some("cli-path-missing"),
            "`src/lib.rs` from `docs/` is `docs/src/lib.rs`, which doesn't exist"
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
    fn list_link_failed_help_names_the_source_from_the_current_directory() {
        let project = project();
        project.write("supdoc.config.toml", "");
        project.write("docs/a.md", "See [add](../src/lib.rs#add).\n");
        project.record_all();
        project.write("src/lib.rs", &LIB.replace("a + b", "b + a"));

        let run = project.list_from("docs", &["--json", "list"]);

        let json = run.json();
        let help = json
            .pointer("/data/links/0/finding/help")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("the changed link should carry help: {json}"));
        assert!(
            help.contains("supdoc update a.md --yes")
                && project.root().as_path().join("docs/a.md").exists(),
            "from docs/ the help should name `a.md`, which exists there: {help}"
        );
    }

    #[test]
    fn list_target_accepts_dot_slash_and_an_absolute_path_inside_the_root() {
        let project = project();
        project.write("docs/a.md", "[add](../src/lib.rs#add).\n");
        let absolute = format!("{}#add", project.absolute("src/lib.rs"));

        let dotted = project.list(&["--json", "list", "./src/lib.rs#add"]);
        let absolute = project.list(&["--json", "list", &absolute]);

        assert_eq!(
            dotted.listed(),
            ["docs/a.md src/lib.rs#add new"],
            "stdout: {}",
            dotted.stdout
        );
        assert_eq!(absolute.listed(), dotted.listed());
    }

    #[test]
    fn list_target_outside_the_root_is_the_path_error() {
        let project = project();

        let run = project.list(&["--json", "list", "/elsewhere/src/lib.rs"]);

        let error = run
            .result
            .expect_err("a target outside the project should be refused");
        assert_eq!(
            (error.code(), error.exit_code()),
            ("cli-path-outside-project", 2)
        );
    }

    #[test]
    fn list_target_outside_the_directory_without_a_config_says_no_config_was_found() {
        let project = project();
        let config = project.dir.path().join(crate::config::CONFIG_FILE_NAME);
        assert!(!config.exists(), "the fixture should have no config");

        let run = project.list(&["list", "/elsewhere/src/lib.rs"]);

        let error = run
            .result
            .expect_err("a target outside the directory should be refused");
        assert_eq!(error.code(), "cli-path-outside-project");
        assert!(
            error
                .message()
                .ends_with("and no supdoc.config.toml was found above it"),
            "the message should say no config was found: {}",
            error.message()
        );
        assert!(
            error.help().contains("supdoc -c <dir>/supdoc.config.toml"),
            "the help should offer -c: {}",
            error.help()
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
        let project = project();
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
        let project = project();
        project.write("docs/a.md", "[add](../src/lib.rs#add).\n");
        project.write("src/other.rs", "pub fn other() {}\n");

        let run = project.list(&["list", "src/other.rs"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!(run.stdout, "");
        assert_eq!(run.stderr, "No links to src/other.rs.\n");
    }

    #[test]
    fn list_of_a_missing_file_is_refused_not_no_links() {
        let project = project();
        project.write("docs/a.md", "[add](../src/lib.rs#add).\n");
        project.write("src/lib.rs", LIB);

        let error = project
            .list(&["list", "src/nope.rs"])
            .result
            .expect_err("a missing file should be refused");

        assert_eq!((error.code(), error.exit_code()), ("cli-path-missing", 2));
    }

    #[test]
    fn list_of_a_misspelled_symbol_is_refused_with_the_nearest_symbol() {
        let project = project();
        project.write("docs/a.md", "[add](../src/lib.rs#add).\n");
        project.write("src/lib.rs", LIB);

        let error = project
            .list(&["list", "src/lib.rs#ad"])
            .result
            .expect_err("a symbol that doesn't resolve should be refused");

        assert_eq!((error.code(), error.exit_code()), ("resolve-not-found", 1));
        assert!(
            error.help().contains("`add`") && error.help().contains("supdoc resolve src/lib.rs"),
            "help: {}",
            error.help()
        );
    }

    #[test]
    fn list_of_an_unknown_namespace_is_refused_naming_the_defined_ones() {
        let project = project();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nauth = \"services/auth\"\n",
        );
        project.write("services/auth/login.rs", "pub fn login() {}\n");
        project.write("docs/a.md", "[login](auth:login.rs#login).\n");

        let error = project
            .list(&["list", "auht:login.rs"])
            .result
            .expect_err("an unknown namespace should be refused");

        assert_eq!(
            (error.code(), error.exit_code()),
            ("link-unknown-namespace", 2)
        );
        assert!(error.help().contains("`auth`"), "help: {}", error.help());
    }

    #[test]
    fn list_of_a_missing_namespaced_file_points_at_the_namespace_directory() {
        let project = project();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nauth = \"services/auth\"\n",
        );
        project.write("services/auth/login.rs", "pub fn login() {}\n");
        project.write("docs/a.md", "[login](auth:login.rs#login).\n");

        let error = project
            .list(&["list", "auth:nope.rs"])
            .result
            .expect_err("a missing namespaced file should be refused");

        assert_eq!((error.code(), error.exit_code()), ("cli-path-missing", 2));
        assert!(
            error.help().contains("`ls services/auth`")
                && !error.help().contains("current directory"),
            "help: {}",
            error.help()
        );
    }

    #[test]
    fn list_still_shows_links_to_a_file_that_is_gone() {
        let project = project();
        project.write("docs/a.md", "[add](../src/gone.rs#add).\n");

        let run = project.list(&["--json", "list", "src/gone.rs#add"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(run.listed().len(), 1, "stdout: {}", run.stdout);
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
        let project = project();
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
        let schema = schema();
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
