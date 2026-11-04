//! rumdl: its JSON diagnostics, and the project config supdoc finds for it.
//!
//! rumdl finds its config from the directory it runs in, never from the block's virtual path,
//! and supdoc runs it from an empty directory of its own. So supdoc walks up from the doc inside
//! the root, and refuses a config that could make rumdl run a command or read a file the result
//! cache doesn't key on: a `code-block-tools` section, a `command`, or an `extends`. A config it
//! accepts isn't passed as the project wrote it: supdoc writes the table it validated again into
//! a private file and passes that by `--config`. With no config found, rumdl gets `--no-config`,
//! so a config in the home directory never applies.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::format::ParseError;
use super::lint::{self, Diagnostic};
use super::runner::{self, Failure, Ran};
use super::tools::ToolEnv;
use crate::hash::Hash;

/// The config names rumdl looks for in a directory, in its own order. `pyproject.toml` counts
/// only when it has a `[tool.rumdl]` table.
pub(crate) const CONFIG_FILES: [&str; 4] = [
    ".rumdl.toml",
    "rumdl.toml",
    ".config/rumdl.toml",
    "pyproject.toml",
];

/// The most bytes of a config supdoc reads.
const MAX_CONFIG: u64 = 256 * 1024;

/// The keys that make rumdl run a command or read another config, as the words rumdl spells them
/// with (`-` or `_` between), and what each does.
const REFUSED_KEYS: [(&[&str], &str); 3] = [
    (
        &["code", "block", "tools"],
        "declares `code-block-tools`, which run commands",
    ),
    (&["command"], "declares a `command`, which rumdl would run"),
    (
        &["extends"],
        "extends another config, which supdoc doesn't follow",
    ),
];

/// One entry of `rumdl check --output-format json`'s array.
#[derive(serde::Deserialize)]
struct Entry {
    /// Its 1-based line.
    line: usize,
    /// Its 1-based column.
    column: usize,
    /// The rule's code, such as `MD013`.
    rule: String,
    /// What rumdl says.
    message: String,
}

/// The diagnostics in `rumdl check --output-format json`'s `stdout`. `None` when it isn't that
/// JSON array.
#[must_use]
pub(crate) fn diagnostics(stdout: &[u8]) -> Option<Vec<Diagnostic>> {
    let entries: Vec<Entry> = serde_json::from_slice(stdout).ok()?;
    Some(
        entries
            .into_iter()
            .map(|entry| Diagnostic {
                line: entry.line,
                column: entry.column,
                rule: entry.rule,
                message: entry.message,
            })
            .collect(),
    )
}

/// Judges a `rumdl check` run as [`lint::judge`] does, reading its stdout as rumdl's JSON array.
#[must_use]
pub(crate) fn lint_verdict(_input: &str, result: Result<Ran, Failure>) -> lint::Verdict {
    lint::judge(result, diagnostics)
}

/// `rumdl fmt`'s syntax-error report: none, since every text is Markdown.
#[must_use]
pub(crate) fn parse_error(_stderr: &str) -> Option<ParseError> {
    None
}

/// Why a block isn't given to rumdl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The project's rumdl config, at `file` (relative to the root), can't be given to rumdl.
    Config {
        /// The config's path relative to the root.
        file: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// supdoc has no private directory to keep rumdl's config in.
    NoPrivateDir,
}

/// What rumdl is given for the blocks of one doc directory: its flags, and where the project's
/// config now lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Project {
    /// `--config=<private file>`, or `--no-config` when the project has no config.
    pub(crate) flags: Vec<OsString>,
    /// The directory the project's config was in, and the private directory holding its copy:
    /// `None` when there is no config.
    moved: Option<(PathBuf, PathBuf)>,
}

impl Project {
    /// Where rumdl is told `doc` is: its position under the config's directory, kept under the
    /// private copy's directory, so patterns the config writes relative to its own directory,
    /// such as `[per-file-ignores]`, still match. `doc` itself when there is no config. The
    /// file need not exist.
    #[must_use]
    pub(crate) fn doc_as_seen(&self, doc: &Path) -> PathBuf {
        match &self.moved {
            Some((from, to)) => doc
                .strip_prefix(from)
                .map_or_else(|_| doc.to_path_buf(), |relative| to.join(relative)),
            None => doc.to_path_buf(),
        }
    }
}

