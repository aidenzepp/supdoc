//! `supdoc config show` and `supdoc namespace list`: what the loaded config says, and which file
//! said it (06a §5.6, §5.7).
//!
//! Both are read-only. Each runs `load_config → report`, and the two share the namespace row.

use std::io::{self, Write};

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::{Config, PatternDefinition};
use crate::hash::LineEndings;
use crate::limits::{Limits, Setting, Source};
use crate::output::json::Envelope;
use crate::output::paths::relative_to;
use crate::output::text::visible;
use crate::output::{Output, RecordArray, Report, TextContext, Verbosity};
use crate::path::AbsolutePath;

use super::args::{Inspect, Limit};
use super::project::{load_config, lock_path};
use super::{CliError, Context, Outcome};

/// Runs `supdoc config show` or `supdoc namespace list`, as `inspect` says.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable) and `cli-output-failed` when the
/// report cannot be written (74).
pub(crate) fn run(
    inspect: Inspect<'_>,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let config = load_config(context, output)?;
    let written = match inspect {
        Inspect::ConfigShow => output
            .report(&ConfigShowReport::new(&config), true)
            .and_then(|()| {
                if config.files().is_empty() {
                    output.note(&format!(
                        "No supdoc.config.toml found; using defaults rooted at {}.",
                        context.cwd.display()
                    ))
                } else {
                    Ok(())
                }
            }),
        Inspect::NamespaceList(args) => {
            let report = NamespaceListReport::new(&config, args.limit);
            output.report(&report, true).and_then(|()| {
                if report.truncated {
                    output.note(&Limit::truncation_note(
                        report.namespaces.len(),
                        report.total,
                        "namespaces",
                    ))
                } else if report.namespaces.is_empty() {
                    output.note("No namespaces configured.")
                } else {
                    Ok(())
                }
            })
        }
    };
    let subject = match inspect {
        Inspect::ConfigShow => "the config show report",
        Inspect::NamespaceList(_) => "the namespace list report",
    };
    written.map_err(|error| CliError::output_failed(subject, &error))?;
    Ok(Outcome::Clean)
}

/// The JSON Schema of the `--json` output of `supdoc config show`: the whole envelope, with `data`
/// typed as the command's report.
#[must_use]
pub(crate) fn config_show_schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, ConfigShowReport>)
}

/// The JSON Schema of the `--json` output of `supdoc namespace list`: the whole envelope, with
/// `data` typed as the command's report. The shape is the same for every `--limit`.
#[must_use]
pub(crate) fn namespace_list_schema() -> schemars::Schema {
    schemars::schema_for!(Envelope<'static, NamespaceListReport>)
}

/// What `config show` prints: the resolved config and the file each part came from.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ConfigShowReport {
    /// The config files in the chain, child first. Empty when running on defaults.
    files: Vec<String>,
    /// The project's lockfile, whether or not it exists yet.
    lockfile: String,
    /// The files to read links from.
    include: PatternsJson,
    /// The files removed from `include`.
    exclude: PatternsJson,
    /// Every namespace, by name.
    namespaces: Vec<NamespaceJson>,
    /// The feature settings in effect.
    features: FeaturesJson,
    /// Every `[limits]` ceiling in effect and where it came from.
    limits: LimitsJson,
    /// The files in the chain, for text output.
    #[serde(skip)]
    chain: Vec<AbsolutePath>,
    /// The lockfile's location, for text output.
    #[serde(skip)]
    lock: AbsolutePath,
}

impl ConfigShowReport {
    /// The report for `config`.
    fn new(config: &Config) -> Self {
        let root = config.root();
        Self {
            limits: LimitsJson::new(root, config.limits_in_effect()),
            files: config
                .files()
                .iter()
                .map(|file| relative_to(root, file))
                .collect(),
            lockfile: relative_to(root, &lock_path(root)),
            include: PatternsJson::new(
                root,
                config.include_definition(),
                vec![DEFAULT_INCLUDE.to_owned()],
            ),
            exclude: PatternsJson::new(root, config.exclude_definition(), Vec::new()),
            namespaces: namespace_rows(config),
            features: FeaturesJson::new(config),
            chain: config.files().to_vec(),
            lock: lock_path(root),
        }
    }
}

