//! `supdoc resolve`: lists the symbols a file exposes, or confirms that one symbol resolves in it
//! (06 X11, 03).
//!
//! Read-only. The run is `load_config → locate FILE → SourceFile::read → SymbolTable → list or
//! resolve`, with effects only at the two ends.

use std::io::{self, Write};
use std::path::{Component, Path};
use std::str::FromStr;

use schemars::JsonSchema;
use serde::Serialize;

use crate::config::Config;
use crate::finding::{Finding, Severity};
use crate::link::SymbolPath;
use crate::namespace::NamespaceName;
use crate::output::json::Envelope;
use crate::output::text::{render_finding, visible};
use crate::output::{Output, OutputError, Report, TextContext};
use crate::path::AbsolutePath;
use crate::read::cartridge::{Languages, SourceFile, Symbol};
use crate::read::error::{ReadError, ResolveError};

use super::args::ResolveArgs;
use super::project::{absolute, load_config, path_missing};
use super::{CliError, Context, Outcome};

/// Runs `supdoc resolve`: prints every symbol in `FILE`, or the one `SYMBOL` names.
///
/// `FILE` may be written `namespace:path`, which reads `path` inside the namespace's directory.
///
/// # Errors
///
/// Returns the config's own error (78, or 74 when unreadable), `cli/path-missing` for a missing
/// `FILE` and `link/unknown-namespace` for an undefined namespace (both 2), the file's own read
/// error (1, or 74 for I/O and 70 for a supdoc bug), `resolve/not-found` or `resolve/ambiguous`
/// for a `SYMBOL` that names no single symbol (1), and `cli/resolve-output` when the report
/// cannot be written (74).
pub fn run(
    args: &ResolveArgs,
    context: &Context,
    output: &mut Output,
) -> Result<Outcome, CliError> {
    let config = load_config(context, output, "resolve")?;
    let path = locate(&config, &context.cwd, &args.file)?;
    let file =
        SourceFile::read(path, &Languages::builtin()).map_err(|error| read_failed(&error))?;
    let table = file.language().symbols(&file);

    let (symbols, query, truncated) = if let Some(query) = &args.symbol {
        let symbol = table
            .resolve(file.path(), query)
            .map_err(|error| resolve_failed(&error))?;
        (vec![SymbolJson::new(&file, symbol)], Some(query), false)
    } else {
        let total = table.iter().count();
        let shown = if args.limit == 0 { total } else { args.limit };
        let symbols = table
            .iter()
            .take(shown)
            .map(|symbol| SymbolJson::new(&file, symbol))
            .collect();
        (symbols, None, total > shown)
    };
    let listed = symbols.len();
    let findings = if file.has_syntax_errors() {
        vec![syntax_errors(&file, &context.cwd)]
    } else {
        Vec::new()
    };
    let report = ResolveReport {
        file: relative_to(config.root(), file.path()),
        query: query.map(ToString::to_string),
        symbols,
        shown_as: relative_to(&context.cwd, file.path()),
        truncated,
        findings,
    };
    output
        .report(&report, true)
        .map_err(|error| output_failed(&error))?;
    if truncated {
        output
            .note(&format!(
                "Showing {listed} symbols; there are more. Raise --limit, or use --limit 0 for all."
            ))
            .map_err(|error| output_failed(&error))?;
    } else if listed == 0 {
        output
            .note(&format!(
                "{} has no symbols; link to the whole file instead.",
                report.shown_as
            ))
            .map_err(|error| output_failed(&error))?;
    }
    Ok(Outcome::Clean)
}