/// The [`Project`] rumdl is given for `doc`: the project's config, found walking up from `doc`'s
/// directory to `root`, or `--no-config` when there is none.
///
/// The config is read and checked, and then not passed as the project wrote it: its validated
/// table is written again as TOML into a private directory named by the hash of its content,
/// which is what rumdl reads.
///
/// Effect: reads file metadata and the config, resolves symlinks, and may create supdoc's
/// private directory and the config file in it.
///
/// # Errors
///
/// [`Refusal::Config`] when the config found is a link out of the root, isn't a readable TOML
/// file of at most 256 KiB, extends anything, declares commands, reads `.editorconfig`, or is a
/// `pyproject.toml` whose `[tool.rumdl]` isn't a table; [`Refusal::NoPrivateDir`] when there is
/// nowhere private to put it. A `pyproject.toml` supdoc can't read as TOML, or that has no
/// `[tool.rumdl]`, is skipped.
pub(crate) fn project_args(env: &ToolEnv, root: &Path, doc: &Path) -> Result<Project, Refusal> {
    for candidate in candidates(root, doc) {
        if let Some(table) = checked(root, &candidate)? {
            let file = relative(root, &candidate);
            let private = private_config(env, root, &table).map_err(|why| match why {
                None => Refusal::NoPrivateDir,
                Some(why) => Refusal::Config { file, why },
            })?;
            let mut flag = OsString::from("--config=");
            flag.push(private.join("rumdl.toml"));
            return Ok(Project {
                flags: vec![flag],
                moved: candidate.parent().map(|dir| (dir.to_path_buf(), private)),
            });
        }
    }
    Ok(Project {
        flags: vec![OsString::from("--no-config")],
        moved: None,
    })
}

/// `path` relative to `root`, as shown to the user.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The config files in `doc`'s directory and the ancestors of it inside `root`, nearest first.
fn candidates(root: &Path, doc: &Path) -> impl Iterator<Item = PathBuf> {
    let dirs = doc
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(|dir| dir.starts_with(root))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    dirs.into_iter()
        .flat_map(|dir| CONFIG_FILES.map(|name| dir.join(name)))
        .filter(|candidate| fs::symlink_metadata(candidate).is_ok())
}

/// The validated rumdl table of the config at `found`; `None` for a `pyproject.toml` that supdoc
/// can't read as TOML or that has no `[tool.rumdl]` table, which supdoc doesn't count as a config.
///
/// # Errors
///
/// [`Refusal::Config`] naming what is wrong with it.
fn checked(root: &Path, found: &Path) -> Result<Option<toml::Table>, Refusal> {
    let file = relative(root, found);
    let refuse = |why| Refusal::Config {
        file: file.clone(),
        why,
    };
    let is_pyproject = found
        .file_name()
        .is_some_and(|name| name == "pyproject.toml");
    let read = read_table(root, found);
    let table = match read {
        Ok(table) => table,
        Err(_) if is_pyproject => return Ok(None),
        Err(why) => return Err(refuse(why)),
    };
    let rumdl = if is_pyproject {
        match table
            .get("tool")
            .and_then(toml::Value::as_table)
            .and_then(|tool| tool.get("rumdl"))
        {
            Some(toml::Value::Table(rumdl)) => rumdl.clone(),
            Some(_) => return Err(refuse("has a `[tool.rumdl]` that isn't a table")),
            None => return Ok(None),
        }
    } else {
        table
    };
    match unsafe_part(&rumdl) {
        Some(why) => Err(refuse(why)),
        None => Ok(Some(rumdl)),
    }
}

/// The TOML table in the file at `found`, which must resolve inside `root`.
///
/// # Errors
///
/// What is wrong with the file.
fn read_table(root: &Path, found: &Path) -> Result<toml::Table, &'static str> {
    let real_root = root.canonicalize().map_err(|_| "couldn't be resolved")?;
    let real = found.canonicalize().map_err(|_| "couldn't be resolved")?;
    if !real.starts_with(&real_root) {
        return Err("is a link that resolves outside the project");
    }
    let meta = fs::metadata(&real).map_err(|_| "couldn't be read")?;
    if !meta.is_file() || meta.len() > MAX_CONFIG {
        return Err("isn't a regular file of at most 256 KiB");
    }
    let text = fs::read_to_string(&real).map_err(|_| "couldn't be read as UTF-8")?;
    text.parse::<toml::Table>()
        .map_err(|_| "doesn't parse as TOML")
}