impl Report for ConfigShowReport {
    fn record_arrays(&self) -> &'static [RecordArray] {
        const { &[RecordArray::new("namespaces", "namespace")] }
    }

    fn command(&self) -> &'static str {
        "config show"
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let arrow = context.arrow();
        match self.chain.split_first() {
            None => writeln!(out, "config     (none; defaults)")?,
            Some((child, parents)) => {
                writeln!(
                    out,
                    "config     {}",
                    visible(&context.paths.text_absolute(child))
                )?;
                for parent in parents {
                    writeln!(
                        out,
                        "extends    {}",
                        visible(&context.paths.text_absolute(parent))
                    )?;
                }
            }
        }
        writeln!(
            out,
            "lockfile   {}",
            visible(&context.paths.text_absolute(&self.lock))
        )?;
        render_patterns(context, "include", &self.include, out)?;
        render_patterns(context, "exclude", &self.exclude, out)?;
        let line_endings = &self.features.hash.line_endings;
        let origin = match &line_endings.defined_in {
            Some(file) => format!("(from {})", visible(&context.paths.text_absolute(file))),
            None => "(default)".to_owned(),
        };
        writeln!(out, "line-endings  {}  {origin}", line_endings.value)?;
        for (key, limit) in self.limits.rows() {
            let origin = match &limit.origin {
                Source::Flag => format!("(from --{key})"),
                Source::Env => format!("(from SUPDOC_{})", key.to_uppercase().replace('-', "_")),
                Source::Config(file) => {
                    format!("(from {})", visible(&context.paths.text_absolute(file)))
                }
                Source::Default => "(default)".to_owned(),
            };
            writeln!(out, "{key:<15} {}  {origin}", limit.value)?;
        }
        for namespace in &self.namespaces {
            writeln!(
                out,
                "namespace  {} {arrow} {}  ({}){}",
                visible(&namespace.name),
                visible(&context.paths.text_absolute(&namespace.folder)),
                visible(&context.paths.text_absolute(&namespace.defined_in)),
                missing_mark(namespace),
            )?;
        }
        Ok(())
    }
}

/// What `namespace list` prints: every effective namespace, sorted by name.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct NamespaceListReport {
    /// Every namespace up to `--limit`, by name.
    namespaces: Vec<NamespaceJson>,
    /// Whether `--limit` left namespaces out.
    #[serde(skip)]
    truncated: bool,
    /// How many namespaces exist, shown or not.
    #[serde(skip)]
    total: usize,
}

impl NamespaceListReport {
    /// The report for `config`, showing at most `limit` namespaces.
    fn new(config: &Config, limit: Limit) -> Self {
        let all = namespace_rows(config);
        let total = all.len();
        Self {
            namespaces: limit.take(all).collect(),
            truncated: limit.truncated(total),
            total,
        }
    }
}

impl Report for NamespaceListReport {
    fn record_arrays(&self) -> &'static [RecordArray] {
        const { &[RecordArray::new("namespaces", "namespace")] }
    }

    fn command(&self) -> &'static str {
        "namespace list"
    }

    fn truncated(&self) -> bool {
        self.truncated
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let arrow = context.arrow();
        let width = self
            .namespaces
            .iter()
            .map(|namespace| visible(&namespace.name).chars().count())
            .max()
            .unwrap_or(0);
        for namespace in &self.namespaces {
            let name = visible(&namespace.name);
            write!(
                out,
                "{name:<width$}  {arrow}  {}{}",
                visible(&context.paths.text_absolute(&namespace.folder)),
                missing_mark(namespace),
            )?;
            if context.verbosity >= Verbosity::Verbose {
                write!(
                    out,
                    "  (from {})",
                    visible(&context.paths.text_absolute(&namespace.defined_in))
                )?;
            }
            writeln!(out)?;
        }
        Ok(())
    }
}

/// The `include` pattern that matches every file, which applies when no config sets `include`.
const DEFAULT_INCLUDE: &str = "**";

/// The patterns of `include` or `exclude` and the file that set them.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PatternsJson {
    /// The patterns as written, in order. The built-in default when `from` is `null`.
    patterns: Vec<String>,
    /// The config file that set them, relative to the project root. `null` for the default.
    from: Option<String>,
    /// The file that set them, for text output.
    #[serde(skip)]
    file: Option<AbsolutePath>,
}

