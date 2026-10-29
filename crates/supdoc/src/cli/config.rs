//! `supdoc config show` and `supdoc namespace list`: what the loaded config says, and which file
//! said it (06a §5.6, §5.7).
//!
//! Both are read-only. Each runs `load_config → report`, and the two share the namespace row.

use std::io::{self, Write};

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::{Config, PatternDefinition};
use crate::hash::LineEndings;
use crate::output::json::Envelope;
use crate::output::paths::relative_to;
use crate::output::text::visible;
use crate::output::{Output, OutputError, Report, TextContext, Verbosity};
use crate::path::AbsolutePath;

use super::args::Inspect;
use super::project::{load_config, lock_path};
use super::{CliError, Context, Outcome};

/// Runs `supdoc config show` or `supdoc namespace list`, as `inspect` says.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable) and `cli/config-output` when the
/// report cannot be written (74).
pub fn run(inspect: Inspect, context: &Context, output: &mut Output) -> Result<Outcome, CliError> {
    let config = load_config(context, output, command(inspect))?;
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
        Inspect::NamespaceList => {
            let report = NamespaceListReport::new(&config);
            output.report(&report, true).and_then(|()| {
                if report.namespaces.is_empty() {
                    output.note("No namespaces configured.")
                } else {
                    Ok(())
                }
            })
        }
    };
    written.map_err(|error| output_failed(inspect, &error))?;
    Ok(Outcome::Clean)
}

/// The JSON Schema of the `--json` output of `supdoc config show` or `supdoc namespace list`, as
/// `inspect` says: the whole envelope, with `data` typed as the command's report.
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema(inspect: Inspect) -> Result<schemars::Schema, CliError> {
    Ok(match inspect {
        Inspect::ConfigShow => schemars::schema_for!(Envelope<'static, ConfigShowReport>),
        Inspect::NamespaceList => schemars::schema_for!(Envelope<'static, NamespaceListReport>),
    })
}

/// The command `inspect` names, as a user types it after `supdoc`.
const fn command(inspect: Inspect) -> &'static str {
    match inspect {
        Inspect::ConfigShow => "config show",
        Inspect::NamespaceList => "namespace list",
    }
}

/// The error for a report that could not be written.
fn output_failed(inspect: Inspect, error: &OutputError) -> CliError {
    let (code, name) = match inspect {
        Inspect::ConfigShow => ("cli/config-output", "config show"),
        Inspect::NamespaceList => ("cli/namespace-output", "namespace list"),
    };
    CliError::new(
        code,
        format!("cannot write the {name} report: {error}"),
        "Check that standard output and standard error are open and writable",
        CliError::IO_EXIT_CODE,
    )
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
    fn command(&self) -> &'static str {
        "config show"
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let arrow = arrow(context);
        match self.chain.split_first() {
            None => writeln!(out, "config     (none; defaults)")?,
            Some((child, parents)) => {
                writeln!(out, "config     {}", shown(context, child))?;
                for parent in parents {
                    writeln!(out, "extends    {}", shown(context, parent))?;
                }
            }
        }
        writeln!(out, "lockfile   {}", shown(context, &self.lock))?;
        render_patterns(context, "include", &self.include, out)?;
        render_patterns(context, "exclude", &self.exclude, out)?;
        let line_endings = &self.features.hash.line_endings;
        let origin = match &line_endings.defined_in {
            Some(file) => format!("(from {})", shown(context, file)),
            None => "(default)".to_owned(),
        };
        writeln!(out, "line-endings  {}  {origin}", line_endings.value)?;
        for namespace in &self.namespaces {
            writeln!(
                out,
                "namespace  {} {arrow} {}  ({}){}",
                visible(&namespace.name),
                shown(context, &namespace.folder),
                shown(context, &namespace.defined_in),
                missing_mark(namespace),
            )?;
        }
        Ok(())
    }
}

/// What `namespace list` prints: every effective namespace, sorted by name.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct NamespaceListReport {
    /// Every namespace, by name.
    namespaces: Vec<NamespaceJson>,
}

impl NamespaceListReport {
    /// The report for `config`.
    fn new(config: &Config) -> Self {
        Self {
            namespaces: namespace_rows(config),
        }
    }
}

impl Report for NamespaceListReport {
    fn command(&self) -> &'static str {
        "namespace list"
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let arrow = arrow(context);
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
                shown(context, &namespace.folder),
                missing_mark(namespace),
            )?;
            if context.verbosity >= Verbosity::Verbose {
                write!(out, "  (from {})", shown(context, &namespace.defined_in))?;
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
                    file: defined_in.map(|file| relative_to(root, file)),
                    defined_in: defined_in.cloned(),
                },
            },
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
    file: Option<String>,
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
        .map(|(name, folder)| {
            // A loaded config records the defining file of every namespace; the root only
            // stands in for a record that cannot be missing.
            let defined_in = config.namespace_defined_in(name).unwrap_or(root);
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
        Some(file) => format!("(from {})", shown(context, file)),
        None => "(default)".to_owned(),
    };
    writeln!(out, "{key:<10} {list}  {origin}")
}