/// Writes `table` as TOML into `rumdl.toml` in a private directory named by the hash of what is
/// written, under supdoc's `rumdl-config` directory, and returns that directory. It holds an
/// empty `.git` directory too, which makes it the project root rumdl resolves a config's
/// relative patterns against, wherever supdoc's cache directory is. A file already there with
/// the same content is kept; anything else at the name is replaced.
///
/// Effect: creates the directories mode 0700 and the file mode 0600, and may replace a stale file.
///
/// # Errors
///
/// `None` when there is no private directory; `Some(why)` when the table can't be written.
fn private_config(
    env: &ToolEnv,
    root: &Path,
    table: &toml::Table,
) -> Result<PathBuf, Option<&'static str>> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let neutral = runner::neutral_dir(env, root);
    if neutral == Path::new("/") {
        return Err(None);
    }
    let text = toml::to_string(table).map_err(|_| Some("couldn't be written again as TOML"))?;
    let parent = neutral.parent().ok_or(None)?.join("rumdl-config");
    let dir = parent.join(Hash::digest(text.as_bytes()).to_string());
    let private = |path: &Path| {
        fs::symlink_metadata(path)
            .is_ok_and(|meta| meta.is_dir() && meta.permissions().mode().trailing_zeros() >= 6)
    };
    for path in [&parent, &dir, &dir.join(".git")] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| None)?;
        if !private(path) {
            return Err(None);
        }
    }
    let file = dir.join("rumdl.toml");
    let current = fs::symlink_metadata(&file)
        .ok()
        .filter(fs::Metadata::is_file)
        .and_then(|_| fs::read_to_string(&file).ok());
    if current.as_deref() != Some(text.as_str()) {
        let _stale = fs::remove_file(&file);
        let mut created = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .map_err(|_| None)?;
        created.write_all(text.as_bytes()).map_err(|_| None)?;
    }
    Ok(dir)
}

/// What in the config `table` rumdl mustn't be given, `None` when nothing. Keys are compared as
/// rumdl reads them: lowercase, `_` and `-` alike.
fn unsafe_part(table: &toml::Table) -> Option<&'static str> {
    table.iter().find_map(|(key, value)| {
        let key = key.to_ascii_lowercase().replace('_', "-");
        let refused = REFUSED_KEYS
            .iter()
            .find(|(words, _)| key.split('-').eq(words.iter().copied()))
            .map(|(_, why)| *why);
        let reads_editorconfig = key == "editorconfig" && *value != toml::Value::Boolean(false);
        if reads_editorconfig {
            return Some("sets `editorconfig`, which makes rumdl read `.editorconfig` files");
        }
        refused.or_else(|| nested(value))
    })
}