impl PatternsJson {
    /// The patterns `definition` holds, or `default` when no config file sets the key.
    fn new(
        root: &AbsolutePath,
        definition: Option<PatternDefinition<'_>>,
        default: Vec<String>,
    ) -> Self {
        match definition {
            Some(definition) => Self {
                patterns: definition.patterns().to_vec(),
                from: Some(relative_to(root, definition.file())),
                file: Some(definition.file().clone()),
            },
            None => Self {
                patterns: default,
                from: None,
                file: None,
            },
        }
    }
}

/// The `[features]` settings in effect.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct FeaturesJson {
    /// The `[features.hash]` settings.
    hash: HashFeatureJson,
}

impl FeaturesJson {
    /// The feature settings `config` resolves to.
    fn new(config: &Config) -> Self {
        let root = config.root();
        let defined_in = config.line_endings_defined_in();
        Self {
            hash: HashFeatureJson {
                line_endings: LineEndingsJson {
                    value: config.hash_settings().line_endings,
                    from: defined_in.map(|file| relative_to(root, file)),
                    defined_in: defined_in.cloned(),
                },
            },
        }
    }
}

/// The `[limits]` ceilings in effect, each with where it came from.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LimitsJson {
    /// How long one read of a file may take.
    parse_timeout: LimitJson,
    /// How large `supdoc.lock` may be.
    max_lock_size: LimitJson,
    /// How large each config file may be. Set only by flag or environment variable.
    max_config_size: LimitJson,
    /// How large each ignore file may be.
    max_ignore_size: LimitJson,
}

impl LimitsJson {
    /// The report of `limits`, with config files relative to `root`.
    fn new(root: &AbsolutePath, limits: &Limits) -> Self {
        Self {
            parse_timeout: LimitJson::new(root, &limits.parse_timeout),
            max_lock_size: LimitJson::new(root, &limits.max_lock_size),
            max_config_size: LimitJson::new(root, &limits.max_config_size),
            max_ignore_size: LimitJson::new(root, &limits.max_ignore_size),
        }
    }

    /// Each limit with its key, in the order text output shows them.
    const fn rows(&self) -> [(&'static str, &LimitJson); 4] {
        [
            ("parse-timeout", &self.parse_timeout),
            ("max-lock-size", &self.max_lock_size),
            ("max-config-size", &self.max_config_size),
            ("max-ignore-size", &self.max_ignore_size),
        ]
    }
}

/// Where a limit's value came from, as JSON names it.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum LimitSource {
    /// The command-line flag.
    Flag,
    /// The `SUPDOC_*` environment variable.
    Env,
    /// The config file named in `from`.
    Config,
    /// The built-in default.
    Default,
}

/// One limit and where it came from.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LimitJson {
    /// The ceiling with its unit, such as `10s` or `256KiB`; `0` for no limit.
    value: String,
    /// `flag`, `env`, `config` or `default`.
    source: LimitSource,
    /// The config file that set it, relative to the project root, when `source` is `config`.
    from: Option<String>,
    /// Where it came from, for text output.
    #[serde(skip)]
    origin: Source,
}

impl LimitJson {
    /// The report of `setting`, with a config file relative to `root`.
    fn new<T: std::fmt::Display>(root: &AbsolutePath, setting: &Setting<T>) -> Self {
        let (source, from) = match &setting.source {
            Source::Flag => (LimitSource::Flag, None),
            Source::Env => (LimitSource::Env, None),
            Source::Config(file) => (LimitSource::Config, Some(relative_to(root, file))),
            Source::Default => (LimitSource::Default, None),
        };
        Self {
            value: setting.value.to_string(),
            source,
            from,
            origin: setting.source.clone(),
        }
    }
}

/// The `[features.hash]` table as `config show` reports it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct HashFeatureJson {
    /// How line endings are treated when hashing.
    line_endings: LineEndingsJson,
}

/// The `line-endings` setting and the file that set it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LineEndingsJson {
    /// `normalize` or `exact`.
    value: LineEndings,
    /// The config file that set it, relative to the project root. `null` for the default.
    from: Option<String>,
    /// The file that set it, for text output.
    #[serde(skip)]
    defined_in: Option<AbsolutePath>,
}