/// The JSON Schema of `supdoc resolve`'s `--json` output: the whole envelope, with `data` typed as
/// [`ResolveReport`].
///
/// # Errors
///
/// Never fails; the `Result` matches the commands that are not built yet.
pub fn schema() -> Result<schemars::Schema, CliError> {
    Ok(schemars::schema_for!(Envelope<'static, ResolveReport>))
}

/// The file `typed` names: relative to the current directory, or, when written `namespace:path`,
/// inside the namespace's directory.
///
/// # Errors
///
/// Returns `link/unknown-namespace` for a namespace the config doesn't define, and
/// `cli/path-missing` for a file that doesn't exist. Both exit 2.
fn locate(config: &Config, cwd: &AbsolutePath, typed: &Path) -> Result<AbsolutePath, CliError> {
    let namespaced = typed.to_str().and_then(|text| {
        let (name, rest) = text.split_once(':')?;
        let name = NamespaceName::from_str(name).ok()?;
        (!rest.is_empty()).then_some((name, rest))
    });
    let found = match namespaced {
        Some((name, rest)) => {
            let directory = config
                .namespace(&name)
                .ok_or_else(|| unknown_namespace(config, &name))?;
            // A namespace directory is absolute, so the join is too.
            AbsolutePath::new(&directory.as_path().join(rest)).unwrap_or_else(|| directory.clone())
        }
        None => absolute(cwd, typed),
    };
    if found.as_path().symlink_metadata().is_err() {
        return Err(path_missing(typed));
    }
    Ok(found)
}

/// The error for a namespace the config doesn't define, naming the closest defined one.
fn unknown_namespace(config: &Config, name: &NamespaceName) -> CliError {
    let defined: Vec<&str> = config
        .namespaces()
        .keys()
        .map(NamespaceName::as_str)
        .collect();
    let nearest = crate::scan::targets::close_match(name.as_str(), defined.iter().copied());
    let help = match (nearest, defined.is_empty()) {
        (Some(nearest), _) => format!(
            "Did you mean `{nearest}`? Defined namespaces: {}",
            defined.join(", ")
        ),
        (None, false) => format!("Defined namespaces: {}", defined.join(", ")),
        (None, true) => format!(
            "Define `{name}` under `[namespaces]` in {}",
            crate::config::CONFIG_FILE_NAME
        ),
    };
    CliError::new(
        "link/unknown-namespace",
        format!("unknown namespace `{name}`"),
        help,
        CliError::USAGE_EXIT_CODE,
    )
}

/// The error for a file that couldn't be read, with the read error's own code and help.
fn read_failed(error: &ReadError) -> CliError {
    let exit_code = match error {
        ReadError::Missing { .. } => CliError::USAGE_EXIT_CODE,
        ReadError::Io { .. } => CliError::IO_EXIT_CODE,
        ReadError::Internal { .. } => CliError::BUG_EXIT_CODE,
        ReadError::NotUtf8 { .. }
        | ReadError::TooLarge { .. }
        | ReadError::UnsupportedLanguage { .. } => CliError::REFUSED_EXIT_CODE,
    };
    CliError::new(error.code(), error.to_string(), error.help(), exit_code)
}

/// The error for a `SYMBOL` that names no single symbol, with the resolve error's own code and
/// help.
fn resolve_failed(error: &ResolveError) -> CliError {
    CliError::new(
        error.code(),
        error.to_string(),
        error.help(),
        CliError::REFUSED_EXIT_CODE,
    )
}

/// The error for a report that could not be written.
fn output_failed(error: &OutputError) -> CliError {
    CliError::new(
        "cli/resolve-output",
        format!("cannot write the resolve report: {error}"),
        "Check that standard output and standard error are open and writable",
        CliError::IO_EXIT_CODE,
    )
}

/// The warning that `file` has syntax errors, so its symbols may be incomplete (03 R8).
fn syntax_errors(file: &SourceFile, cwd: &AbsolutePath) -> Finding {
    Finding::new(
        "read/syntax-errors",
        Severity::Warning,
        format!(
            "{} has syntax errors; its symbols may be incomplete",
            relative_to(cwd, file.path())
        ),
    )
    .with_help("Fix the syntax errors, then run `supdoc resolve` again")
}

/// `target` written relative to the directory `base`, climbing with `..` where needed.
fn relative_to(base: &AbsolutePath, target: &AbsolutePath) -> String {
    let base_parts: Vec<Component<'_>> = base.as_path().components().collect();
    let target_parts: Vec<Component<'_>> = target.as_path().components().collect();
    let shared = base_parts
        .iter()
        .zip(&target_parts)
        .take_while(|(left, right)| left == right)
        .count();
    let climbs = std::iter::repeat_n("..".to_owned(), base_parts.len() - shared);
    let descents = target_parts
        .iter()
        .skip(shared)
        .map(|part| part.as_os_str().to_string_lossy().into_owned());
    let parts: Vec<String> = climbs.chain(descents).collect();
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

/// What `resolve` prints: the symbols of one file, or the one symbol asked for.
///
/// JSON puts `file`, `query` and `symbols` under `data`; the envelope carries the warning for a
/// file with syntax errors.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ResolveReport {
    /// The file, relative to the project root. `..` climbs out of it for a file outside.
    file: String,
    /// The `SYMBOL` that was resolved. Left out when listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    /// The symbols shown: every symbol up to `--limit` in file order, or the one resolved.
    symbols: Vec<SymbolJson>,
    /// The file as text output shows it: relative to the current directory.
    #[serde(skip)]
    shown_as: String,
    /// Whether `--limit` left symbols out.
    #[serde(skip)]
    truncated: bool,
    /// Findings not tied to one symbol.
    #[serde(skip)]
    findings: Vec<Finding>,
}

/// One symbol as `resolve --json` shows it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SymbolJson {
    /// The full dotted path: `Config.validate`.
    path: String,
    /// Other paths that resolve to the same symbol, such as a Markdown heading's anchor.
    aliases: Vec<String>,
    /// 1-based line where the symbol starts.
    line: u32,
    /// 1-based column where the symbol starts, in characters.
    column: u32,
    /// Start byte offset.
    start: usize,
    /// End byte offset.
    end: usize,
}

impl SymbolJson {
    /// `symbol` as found in `file`.
    fn new(file: &SourceFile, symbol: &Symbol) -> Self {
        let before = file.text().get(..symbol.range.start).unwrap_or_default();
        let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
        let line = before.matches('\n').count() + 1;
        let column = before
            .get(line_start..)
            .map_or(0, |text| text.chars().count())
            + 1;
        Self {
            path: symbol.path.to_string(),
            aliases: symbol.aliases.iter().map(SymbolPath::to_string).collect(),
            line: u32::try_from(line).unwrap_or(u32::MAX),
            column: u32::try_from(column).unwrap_or(u32::MAX),
            start: symbol.range.start,
            end: symbol.range.end,
        }
    }
}

impl Report for ResolveReport {
    fn command(&self) -> &'static str {
        "resolve"
    }

    fn findings(&self) -> &[Finding] {
        &self.findings
    }

    fn truncated(&self) -> bool {
        self.truncated
    }

    fn render_text(&self, context: &TextContext, out: &mut dyn Write) -> io::Result<()> {
        let file = visible(&self.shown_as);
        for symbol in &self.symbols {
            write!(out, "{file}#{}", visible(&symbol.path))?;
            if self.query.is_none() && !symbol.aliases.is_empty() {
                let aliases: Vec<String> = symbol
                    .aliases
                    .iter()
                    .map(|alias| format!("#{}", visible(alias)))
                    .collect();
                write!(out, "  (also {})", aliases.join(", "))?;
            }
            writeln!(out)?;
        }
        for finding in &self.findings {
            render_finding(context, finding, out)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Mutex};

    use clap::Parser as _;

    use super::*;
    use crate::cli::args::{Cli, Command};
    use crate::output::{OutputSettings, PathStyle, Verbosity};

    /// Rust code with a struct, a method and a free function.
    const LIB: &str = "pub struct Config {\n    port: u16,\n}\n\nimpl Config {\n    pub fn validate(&self) -> bool {\n        self.port > 0\n    }\n}\n\npub fn load() {}\n";

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

    /// The value at `pointer` (RFC 6901) in `json`, or a failure naming what is missing.
    fn at(json: &serde_json::Value, pointer: &str) -> serde_json::Value {
        json.pointer(pointer)
            .unwrap_or_else(|| panic!("`{pointer}` should be in the JSON output: {json}"))
            .clone()
    }

    /// What one `supdoc resolve` run returned and printed.
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

        /// The error the run ended with, or a failure saying it succeeded.
        fn error(&self) -> &CliError {
            self.result
                .as_ref()
                .err()
                .unwrap_or_else(|| panic!("the run should fail, but printed: {}", self.stdout))
        }

        /// stdout parsed as the one JSON document `--json` prints.
        fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.stdout)
                .unwrap_or_else(|error| panic!("stdout should be JSON ({error}): {}", self.stdout))
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
            project.write(
                "supdoc.config.toml",
                "[namespaces]\nauth = \"services/auth\"\n",
            );
            project.write("src/lib.rs", LIB);
            project
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.dir.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("the fixture directory should be created");
            }
            fs::write(&path, text).expect("the fixture file should be written");
        }

        /// Runs `supdoc <args>` from the project root. `args` must start with `resolve` or a
        /// global flag.
        fn resolve(&self, args: &[&str]) -> Run {
            let cwd =
                AbsolutePath::new(self.dir.path()).expect("a temp directory should be absolute");
            let command_line: Vec<&str> = std::iter::once("supdoc")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&command_line).unwrap_or_else(|error| {
                panic!("`{}` should parse: {error}", command_line.join(" "))
            });
            let Command::Resolve(resolve_args) = &cli.command else {
                panic!("`{}` should be a resolve command", command_line.join(" "));
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
            };
            let result = run(resolve_args, &context, &mut output);
            Run {
                result,
                stdout: stdout.text(),
                stderr: stderr.text(),
            }
        }
    }

    #[test]
    fn resolve_lists_rust_symbols_in_file_order() {
        let project = Project::new();

        let run = project.resolve(&["resolve", "src/lib.rs"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(
            run.stdout,
            "src/lib.rs#Config\nsrc/lib.rs#Config.port\nsrc/lib.rs#Config.validate\nsrc/lib.rs#load\n"
        );
    }

    #[test]
    fn resolve_lists_markdown_headings_with_anchor_aliases() {
        let project = Project::new();
        project.write("docs/guide.md", "# News\n\n## What's new\n\ntext\n");

        let run = project.resolve(&["resolve", "docs/guide.md"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert!(
            run.stdout
                .contains("docs/guide.md#news.whats-new  (also #whats-new)\n"),
            "stdout: {}",
            run.stdout
        );
    }

    #[test]
    fn resolve_confirms_one_symbol_and_prints_it() {
        let project = Project::new();

        let run = project.resolve(&["resolve", "src/lib.rs", "Config.validate"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "src/lib.rs#Config.validate\n");
    }

    #[test]
    fn resolve_unknown_symbol_exits_1_with_nearest_in_help() {
        let project = Project::new();

        let run = project.resolve(&["resolve", "src/lib.rs", "Config.validat"]);

        assert_eq!(run.exit_code(), 1);
        assert!(run.stdout.is_empty(), "stdout: {}", run.stdout);
        let error = run.error();
        assert_eq!(error.code(), "resolve/not-found");
        assert!(
            error.help().contains("`Config.validate`"),
            "help: {}",
            error.help()
        );
    }

    #[test]
    fn resolve_ambiguous_symbol_exits_1_listing_full_paths() {
        let project = Project::new();
        project.write(
            "src/two.rs",
            "pub struct Foo;\npub struct Bar;\nimpl Foo {\n    pub fn new() {}\n}\nimpl Bar {\n    pub fn new() {}\n}\n",
        );

        let run = project.resolve(&["resolve", "src/two.rs", "new"]);

        assert_eq!(run.exit_code(), 1);
        let error = run.error();
        assert_eq!(error.code(), "resolve/ambiguous");
        assert!(
            error.help().contains("`Foo.new`") && error.help().contains("`Bar.new`"),
            "help: {}",
            error.help()
        );
    }

    #[test]
    fn resolve_namespaced_file_reads_through_the_namespace() {
        let project = Project::new();
        project.write("services/auth/login.rs", "pub fn login() {}\n");

        let run = project.resolve(&["resolve", "auth:login.rs", "login"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert_eq!(run.stdout, "services/auth/login.rs#login\n");
    }

    #[test]
    fn resolve_unknown_namespace_is_usage_error_exit_2() {
        let project = Project::new();

        let run = project.resolve(&["resolve", "autt:login.rs"]);

        assert_eq!(run.exit_code(), 2);
        let error = run.error();
        assert_eq!(error.code(), "link/unknown-namespace");
        assert!(error.help().contains("`auth`"), "help: {}", error.help());
    }

    #[test]
    fn resolve_missing_file_is_usage_error_exit_2() {
        let project = Project::new();

        let run = project.resolve(&["resolve", "src/nope.rs"]);

        assert_eq!(run.exit_code(), 2);
        assert_eq!(run.error().code(), "cli/path-missing");
    }

    #[test]
    fn resolve_unsupported_extension_exits_1() {
        let project = Project::new();
        project.write("logo.png", "not an image");

        let run = project.resolve(&["resolve", "logo.png"]);

        assert_eq!(run.exit_code(), 1);
        assert_eq!(run.error().code(), "read/unsupported-language");
    }

    #[test]
    fn resolve_caps_at_limit_and_signals_truncation() {
        let project = Project::new();

        let run = project.resolve(&["--json", "resolve", "src/lib.rs", "--limit", "2"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(at(&json, "/truncated"), true, "json: {json}");
        assert_eq!(
            at(&json, "/data/symbols").as_array().map(Vec::len),
            Some(2),
            "json: {json}"
        );

        let text = project.resolve(&["resolve", "src/lib.rs", "--limit", "2"]);
        assert!(
            text.stderr.contains("Showing 2 symbols"),
            "stderr: {}",
            text.stderr
        );
    }

    #[test]
    fn resolve_file_with_syntax_error_lists_and_warns() {
        let project = Project::new();
        project.write("src/broken.rs", "pub fn fine() {}\n\npub fn broken( {\n");

        let run = project.resolve(&["--json", "resolve", "src/broken.rs"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        let json = run.json();
        assert_eq!(
            at(&json, "/findings/0/kind"),
            "read/syntax-errors",
            "json: {json}"
        );
        assert_eq!(at(&json, "/findings/0/severity"), "warning", "json: {json}");
        assert!(
            at(&json, "/data/symbols")
                .as_array()
                .is_some_and(|symbols| !symbols.is_empty()),
            "json: {json}"
        );
    }

    #[test]
    fn resolve_file_without_symbols_says_to_link_the_whole_file() {
        let project = Project::new();
        project.write("src/empty.rs", "// nothing here\n");

        let run = project.resolve(&["resolve", "src/empty.rs"]);

        assert_eq!(run.exit_code(), 0, "stderr: {}", run.stderr);
        assert!(run.stdout.is_empty(), "stdout: {}", run.stdout);
        assert_eq!(
            run.stderr,
            "src/empty.rs has no symbols; link to the whole file instead.\n"
        );
    }

    #[test]
    fn resolve_json_validates_against_its_schema() {
        let project = Project::new();
        let json = project.resolve(&["--json", "resolve", "src/lib.rs"]).json();

        let schema = serde_json::to_value(schema().expect("resolve is built, so it has a schema"))
            .expect("the resolve schema should serialize");
        let validator = jsonschema::validator_for(&schema)
            .expect("the generated schema should be a valid JSON Schema");
        let errors: Vec<String> = validator
            .iter_errors(&json)
            .map(|error| error.to_string())
            .collect();

        assert!(errors.is_empty(), "schema violations: {errors:?} in {json}");
        assert_eq!(at(&json, "/data/file"), "src/lib.rs", "json: {json}");
        assert_eq!(at(&json, "/command"), "resolve", "json: {json}");
    }
}