/// [`unsafe_part`] through the tables and arrays of tables inside `value`.
fn nested(value: &toml::Value) -> Option<&'static str> {
    match value {
        toml::Value::Table(table) => unsafe_part(table),
        toml::Value::Array(items) => items.iter().find_map(nested),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Project, Refusal, diagnostics, project_args, unsafe_part};
    use crate::code_blocks::lint::Diagnostic;
    use crate::code_blocks::tools::ToolEnv;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn refusal_of(config: &str) -> Option<&'static str> {
        unsafe_part(&config.parse().expect("the fixture config should be TOML"))
    }

    #[test]
    fn rumdls_json_gives_each_diagnostics_rule_line_and_column() {
        let stdout = br#"[{"file":"/p/a.md.1.md","line":3,"column":21,"rule":"MD013","message":"Line length 49 exceeds 20 characters","severity":"warning","fixable":false}]"#;

        assert_eq!(
            diagnostics(stdout),
            Some(vec![Diagnostic {
                line: 3,
                column: 21,
                rule: "MD013".to_owned(),
                message: "Line length 49 exceeds 20 characters".to_owned(),
            }])
        );
    }

    #[test]
    fn output_that_isnt_rumdls_json_array_has_no_diagnostics() {
        assert_eq!(diagnostics(b"error: boom\n"), None);
        assert_eq!(diagnostics(b"[]"), Some(Vec::new()));
    }

    #[test]
    fn code_block_tools_commands_and_extends_are_refused_at_any_depth() {
        assert!(refusal_of("[code-block-tools]\nenabled = true\n").is_some());
        assert!(refusal_of("[code_block_tools.languages]\nrust = \"x\"\n").is_some());
        assert!(refusal_of("[global]\nextends = \"../base.toml\"\n").is_some());
        assert!(refusal_of("[[tools]]\ncommand = [\"sh\"]\n").is_some());
        assert!(refusal_of("[global]\neditorconfig = true\n").is_some());
        assert_eq!(refusal_of("[global]\neditorconfig = false\n"), None);
        assert_eq!(
            refusal_of("[global]\nline-length = 20\n[MD013]\nenabled = false\n"),
            None
        );
    }

    /// A `ToolEnv` whose cache home is `cache`.
    fn env_in(cache: &Path) -> ToolEnv {
        ToolEnv::from_vars(|name| (name == "XDG_CACHE_HOME").then(|| cache.as_os_str().to_owned()))
    }

    /// The path in the one `--config=<path>` flag of `project`.
    fn config_path(project: &Project) -> PathBuf {
        let [flag] = project.flags.as_slice() else {
            panic!("expected one flag, got {:?}", project.flags);
        };
        let flag = flag.to_string_lossy();
        PathBuf::from(
            flag.strip_prefix("--config=")
                .expect("the flag should be --config=<path>"),
        )
    }

    #[test]
    fn the_nearest_config_inside_the_root_is_passed_as_a_private_copy_of_its_validated_table() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::create_dir_all(root.path().join("docs/deep")).expect("the fixture dirs");
        fs::write(root.path().join(".rumdl.toml"), "extends = \"x.toml\"\n").expect("root config");
        fs::write(
            root.path().join("docs/rumdl.toml"),
            "# a comment\n[global]\nline-length = 20\n",
        )
        .expect("docs config");
        let env = env_in(cache.path());
        let near = root.path().join("docs/deep/a.md");
        let far = root.path().join("a.md");

        let passed = project_args(&env, root.path(), &near).expect("the docs config is safe");
        let refused = project_args(&env, root.path(), &far);

        let path = config_path(&passed);
        assert!(
            !path.starts_with(root.path()),
            "{path:?} is inside the root"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("the private file should be readable"),
            "[global]\nline-length = 20\n"
        );
        assert!(
            matches!(refused, Err(Refusal::Config { ref file, .. }) if file == ".rumdl.toml"),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_pyproject_counts_only_with_a_readable_tool_rumdl_table() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let env = env_in(cache.path());
        let doc = root.path().join("a.md");
        let pyproject = root.path().join("pyproject.toml");
        let none = |project: Result<Project, Refusal>| {
            project.expect("no config is not a refusal").flags == [OsString::from("--no-config")]
        };

        fs::write(&pyproject, "[tool.ruff]\nx = 1\n").expect("pyproject");
        let without = project_args(&env, root.path(), &doc);
        fs::write(&pyproject, "not = [toml").expect("pyproject");
        let unparseable = project_args(&env, root.path(), &doc);
        fs::write(&pyproject, format!("#{}\n", "x".repeat(300 * 1024))).expect("pyproject");
        let oversized = project_args(&env, root.path(), &doc);
        fs::write(&pyproject, "[tool]\nrumdl = 1\n").expect("pyproject");
        let not_a_table = project_args(&env, root.path(), &doc);
        fs::write(&pyproject, "[tool.rumdl.global]\nline-length = 20\n").expect("pyproject");
        let with = project_args(&env, root.path(), &doc).expect("a [tool.rumdl] is a config");

        assert!(none(without));
        assert!(none(unparseable));
        assert!(none(oversized));
        assert!(
            matches!(not_a_table, Err(Refusal::Config { .. })),
            "got {not_a_table:?}"
        );
        assert_eq!(
            fs::read_to_string(config_path(&with)).expect("the private file should be readable"),
            "[global]\nline-length = 20\n"
        );
    }

    #[test]
    fn a_config_that_is_a_link_out_of_the_root_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let outside = tempfile::TempDir::new().expect("a temp dir should be creatable");
        let cache = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(outside.path().join("r.toml"), "[global]\n").expect("outside config");
        std::os::unix::fs::symlink(
            outside.path().join("r.toml"),
            root.path().join(".rumdl.toml"),
        )
        .expect("the link");

        let refused = project_args(
            &env_in(cache.path()),
            root.path(),
            &root.path().join("a.md"),
        );

        assert!(
            matches!(refused, Err(Refusal::Config { .. })),
            "got {refused:?}"
        );
    }

    #[test]
    fn with_a_config_and_no_private_dir_the_block_is_refused() {
        let root = tempfile::TempDir::new().expect("a temp dir should be creatable");
        fs::write(root.path().join(".rumdl.toml"), "[global]\n").expect("config");

        let refused = project_args(&ToolEnv::default(), root.path(), &root.path().join("a.md"));

        assert_eq!(refused, Err(Refusal::NoPrivateDir));
    }
}