/// One namespace as `config show` and `namespace list` show it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct NamespaceJson {
    /// The namespace's name, as links write it before the colon.
    name: String,
    /// The directory it names, relative to the project root.
    path: String,
    /// The config file that defined it, relative to the project root.
    from: String,
    /// Whether the directory exists now.
    exists: bool,
    /// The directory, for text output.
    #[serde(skip)]
    folder: AbsolutePath,
    /// The defining file, for text output.
    #[serde(skip)]
    defined_in: AbsolutePath,
}

/// Every namespace of `config`, sorted by name, with whether its directory exists.
fn namespace_rows(config: &Config) -> Vec<NamespaceJson> {
    let root = config.root();
    config
        .namespaces()
        .iter()
        .map(|(name, namespace)| {
            let folder = namespace.dir();
            let defined_in = namespace.defined_in();
            NamespaceJson {
                name: name.to_string(),
                path: relative_to(root, folder),
                from: relative_to(root, defined_in),
                exists: folder.as_path().is_dir(),
                folder: folder.clone(),
                defined_in: defined_in.clone(),
            }
        })
        .collect()
}

/// `include  docs/**, README.md  (from base.toml)`, or `(default)` when no file sets the key.
///
/// # Errors
///
/// Fails if writing to `out` fails.
fn render_patterns(
    context: &TextContext,
    key: &str,
    patterns: &PatternsJson,
    out: &mut dyn Write,
) -> io::Result<()> {
    let list = if patterns.patterns.is_empty() {
        "(none)".to_owned()
    } else {
        visible(&patterns.patterns.join(", "))
    };
    let origin = match &patterns.file {
        Some(file) => format!("(from {})", visible(&context.paths.text_absolute(file))),
        None => "(default)".to_owned(),
    };
    writeln!(out, "{key:<10} {list}  {origin}")
}