/// ` (missing)` for a namespace whose directory is gone, and nothing otherwise.
const fn missing_mark(namespace: &NamespaceJson) -> &'static str {
    if namespace.exists { "" } else { " (missing)" }
}

/// The arrow between a namespace and its directory.
const fn arrow(context: &TextContext) -> &'static str {
    if context.unicode { "→" } else { "->" }
}

/// `path` as text output shows it: relative to the current directory.
fn shown(context: &TextContext, path: &AbsolutePath) -> String {
    visible(&context.paths.text_absolute(path))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex};

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::{Cli, Command, ConfigCommand, NamespaceCommand};
    use crate::output::{OutputSettings, PathStyle};

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

    /// What one run returned and printed.
    struct Run {
        result: Result<Outcome, CliError>,
        stdout: String,
        stderr: String,
    }

    impl Run {
        /// The exit code the process would end with.
        fn exit_code(&self) -> u8 {
            match &self.result {
                Ok(outcome) => outcome.exit_code(),
                Err(error) => error.exit_code(),
            }
        }

        /// stdout parsed as the one JSON document `--json` prints.
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.stdout)
                .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", self.stdout))
        }
    }

    /// The value at `pointer` (RFC 6901) in `json`, or a failure naming what is missing.
    fn at(json: &serde_json::Value, pointer: &str) -> serde_json::Value {
        json.pointer(pointer)
            .unwrap_or_else(|| panic!("`{pointer}` should be in the JSON output: {json}"))
            .clone()
    }

    /// A project in a fresh temporary directory.
    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("a fresh temp directory should be created"),
            }
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

        /// Runs `supdoc <args>` from the project root.
        fn run(&self, args: &[&str]) -> Run {
            run_in(self.root(), args)
        }
    }

    /// Runs `supdoc <args>` from `cwd`, with output paths provisionally rooted there as `main`
    /// roots them before any config loads. `args` must name `config show` or `namespace list`.
    fn run_in(cwd: AbsolutePath, args: &[&str]) -> Run {
        let command_line: Vec<&str> = std::iter::once("supdoc")
            .chain(args.iter().copied())
            .collect();
        let cli = Cli::try_parse_from(&command_line)
            .unwrap_or_else(|error| panic!("`{}` should parse: {error}", command_line.join(" ")));
        let inspect = match &cli.command {
            Command::Config(ConfigCommand::Show) => Inspect::ConfigShow,
            Command::Namespace(NamespaceCommand::List) => Inspect::NamespaceList,
            _ => panic!(
                "`{}` should be config show or namespace list",
                command_line.join(" ")
            ),
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
        let mut output = Output::new(settings, Box::new(stdout.clone()), Box::new(stderr.clone()));
        let context = Context {
            global: cli.global,
            cwd,
            home: None,
            claude_config_dir: None,
        };
        let result = super::run(inspect, &context, &mut output);
        Run {
            result,
            stdout: stdout.text(),
            stderr: stderr.text(),
        }
    }

    /// A parent config in the project root and a child under `child/` that extends it.
    fn chain_project(parent: &str, child_extra: &str) -> Project {
        let project = Project::new();
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
        let project = Project::new();

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
        let project = Project::new();
        project.write("supdoc.config.toml", "include = 3\n");

        let run = project.run(&["config", "show"]);

        let error = run
            .result
            .expect_err("a config with a wrong type should not load");
        assert_eq!(
            (error.code(), error.exit_code()),
            ("config/parse", CliError::CONFIG_EXIT_CODE)
        );
        assert_eq!(run.stdout, "", "a fatal error leaves stdout empty");
    }

    /// Every way `json` breaks the schema `schema(inspect)` publishes.
    fn schema_violations(inspect: Inspect, json: &serde_json::Value) -> Vec<String> {
        let schema = schema(inspect).expect("the inspect commands are built, so they have schemas");
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

        let errors = schema_violations(Inspect::ConfigShow, &json);

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
        assert_eq!(at(&json, "/command"), "config show");
    }

    #[test]
    fn namespace_list_shows_name_and_folder_sorted() {
        let project = Project::new();
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
        let project = Project::new();
        project.write("supdoc.config.toml", "");

        let run = project.run(&["namespace", "list"]);

        assert_eq!(run.exit_code(), 0);
        assert_eq!(run.stdout, "");
        assert_eq!(run.stderr, "No namespaces configured.\n");
    }

    #[test]
    fn namespace_list_marks_missing_folder() {
        let project = Project::new();
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
        let project = Project::new();
        project.write(
            "supdoc.config.toml",
            "[namespaces]\nauth = \"services/auth\"\n",
        );
        let json = project.run(&["--json", "namespace", "list"]).json();

        let errors = schema_violations(Inspect::NamespaceList, &json);

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
        assert_eq!(at(&json, "/command"), "namespace list");
    }
}