/// ` (missing)` for a namespace whose directory is gone, and nothing otherwise.
const fn missing_mark(namespace: &NamespaceJson) -> &'static str {
    if namespace.exists { "" } else { " (missing)" }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cli::args::{Command, ConfigCommand, NamespaceCommand};
    use crate::cli::testing::{Project, Run, Session, at};
    use crate::exit::ExitStatus;

    impl Project {
        /// Runs `supdoc <args>` from the project root.
        fn run(&self, args: &[&str]) -> Run {
            run_in(self.root(), args)
        }
    }

    /// Runs `supdoc <args>` from `cwd`. `args` must name `config show` or `namespace list`.
    fn run_in(cwd: AbsolutePath, args: &[&str]) -> Run {
        let mut session = Session::new(cwd, args);
        let inspect = match &session.cli.command {
            Command::Config(ConfigCommand::Show) => Inspect::ConfigShow,
            Command::Namespace(NamespaceCommand::List(args)) => Inspect::NamespaceList(args),
            _ => panic!(
                "`supdoc {}` should be config show or namespace list",
                args.join(" ")
            ),
        };
        let result = super::run(inspect, &session.context, &mut session.output);
        session.finished(result)
    }

    /// A parent config in the project root and a child under `child/` that extends it.
    fn chain_project(parent: &str, child_extra: &str) -> Project {
        let project = Project::empty();
        project.write("supdoc.config.toml", parent);
        project.write(
            "child/supdoc.config.toml",
            &format!("extends = \"../supdoc.config.toml\"\n{child_extra}"),
        );
        project
    }

    /// Runs `supdoc <args>` from the child directory of a [`chain_project`].
    fn run_in_child(project: &Project, args: &[&str]) -> Run {
        let cwd = AbsolutePath::new(&project.dir.path().join("child"))
            .expect("the child directory should be absolute");
        run_in(cwd, args)
    }

    #[test]
    fn config_show_names_the_file_each_key_came_from_in_an_extends_chain() {
        let project = chain_project(
            "include = [\"docs\"]\nexclude = [\"docs/drafts\"]\n[namespaces]\nauth = \"child/services/auth\"\n",
            "[namespaces]\nweb = \"apps/web\"\n",
        );

        let run = run_in_child(&project, &["--json", "config", "show"]);

        assert_eq!(run.exit_code(), 0, "stdout: {}", run.stdout);
        let json = run.json();
        assert_eq!(
            at(&json, "/data/files"),
            serde_json::json!(["supdoc.config.toml", "../supdoc.config.toml"]),
            "files are the chain, child first, relative to the child's root: {json}"
        );
        assert_eq!(at(&json, "/data/lockfile"), "supdoc.lock");
        assert_eq!(
            at(&json, "/data/include"),
            serde_json::json!({ "patterns": ["docs"], "from": "../supdoc.config.toml" }),
        );
        assert_eq!(
            at(&json, "/data/exclude"),
            serde_json::json!({ "patterns": ["docs/drafts"], "from": "../supdoc.config.toml" }),
        );
        assert_eq!(
            at(&json, "/data/namespaces"),
            serde_json::json!([
                { "name": "auth", "path": "services/auth",
                  "from": "../supdoc.config.toml", "exists": false },
                { "name": "web", "path": "apps/web",
                  "from": "supdoc.config.toml", "exists": false },
            ]),
        );
    }

    #[test]
    fn config_show_without_config_reports_defaults_and_notes_it() {
        let project = Project::empty();

        let json = project.run(&["--json", "config", "show"]).json();
        let text = project.run(&["config", "show"]);

        assert_eq!(at(&json, "/data/files"), serde_json::json!([]));
        assert_eq!(
            at(&json, "/data/include"),
            serde_json::json!({ "patterns": ["**"], "from": null }),
        );
        assert_eq!(
            at(&json, "/data/exclude"),
            serde_json::json!({ "patterns": [], "from": null }),
        );
        assert_eq!(at(&json, "/data/namespaces"), serde_json::json!([]));
        assert_eq!(text.exit_code(), 0);
        assert!(
            text.stdout.contains("(default)"),
            "defaults are labelled as such: {}",
            text.stdout
        );
        assert_eq!(
            text.stderr,
            format!(
                "No supdoc.config.toml found; using defaults rooted at {}.\n",
                project.root().display()
            ),
        );
    }

    #[test]
    fn config_show_inherited_include_names_the_parent_file() {
        let project = chain_project("include = [\"docs\"]\n", "");

        let run = run_in_child(&project, &["config", "show"]);

        assert_eq!(run.exit_code(), 0);
        assert!(
            run.stdout
                .contains("include    docs  (from ../supdoc.config.toml)"),
            "include comes from the parent, and exclude from nowhere: {}",
            run.stdout
        );
        assert!(
            run.stdout.contains("exclude    (none)  (default)"),
            "an unset exclude is labelled default: {}",
            run.stdout
        );
    }

    #[test]
    fn config_show_child_namespace_override_names_the_child() {
        let project = chain_project(
            "[namespaces]\nauth = \"child/parent/auth\"\nweb = \"child/parent/web\"\n",
            "[namespaces]\nauth = \"child/auth\"\n",
        );

        let json = run_in_child(&project, &["--json", "config", "show"]).json();

        assert_eq!(
            at(&json, "/data/namespaces/0"),
            serde_json::json!({
                "name": "auth", "path": "child/auth",
                "from": "supdoc.config.toml", "exists": false,
            }),
            "the nearer definition wins and names its own file: {json}"
        );
        assert_eq!(
            at(&json, "/data/namespaces/1/from"),
            "../supdoc.config.toml",
            "a namespace the child doesn't redefine keeps the parent: {json}"
        );
    }

    #[test]
    fn config_show_broken_config_exits_78() {
        let project = Project::empty();
        project.write("supdoc.config.toml", "include = 3\n");

        let run = project.run(&["config", "show"]);

        let error = run
            .result
            .expect_err("a config with a wrong type should not load");
        assert_eq!(
            (error.code(), error.exit_code()),
            ("config-parse", ExitStatus::Config.code())
        );
        assert_eq!(run.stdout, "", "a fatal error leaves stdout empty");
    }

    /// Every way `json` breaks `schema`, the schema a command publishes.
    fn schema_violations(schema: schemars::Schema, json: &serde_json::Value) -> Vec<String> {
        let schema = serde_json::to_value(schema).expect("the schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        validator
            .iter_errors(json)
            .map(|error| error.to_string())
            .collect()
    }

    #[test]
    fn config_show_json_validates_against_its_schema() {
        let project = chain_project(
            "include = [\"docs\"]\n[namespaces]\nauth = \"child/services/auth\"\n",
            "",
        );
        let json = run_in_child(&project, &["--json", "config", "show"]).json();

        let errors = schema_violations(config_show_schema(), &json);

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
        assert_eq!(at(&json, "/command"), "config show");
    }

    #[test]
    fn namespace_list_caps_at_limit_and_signals_truncation() {
        let project = Project::empty();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nweb = \"apps/web\"\nauth = \"services/auth\"\ncore = \"core\"\n",
        );

        let run = project.run(&["--json", "namespace", "list", "--limit", "2"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/truncated"), true, "json: {json}");
        assert_eq!(
            at(&json, "/data/namespaces").as_array().map(Vec::len),
            Some(2),
            "json: {json}"
        );

        let text = project.run(&["namespace", "list", "--limit", "2"]);
        assert!(
            text.stderr.contains("Showing 2 of 3 namespaces."),
            "stderr: {}",
            text.stderr
        );
    }

    #[test]
    fn namespace_list_limit_0_shows_every_namespace() {
        let project = Project::empty();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nweb = \"apps/web\"\nauth = \"services/auth\"\ncore = \"core\"\n",
        );

        let run = project.run(&["--json", "namespace", "list", "--limit", "0"]);

        let json = run.json();
        assert_eq!(at(&json, "/truncated"), false, "json: {json}");
        assert_eq!(
            at(&json, "/data/namespaces").as_array().map(Vec::len),
            Some(3),
            "json: {json}"
        );
    }

    #[test]
    fn namespace_list_shows_name_and_folder_sorted() {
        let project = Project::empty();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nweb = \"apps/web\"\nauth = \"services/auth\"\n",
        );
        project.write("apps/web/keep", "");
        project.write("services/auth/keep", "");

        let run = project.run(&["namespace", "list"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!(
            run.stdout, "auth  ->  services/auth\nweb   ->  apps/web\n",
            "rows are sorted by name with aligned arrows"
        );
        assert_eq!(run.stderr, "");
    }

    #[test]
    fn namespace_list_includes_inherited_namespaces_resolved_from_their_config() {
        let project = chain_project(
            "[namespaces]\nshared = \"child/lib/shared\"\n",
            "[namespaces]\nown = \"own\"\n",
        );

        let json = run_in_child(&project, &["--json", "namespace", "list"]).json();
        let verbose = run_in_child(&project, &["-v", "namespace", "list"]);

        assert_eq!(
            at(&json, "/data/namespaces"),
            serde_json::json!([
                { "name": "own", "path": "own",
                  "from": "supdoc.config.toml", "exists": false },
                { "name": "shared", "path": "lib/shared",
                  "from": "../supdoc.config.toml", "exists": false },
            ]),
            "the parent's namespace resolves against the parent's directory: {json}"
        );
        assert!(
            verbose.stdout.contains("(from ../supdoc.config.toml)"),
            "-v names the defining file: {}",
            verbose.stdout
        );
    }

    #[test]
    fn namespace_list_empty_prints_nothing_and_notes_on_stderr() {
        let project = Project::empty();
        project.write("supdoc.config.toml", "");

        let run = project.run(&["namespace", "list"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!(run.stdout, "");
        assert_eq!(run.stderr, "No namespaces configured.\n");
    }

    #[test]
    fn namespace_list_marks_missing_folder() {
        let project = Project::empty();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nghost = \"nowhere\"\nreal = \"here\"\n",
        );
        project.write("here/keep", "");

        let text = project.run(&["namespace", "list"]);
        let json = project.run(&["--json", "namespace", "list"]).json();

        assert!(
            text.stdout.contains("ghost  ->  nowhere (missing)"),
            "stdout: {}",
            text.stdout
        );
        assert!(
            text.stdout.contains("real   ->  here\n"),
            "an existing folder isn't marked: {}",
            text.stdout
        );
        assert_eq!(at(&json, "/data/namespaces/0/exists"), false);
        assert_eq!(at(&json, "/data/namespaces/1/exists"), true);
    }

    #[test]
    fn namespace_list_json_validates_against_its_schema() {
        let project = Project::empty();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nauth = \"services/auth\"\n",
        );
        let json = project.run(&["--json", "namespace", "list"]).json();

        let errors = schema_violations(namespace_list_schema(), &json);

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
        assert_eq!(at(&json, "/command"), "namespace list");
    }
}
